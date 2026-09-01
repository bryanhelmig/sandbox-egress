//! Opt-in sustained local CONNECT load measurement.

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};
use socket2::SockRef;
use tokio::io::AsyncReadExt;
use tokio::runtime::Builder;
use tokio::task::JoinSet;

const CONNECT_RESPONSE: &[u8; 39] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

fn environment_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn percentile(sorted_samples: &[Duration], percentile: usize) -> Duration {
    sorted_samples[(sorted_samples.len() - 1) * percentile / 100]
}

fn start_upstreams(connections: usize, destinations: usize) -> (Vec<u16>, thread::JoinHandle<()>) {
    let mut ports = Vec::with_capacity(destinations);
    let mut listeners = Vec::with_capacity(destinations);
    for destination in 0..destinations {
        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
        ports.push(upstream.local_addr().expect("upstream address").port());
        let expected =
            connections / destinations + usize::from(destination < connections % destinations);
        upstream
            .set_nonblocking(true)
            .expect("make upstream nonblocking");
        listeners.push((upstream, expected));
    }
    let server = thread::spawn(move || {
        Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build upstream runtime")
            .block_on(async move {
                let mut acceptors = JoinSet::new();
                for (listener, expected) in listeners {
                    acceptors.spawn(async move {
                        let listener = tokio::net::TcpListener::from_std(listener)
                            .expect("create async upstream");
                        let mut connections = JoinSet::new();
                        for _ in 0..expected {
                            let (mut stream, _) =
                                listener.accept().await.expect("accept proxy dial");
                            connections.spawn(async move {
                                let mut buffer = [0_u8; 1];
                                stream
                                    .read_exact(&mut buffer)
                                    .await
                                    .expect("read tunnel teardown marker");
                                SockRef::from(&stream)
                                    .set_linger(Some(Duration::ZERO))
                                    .expect("reset completed upstream");
                            });
                        }
                        while let Some(connection) = connections.join_next().await {
                            connection.expect("upstream connection task");
                        }
                    });
                }
                while let Some(acceptor) = acceptors.join_next().await {
                    acceptor.expect("upstream acceptor task");
                }
            });
    });
    (ports, server)
}

fn connect_once(endpoint: std::net::SocketAddr, upstream_port: u16) -> Duration {
    let started = Instant::now();
    let mut client = TcpStream::connect(endpoint).expect("connect proxy");
    SockRef::from(&client)
        .set_linger(Some(Duration::ZERO))
        .expect("set reset-on-failure");
    client
        .write_all(
            format!("CONNECT 127.0.0.1:{upstream_port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .as_bytes(),
        )
        .expect("write CONNECT");
    let mut response = [0_u8; 39];
    client
        .read_exact(&mut response)
        .expect("read CONNECT response");
    assert_eq!(&response, CONNECT_RESPONSE);
    let setup = started.elapsed();

    client.write_all(b"x").expect("send tunnel teardown marker");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set teardown timeout");
    let mut byte = [0_u8; 1];
    match client.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == ErrorKind::ConnectionReset => {}
        result => panic!("expected terminal tunnel teardown, got {result:?}"),
    }
    setup
}

#[test]
#[ignore = "load measurement is opt-in; run scripts/measure-load.sh"]
fn concurrent_local_connect_capacity() {
    let connections = environment_usize("SANDBOX_EGRESS_LOAD_CONNECTIONS", 5_000);
    let concurrency = environment_usize("SANDBOX_EGRESS_LOAD_CONCURRENCY", 64).min(connections);
    let destinations = environment_usize("SANDBOX_EGRESS_LOAD_DESTINATIONS", 16).min(connections);
    let (upstream_ports, upstream_thread) = start_upstreams(connections, destinations);

    let proxy = Proxy::start(ProxyConfig::default().with_max_connections(concurrency * 2))
        .expect("start proxy");
    let mut policy_builder = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("loopback CIDR"))
        .max_connections(concurrency * 2)
        .expect("positive connection limit");
    for port in &upstream_ports {
        policy_builder = policy_builder.allow_port(*port);
    }
    let policy = policy_builder.build().expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");

    let endpoint = lease.endpoint().socket_addr();
    let upstream_ports = Arc::new(upstream_ports);
    let next = Arc::new(AtomicUsize::new(0));
    let start_barrier = Arc::new(Barrier::new(concurrency + 1));
    let (samples_tx, samples_rx) = mpsc::channel();
    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let next = Arc::clone(&next);
        let start_barrier = Arc::clone(&start_barrier);
        let samples_tx = samples_tx.clone();
        let upstream_ports = Arc::clone(&upstream_ports);
        workers.push(thread::spawn(move || {
            let mut samples = Vec::new();
            start_barrier.wait();
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= connections {
                    break;
                }
                let upstream_port = upstream_ports[index % upstream_ports.len()];
                samples.push(connect_once(endpoint, upstream_port));
            }
            samples_tx.send(samples).expect("send latency samples");
        }));
    }
    drop(samples_tx);

    let started = Instant::now();
    start_barrier.wait();
    for worker in workers {
        worker.join().expect("load worker");
    }
    let elapsed = started.elapsed();
    upstream_thread.join().expect("upstream thread");
    let mut samples: Vec<Duration> = samples_rx.into_iter().flatten().collect();
    samples.sort_unstable();
    assert_eq!(samples.len(), connections);

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(5))
        .expect("close measured lease")
        .usage();
    assert_eq!(
        final_usage.accepted_connections,
        u64::try_from(connections).expect("connection count fits u64")
    );
    assert_eq!(final_usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");

    let per_second = f64::from(u32::try_from(connections).expect("connection count fits u32"))
        / elapsed.as_secs_f64();
    eprintln!(
        "load connections={connections} concurrency={concurrency} destinations={destinations} elapsed_ms={} connections_per_second={per_second:.1} p50_us={} p95_us={} p99_us={}",
        elapsed.as_millis(),
        percentile(&samples, 50).as_micros(),
        percentile(&samples, 95).as_micros(),
        percentile(&samples, 99).as_micros(),
    );
}
