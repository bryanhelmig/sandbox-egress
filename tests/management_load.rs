//! Opt-in progress evidence while unrelated clients churn on the shared listener.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};

fn setting(name: &str, default: u64) -> u64 {
    let value =
        std::env::var(name).map_or(default, |value| value.parse().expect("numeric workload"));
    assert!(value > 0, "workload settings must be positive");
    value
}

#[derive(Default)]
struct Traffic {
    completed: AtomicU64,
    attempts: AtomicU64,
    connected: AtomicU64,
    connect_errors: AtomicU64,
    read_errors: AtomicU64,
    last_connect_errno: AtomicU64,
}

impl Traffic {
    fn snapshot(&self) -> [u64; 5] {
        [
            &self.completed,
            &self.attempts,
            &self.connected,
            &self.connect_errors,
            &self.read_errors,
        ]
        .map(|counter| counter.load(Ordering::Acquire))
    }
}

fn churn(endpoint: SocketAddr, stop: &AtomicBool, traffic: &Traffic) {
    while !stop.load(Ordering::Acquire) {
        traffic.attempts.fetch_add(1, Ordering::Relaxed);
        let mut stream = match TcpStream::connect_timeout(&endpoint, Duration::from_secs(1)) {
            Ok(stream) => stream,
            Err(error) => {
                traffic.connect_errors.fetch_add(1, Ordering::Relaxed);
                traffic.last_connect_errno.store(
                    u64::try_from(error.raw_os_error().unwrap_or(0)).unwrap_or(0),
                    Ordering::Relaxed,
                );
                continue;
            }
        };
        traffic.connected.fetch_add(1, Ordering::Relaxed);
        // Like the connection benchmarks, retire completed client sockets with
        // RST so local TCP teardown state cannot interrupt the offered churn.
        // We still require a terminal peer outcome before counting useful work.
        socket2::SockRef::from(&stream)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        // Known identities exercise parsing/denial, unknown ones exercise the
        // pre-admission refusal. Neither reaches DNS or the public network.
        let _ = stream.write_all(b"GET / HTTP/1.0\r\n\r\n");
        let mut response = [0; 256];
        let terminal = loop {
            match stream.read(&mut response) {
                Ok(0) => break true,
                Ok(_) => {}
                Err(error) => {
                    break matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::NotConnected
                    );
                }
            }
        };
        if terminal {
            traffic.completed.fetch_add(1, Ordering::Release);
        } else {
            traffic.read_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[test]
#[ignore = "progress workload is opt-in; see docs/factory-pressure.md"]
fn management_progress_during_attributed_and_unknown_connection_churn() {
    let workers = setting("SANDBOX_EGRESS_MANAGEMENT_WORKERS", 8);
    let cycles = setting("SANDBOX_EGRESS_MANAGEMENT_CYCLES", 32);
    let maximum = Duration::from_millis(setting("SANDBOX_EGRESS_MANAGEMENT_MAX_MS", 1_000));
    assert!(workers <= 128 && cycles <= 1_000, "bounded local workload");
    for attributed in [true, false] {
        let proxy = Arc::new(Proxy::start(ProxyConfig::default()).expect("proxy start"));
        let noisy = attributed.then(|| {
            proxy
                .attach(
                    PeerIdentity::SourceIp(Ipv4Addr::LOCALHOST.into()),
                    Policy::builder()
                        .max_connections(256)
                        .unwrap()
                        .build()
                        .unwrap(),
                )
                .unwrap()
        });
        let stop = Arc::new(AtomicBool::new(false));
        let exchanges = Arc::new(Traffic::default());
        let mut clients = Vec::new();
        for _ in 0..workers {
            let stop = Arc::clone(&stop);
            let exchanges = Arc::clone(&exchanges);
            let endpoint = proxy.endpoint().socket_addr();
            clients.push(thread::spawn(move || churn(endpoint, &stop, &exchanges)));
        }
        let (reply, received) = mpsc::sync_channel(1);
        let management_proxy = Arc::clone(&proxy);
        let progress = Arc::clone(&exchanges);
        let management = thread::spawn(move || {
            let warmup = Instant::now() + Duration::from_secs(5);
            while progress.completed.load(Ordering::Acquire) < workers {
                assert!(Instant::now() < warmup, "connection pressure never started");
                thread::sleep(Duration::from_millis(1));
            }
            let mut samples = Vec::new();
            for _ in 0..cycles {
                let before = progress.snapshot();
                let start = Instant::now();
                let lease = management_proxy
                    .attach(
                        PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::new(10, 255, 0, 1))),
                        Policy::builder().build().unwrap(),
                    )
                    .unwrap();
                let attach = start.elapsed();
                let start = Instant::now();
                let final_usage = lease
                    .close(start + maximum)
                    .expect("close under pressure")
                    .usage();
                let close = start.elapsed();
                assert_eq!(
                    final_usage,
                    sandbox_egress::Usage::default(),
                    "unrelated work crossed identities"
                );
                let after = progress.snapshot();
                let delta: [u64; 5] = std::array::from_fn(|i| after[i] - before[i]);
                eprintln!(
                    "management_sample attributed={attributed} cycle={} attach_us={} close_us={} before={before:?} delta_completed_attempts_connected_connect_errors_read_errors={delta:?} last_connect_errno={}",
                    samples.len(),
                    attach.as_micros(),
                    close.as_micros(),
                    progress.last_connect_errno.load(Ordering::Relaxed)
                );
                samples.push((attach, close, delta[0]));
            }
            let _ = reply.send(samples);
        });
        // attach has no public deadline. Observe it from a bounded caller;
        // stop the unrelated traffic before checking the result or joining.
        let result = received
            .recv_timeout(Duration::from_secs(5) + maximum * u32::try_from(cycles * 2).unwrap());
        stop.store(true, Ordering::Release);
        for client in clients {
            client.join().expect("churn client");
        }
        let samples = result.expect("management made no progress within workload deadline");
        management.join().expect("management worker");
        if let Some(noisy) = noisy {
            let usage = noisy
                .close(Instant::now() + Duration::from_secs(2))
                .unwrap()
                .usage();
            assert!(usage.accepted_connections > 0 && usage.denied_connections > 0);
            assert_eq!(usage.active_connections, 0);
        }
        Arc::into_inner(proxy)
            .expect("returned proxy owner")
            .shutdown(Instant::now() + Duration::from_secs(2))
            .unwrap();
        require_samples(
            &samples,
            attributed,
            workers,
            maximum,
            exchanges.completed.load(Ordering::Acquire),
        );
    }
}

fn require_samples(
    samples: &[(Duration, Duration, u64)],
    attributed: bool,
    workers: u64,
    maximum: Duration,
    total: u64,
) {
    let max_attach = samples.iter().map(|s| s.0).max().unwrap();
    let max_close = samples.iter().map(|s| s.1).max().unwrap();
    eprintln!(
        "management_load attributed={attributed} workers={workers} cycles={} quiet=default exchanges={} max_attach_us={} max_close_us={} limit_ms={}",
        samples.len(),
        total,
        max_attach.as_micros(),
        max_close.as_micros(),
        maximum.as_millis()
    );
    assert!(
        samples.iter().all(|s| s.2 > 0),
        "a sample had no competing traffic"
    );
    assert!(
        max_attach <= maximum && max_close <= maximum,
        "management exceeded explicit progress budget"
    );
}
