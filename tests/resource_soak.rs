//! Opt-in process resource measurements under repeated lease churn.

use std::env;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, TcpListener, TcpStream};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};
use socket2::SockRef;

#[derive(Clone, Copy, Debug)]
struct Resources {
    rss_kib: Option<u64>,
    descriptors: Option<u64>,
    threads: Option<u64>,
}

impl Resources {
    fn sample() -> Self {
        Self {
            rss_kib: rss_kib(),
            descriptors: descriptor_count(),
            threads: thread_count(),
        }
    }
}

#[test]
#[ignore = "resource soak is opt-in; run scripts/measure-resources.sh"]
fn identity_churn_has_bounded_process_resources() {
    let runs_per_batch = env_number("SANDBOX_EGRESS_SOAK_RUNS", 2_000);
    let batches = env_number("SANDBOX_EGRESS_SOAK_BATCHES", 4);
    assert!(runs_per_batch > 0 && batches > 0);
    assert!(runs_per_batch.saturating_mul(batches) < 0x00ff_ffff);

    let process_start = Resources::sample();
    let proxy =
        Proxy::start(ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO))
            .expect("start proxy");
    thread::sleep(Duration::from_millis(25));
    let proxy_start = Resources::sample();
    let started = Instant::now();

    eprintln!(
        "resource_soak event=start runs_per_batch={runs_per_batch} batches={batches} process_rss_kib={:?} process_fds={:?} process_threads={:?} proxy_rss_kib={:?} proxy_fds={:?} proxy_threads={:?}",
        process_start.rss_kib,
        process_start.descriptors,
        process_start.threads,
        proxy_start.rss_kib,
        proxy_start.descriptors,
        proxy_start.threads,
    );

    for batch in 0..batches {
        for offset in 0..runs_per_batch {
            let sequence = batch.saturating_mul(runs_per_batch) + offset + 1;
            let identity = PeerIdentity::SourceIp(churn_address(sequence));
            let lease = proxy
                .attach(identity, Policy::builder().build().expect("valid policy"))
                .expect("attach churn lease");
            lease
                .close(Instant::now() + Duration::from_secs(2))
                .expect("close churn lease");
        }

        // Release commands are asynchronous with respect to close returning.
        thread::sleep(Duration::from_millis(25));
        let current = Resources::sample();
        eprintln!(
            "resource_soak event=batch batch={} completed={} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
            batch + 1,
            (batch + 1).saturating_mul(runs_per_batch),
            started.elapsed().as_millis(),
            current.rss_kib,
            current.descriptors,
            current.threads,
        );
        assert_stable_non_memory_resources(proxy_start, current);
    }

    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
    thread::sleep(Duration::from_millis(25));
    let finished = Resources::sample();
    eprintln!(
        "resource_soak event=finish completed={} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        runs_per_batch.saturating_mul(batches),
        started.elapsed().as_millis(),
        finished.rss_kib,
        finished.descriptors,
        finished.threads,
    );
    assert_stable_non_memory_resources(process_start, finished);
}

#[test]
#[ignore = "resource soak is opt-in; run scripts/measure-resources.sh"]
fn concurrent_management_churn_releases_process_resources() {
    let concurrency = env_number("SANDBOX_EGRESS_CONTROL_CONCURRENCY", 64);
    let batches = env_number("SANDBOX_EGRESS_CONTROL_BATCHES", 4);
    assert!(concurrency > 0 && batches > 0);
    assert!(concurrency.saturating_mul(batches) < 0x00ff_ffff);

    let process_start = Resources::sample();
    let proxy = Arc::new(
        Proxy::start(ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO))
            .expect("start proxy"),
    );
    thread::sleep(Duration::from_millis(25));
    let proxy_start = Resources::sample();
    let started = Instant::now();

    eprintln!(
        "control_soak event=start concurrency={concurrency} batches={batches} process_rss_kib={:?} process_fds={:?} process_threads={:?} proxy_rss_kib={:?} proxy_fds={:?} proxy_threads={:?}",
        process_start.rss_kib,
        process_start.descriptors,
        process_start.threads,
        proxy_start.rss_kib,
        proxy_start.descriptors,
        proxy_start.threads,
    );

    for batch in 0..batches {
        let attach_barrier = Arc::new(Barrier::new(concurrency));
        let attached_barrier = Arc::new(Barrier::new(concurrency + 1));
        let close_barrier = Arc::new(Barrier::new(concurrency + 1));
        let mut callers = Vec::with_capacity(concurrency);
        for offset in 0..concurrency {
            let proxy = Arc::clone(&proxy);
            let attach_barrier = Arc::clone(&attach_barrier);
            let attached_barrier = Arc::clone(&attached_barrier);
            let close_barrier = Arc::clone(&close_barrier);
            let sequence = batch.saturating_mul(concurrency) + offset + 1;
            callers.push(thread::spawn(move || {
                attach_barrier.wait();
                let lease = proxy
                    .attach(
                        PeerIdentity::SourceIp(churn_address(sequence)),
                        Policy::builder().build().expect("valid policy"),
                    )
                    .expect("attach contended lease");
                attached_barrier.wait();
                close_barrier.wait();
                lease
                    .close(Instant::now() + Duration::from_secs(2))
                    .expect("close contended lease");
            }));
        }

        attached_barrier.wait();
        let peak = Resources::sample();
        eprintln!(
            "control_soak event=peak batch={} attached={} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
            batch + 1,
            concurrency,
            started.elapsed().as_millis(),
            peak.rss_kib,
            peak.descriptors,
            peak.threads,
        );
        close_barrier.wait();
        for caller in callers {
            caller.join().expect("management caller");
        }

        thread::sleep(Duration::from_millis(25));
        let current = Resources::sample();
        eprintln!(
            "control_soak event=batch batch={} completed={} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
            batch + 1,
            (batch + 1).saturating_mul(concurrency),
            started.elapsed().as_millis(),
            current.rss_kib,
            current.descriptors,
            current.threads,
        );
        assert_stable_non_memory_resources(proxy_start, current);
    }

    Arc::into_inner(proxy)
        .expect("all proxy references returned")
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
    thread::sleep(Duration::from_millis(25));
    let finished = Resources::sample();
    eprintln!(
        "control_soak event=finish completed={} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        concurrency.saturating_mul(batches),
        started.elapsed().as_millis(),
        finished.rss_kib,
        finished.descriptors,
        finished.threads,
    );
    assert_stable_non_memory_resources(process_start, finished);
}

#[test]
#[ignore = "resource soak is opt-in; run scripts/measure-resources.sh"]
fn concurrent_idle_expiry_releases_process_resources() {
    let connections = env_number("SANDBOX_EGRESS_IDLE_CONNECTIONS", 128);
    assert!(connections > 0 && connections < 0x00ff_ffff);

    let process_start = Resources::sample();
    let (port, accepted_rx, upstream) = start_idle_upstream(connections);
    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_max_connections(connections)
            .with_identity_reuse_quiet_period(Duration::ZERO),
    )
    .expect("start idle proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder()
                .allow_network("127.0.0.0/8".parse::<IpNet>().expect("loopback CIDR"))
                .allow_port(port)
                .max_connections(connections)
                .expect("positive idle connection limit")
                .idle_timeout(Duration::from_secs(2))
                .build()
                .expect("valid idle soak policy"),
        )
        .expect("attach idle soak lease");
    thread::sleep(Duration::from_millis(25));
    let idle_start = Resources::sample();
    let started = Instant::now();
    let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    let mut clients = Vec::with_capacity(connections);
    for _ in 0..connections {
        clients.push(open_soak_tunnel(
            lease.endpoint().socket_addr(),
            request.as_bytes(),
        ));
    }
    accepted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("all idle tunnels reached upstream");
    assert_eq!(lease.usage().active_connections, connections as u64);
    let peak = Resources::sample();
    eprintln!(
        "idle_soak event=peak connections={connections} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        started.elapsed().as_millis(),
        peak.rss_kib,
        peak.descriptors,
        peak.threads,
    );

    for mut client in clients {
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("bound client idle read");
        assert_terminal_socket(client.read(&mut [0_u8; 1]));
    }
    upstream.join().expect("idle upstream thread");
    wait_for_no_active_connections(&lease);
    let recovered = Resources::sample();
    eprintln!(
        "idle_soak event=recovered connections={connections} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        started.elapsed().as_millis(),
        recovered.rss_kib,
        recovered.descriptors,
        recovered.threads,
    );
    assert_stable_non_memory_resources(idle_start, recovered);

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("close idle-soak lease")
        .usage();
    assert_eq!(final_usage.accepted_connections, connections as u64);
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.completed_connections, 0);
    assert_eq!(final_usage.denied_connections, connections as u64);
    assert_eq!(final_usage.uploaded_bytes, 0);
    assert_eq!(final_usage.downloaded_bytes, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
    thread::sleep(Duration::from_millis(25));
    let finished = Resources::sample();
    eprintln!(
        "idle_soak event=finish connections={connections} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        started.elapsed().as_millis(),
        finished.rss_kib,
        finished.descriptors,
        finished.threads,
    );
    assert_stable_non_memory_resources(process_start, finished);
}

fn start_idle_upstream(
    connections: usize,
) -> (u16, std::sync::mpsc::Receiver<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind idle upstream");
    let port = listener.local_addr().expect("idle upstream address").port();
    let (accepted_tx, accepted_rx) = std::sync::mpsc::sync_channel(1);
    let upstream = thread::spawn(move || {
        let mut streams = Vec::with_capacity(connections);
        for _ in 0..connections {
            let (stream, _) = listener.accept().expect("accept idle tunnel");
            streams.push(stream);
        }
        accepted_tx.send(()).expect("report idle accepts");
        for mut stream in streams {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("bound upstream idle read");
            assert_terminal_socket(stream.read(&mut [0_u8; 1]));
        }
    });
    (port, accepted_rx, upstream)
}

#[test]
#[ignore = "resource soak is opt-in; run scripts/measure-resources.sh"]
fn concurrent_partial_client_hellos_release_process_resources() {
    let connections = env_number("SANDBOX_EGRESS_TLS_CONNECTIONS", 64);
    assert!(connections > 0 && connections < 0x00ff_ffff);

    let process_start = Resources::sample();
    let (port, accepted_rx, upstream) = start_idle_upstream(connections);
    let (proxy, lease) = start_tls_buffer_lease(connections, port);
    thread::sleep(Duration::from_millis(25));
    let tls_start = Resources::sample();
    let started = Instant::now();
    eprintln!(
        "tls_soak event=start connections={connections} process_rss_kib={:?} process_fds={:?} process_threads={:?} proxy_rss_kib={:?} proxy_fds={:?} proxy_threads={:?}",
        process_start.rss_kib,
        process_start.descriptors,
        process_start.threads,
        tls_start.rss_kib,
        tls_start.descriptors,
        tls_start.threads,
    );
    let request = format!("CONNECT localhost:{port} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let partial_hello = partial_large_client_hello();
    let expected_upload = u64::try_from(partial_hello.len())
        .expect("bounded partial hello")
        .checked_mul(u64::try_from(connections).expect("bounded connection count"))
        .expect("bounded aggregate upload");
    let mut clients = Vec::with_capacity(connections);
    for _ in 0..connections {
        let mut client = open_soak_tunnel(lease.endpoint().socket_addr(), request.as_bytes());
        client
            .write_all(&partial_hello)
            .expect("write partial ClientHello");
        clients.push(client);
    }
    accepted_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("all TLS-buffer tunnels reached upstream");
    let observed_deadline = Instant::now() + Duration::from_secs(5);
    while lease.usage().uploaded_bytes != expected_upload {
        assert!(
            Instant::now() < observed_deadline,
            "partial ClientHello accounting did not reach the expected barrier"
        );
        thread::yield_now();
    }
    assert_eq!(lease.usage().active_connections, connections as u64);
    let peak = Resources::sample();
    eprintln!(
        "tls_soak event=peak connections={connections} hello_bytes={} aggregate_bytes={expected_upload} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        partial_hello.len(),
        started.elapsed().as_millis(),
        peak.rss_kib,
        peak.descriptors,
        peak.threads,
    );

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("close TLS-buffer lease")
        .usage();
    for mut client in clients {
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("bound TLS-buffer client read");
        assert_terminal_socket(client.read(&mut [0_u8; 1]));
    }
    upstream.join().expect("TLS-buffer upstream thread");
    assert_eq!(final_usage.accepted_connections, connections as u64);
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.completed_connections, 0);
    assert_eq!(final_usage.denied_connections, 0);
    assert_eq!(final_usage.uploaded_bytes, expected_upload);
    assert_eq!(final_usage.downloaded_bytes, 0);
    let recovered = Resources::sample();
    eprintln!(
        "tls_soak event=recovered connections={connections} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        started.elapsed().as_millis(),
        recovered.rss_kib,
        recovered.descriptors,
        recovered.threads,
    );
    assert_stable_non_memory_resources(tls_start, recovered);

    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
    thread::sleep(Duration::from_millis(25));
    let finished = Resources::sample();
    eprintln!(
        "tls_soak event=finish connections={connections} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        started.elapsed().as_millis(),
        finished.rss_kib,
        finished.descriptors,
        finished.threads,
    );
    assert_stable_non_memory_resources(process_start, finished);
}

fn start_tls_buffer_lease(connections: usize, port: u16) -> (Proxy, sandbox_egress::Lease) {
    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_max_connections(connections)
            .with_identity_reuse_quiet_period(Duration::ZERO),
    )
    .expect("start TLS-buffer proxy");
    let policy = Policy::builder()
        .allow_host("localhost")
        .expect("valid TLS-buffer hostname")
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("IPv4 loopback CIDR"))
        .allow_network("::1/128".parse::<IpNet>().expect("IPv6 loopback CIDR"))
        .allow_port(port)
        .max_connections(connections)
        .expect("positive TLS-buffer connection limit")
        .require_tls_sni()
        .handshake_timeout(Duration::from_secs(30))
        .build()
        .expect("valid TLS-buffer policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach TLS-buffer lease");
    (proxy, lease)
}

fn partial_large_client_hello() -> Vec<u8> {
    const BUFFERED_HANDSHAKE_BYTES: usize = 60_000;
    const TLS_RECORD_PAYLOAD_BYTES: usize = 16_384;

    // Declare the largest accepted handshake body, but deliberately supply
    // less. Rustls must retain all legal records while waiting for completion.
    let mut handshake = Vec::with_capacity(BUFFERED_HANDSHAKE_BYTES);
    handshake.extend_from_slice(&[1, 0, 0xff, 0xfb]);
    handshake.resize(BUFFERED_HANDSHAKE_BYTES, 0);
    let records = handshake.len().div_ceil(TLS_RECORD_PAYLOAD_BYTES);
    let mut wire = Vec::with_capacity(handshake.len() + records * 5);
    for payload in handshake.chunks(TLS_RECORD_PAYLOAD_BYTES) {
        wire.extend_from_slice(&[22, 3, 1]);
        wire.extend_from_slice(
            &u16::try_from(payload.len())
                .expect("bounded TLS record payload")
                .to_be_bytes(),
        );
        wire.extend_from_slice(payload);
    }
    wire
}

#[test]
#[ignore = "resource soak is opt-in; run scripts/measure-resources.sh"]
fn terminal_connection_churn_releases_process_resources() {
    let runs_per_batch = env_number("SANDBOX_EGRESS_SOAK_RUNS", 2_000);
    let batches = env_number("SANDBOX_EGRESS_SOAK_BATCHES", 4);
    assert!(runs_per_batch > 0 && batches > 0);
    let completed_tunnels = runs_per_batch.saturating_mul(batches);
    assert!(completed_tunnels < 0x00ff_ffff);

    let process_start = Resources::sample();
    let (port, reset_ready_rx, reset_release_tx, upstream) =
        start_terminal_upstream(completed_tunnels);
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder()
                .allow_network("127.0.0.0/8".parse::<IpNet>().expect("loopback CIDR"))
                .allow_port(port)
                .max_upload_bytes(1)
                .build()
                .expect("valid soak policy"),
        )
        .expect("attach soak lease");
    thread::sleep(Duration::from_millis(25));
    let active_start = Resources::sample();
    let started = Instant::now();
    let allowed_request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    let denied_request =
        format!("CONNECT denied.test:{port} HTTP/1.1\r\nHost: denied.test\r\n\r\n");

    eprintln!(
        "connection_soak event=start runs_per_batch={runs_per_batch} batches={batches} process_rss_kib={:?} process_fds={:?} process_threads={:?} active_rss_kib={:?} active_fds={:?} active_threads={:?}",
        process_start.rss_kib,
        process_start.descriptors,
        process_start.threads,
        active_start.rss_kib,
        active_start.descriptors,
        active_start.threads,
    );

    for batch in 0..batches {
        for _ in 0..runs_per_batch {
            complete_soak_tunnel(lease.endpoint().socket_addr(), allowed_request.as_bytes());
            limit_soak_tunnel(lease.endpoint().socket_addr(), allowed_request.as_bytes());
            reset_soak_tunnel(
                lease.endpoint().socket_addr(),
                allowed_request.as_bytes(),
                &reset_ready_rx,
                &reset_release_tx,
            );
            deny_soak_tunnel(lease.endpoint().socket_addr(), denied_request.as_bytes());
        }
        wait_for_no_active_connections(&lease);
        let current = Resources::sample();
        eprintln!(
            "connection_soak event=batch batch={} completed={} transfer_limited={} reset={} host_denied={} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
            batch + 1,
            (batch + 1).saturating_mul(runs_per_batch),
            (batch + 1).saturating_mul(runs_per_batch),
            (batch + 1).saturating_mul(runs_per_batch),
            (batch + 1).saturating_mul(runs_per_batch),
            started.elapsed().as_millis(),
            current.rss_kib,
            current.descriptors,
            current.threads,
        );
        assert_stable_non_memory_resources(active_start, current);
    }

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("close connection-soak lease")
        .usage();
    assert_eq!(final_usage.completed_connections, completed_tunnels as u64);
    assert_eq!(
        final_usage.denied_connections,
        completed_tunnels.saturating_mul(2) as u64
    );
    assert_eq!(
        final_usage.accepted_connections,
        completed_tunnels.saturating_mul(4) as u64
    );
    assert_eq!(
        final_usage.uploaded_bytes,
        completed_tunnels.saturating_mul(3) as u64
    );
    assert_eq!(final_usage.downloaded_bytes, completed_tunnels as u64);
    assert_eq!(final_usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
    upstream.join().expect("soak upstream thread");
    thread::sleep(Duration::from_millis(25));
    let finished = Resources::sample();
    eprintln!(
        "connection_soak event=finish completed={completed_tunnels} transfer_limited={completed_tunnels} reset={completed_tunnels} host_denied={completed_tunnels} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        started.elapsed().as_millis(),
        finished.rss_kib,
        finished.descriptors,
        finished.threads,
    );
    assert_stable_non_memory_resources(process_start, finished);
}

fn start_terminal_upstream(
    tunnels: usize,
) -> (
    u16,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind soak upstream");
    let port = listener.local_addr().expect("soak upstream address").port();
    let (reset_ready_tx, reset_ready_rx) = std::sync::mpsc::sync_channel(1);
    let (reset_release_tx, reset_release_rx) = std::sync::mpsc::sync_channel(1);
    let upstream = thread::spawn(move || {
        for _ in 0..tunnels {
            let (mut stream, _) = listener.accept().expect("accept soak tunnel");
            let mut marker = [0_u8; 1];
            stream.read_exact(&mut marker).expect("read soak marker");
            stream.write_all(&marker).expect("echo soak marker");
            drop(stream);

            let (mut stream, _) = listener.accept().expect("accept limited tunnel");
            let mut limited = Vec::new();
            stream
                .read_to_end(&mut limited)
                .expect("read limited upload");
            assert_eq!(limited, b"x");
            drop(stream);

            let (stream, _) = listener.accept().expect("accept reset tunnel");
            reset_ready_tx.send(()).expect("report reset accept");
            reset_release_rx.recv().expect("release upstream reset");
            SockRef::from(&stream)
                .set_linger(Some(Duration::ZERO))
                .expect("arm upstream reset");
            drop(stream);
        }
    });
    (port, reset_ready_rx, reset_release_tx, upstream)
}

fn complete_soak_tunnel(endpoint: std::net::SocketAddr, request: &[u8]) {
    let mut client = open_soak_tunnel(endpoint, request);
    client.write_all(b"x").expect("write soak marker");
    client
        .shutdown(Shutdown::Write)
        .expect("finish soak upload");
    let mut echoed = Vec::new();
    client.read_to_end(&mut echoed).expect("read soak echo");
    assert_eq!(echoed, b"x");
}

fn limit_soak_tunnel(endpoint: std::net::SocketAddr, request: &[u8]) {
    let mut client = open_soak_tunnel(endpoint, request);
    client.write_all(b"xy").expect("write limited marker");
    client
        .shutdown(Shutdown::Write)
        .expect("finish limited upload");
    client
        .read_to_end(&mut Vec::new())
        .expect("read limited tunnel closure");
}

fn reset_soak_tunnel(
    endpoint: std::net::SocketAddr,
    request: &[u8],
    ready: &std::sync::mpsc::Receiver<()>,
    release: &std::sync::mpsc::SyncSender<()>,
) {
    let mut client = open_soak_tunnel(endpoint, request);
    ready.recv().expect("observe reset accept");
    release.send(()).expect("release upstream reset");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set reset read timeout");
    match client.read(&mut [0_u8; 1]) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
            ) => {}
        result => panic!("expected terminal reset socket, got {result:?}"),
    }
}

fn open_soak_tunnel(endpoint: std::net::SocketAddr, request: &[u8]) -> TcpStream {
    let mut client = TcpStream::connect(endpoint).expect("connect soak proxy");
    client.write_all(request).expect("write soak CONNECT");
    let mut response = [0_u8; 39];
    client
        .read_exact(&mut response)
        .expect("read soak CONNECT response");
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    client
}

fn deny_soak_tunnel(endpoint: std::net::SocketAddr, request: &[u8]) {
    let mut client = TcpStream::connect(endpoint).expect("connect denial proxy");
    client.write_all(request).expect("write denied CONNECT");
    let mut response = String::new();
    client
        .read_to_string(&mut response)
        .expect("read soak denial");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("host-denied"), "{response}");
}

fn wait_for_no_active_connections(lease: &sandbox_egress::Lease) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while lease.usage().active_connections != 0 {
        assert!(
            Instant::now() < deadline,
            "connection work did not return to zero"
        );
        thread::yield_now();
    }
}

fn assert_terminal_socket(result: std::io::Result<usize>) {
    match result {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
            ) => {}
        result => panic!("expected terminal socket, got {result:?}"),
    }
}

fn env_number(name: &str, default: usize) -> usize {
    env::var(name).ok().map_or(default, |value| {
        value.parse().expect("numeric soak setting")
    })
}

fn churn_address(sequence: usize) -> IpAddr {
    let sequence = u32::try_from(sequence).expect("bounded churn sequence");
    let octets = sequence.to_be_bytes();
    IpAddr::V4(Ipv4Addr::new(10, octets[1], octets[2], octets[3]))
}

fn assert_stable_non_memory_resources(baseline: Resources, current: Resources) {
    if let (Some(baseline), Some(current)) = (baseline.descriptors, current.descriptors) {
        assert!(
            current <= baseline + 2,
            "descriptor growth: baseline={baseline}, current={current}"
        );
    }
    if let (Some(baseline), Some(current)) = (baseline.threads, current.threads) {
        assert!(
            current <= baseline + 2,
            "thread growth: baseline={baseline}, current={current}"
        );
    }
}

#[cfg(target_os = "linux")]
fn rss_kib() -> Option<u64> {
    proc_status_number("VmRSS:")
}

#[cfg(target_os = "macos")]
fn rss_kib() -> Option<u64> {
    command_number("ps", &["-o", "rss=", "-p", &std::process::id().to_string()])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rss_kib() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn descriptor_count() -> Option<u64> {
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(|entries| entries.count() as u64)
}

#[cfg(target_os = "macos")]
fn descriptor_count() -> Option<u64> {
    command_line_count("lsof", &["-p", &std::process::id().to_string()])
        .map(|lines| lines.saturating_sub(1))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn descriptor_count() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn thread_count() -> Option<u64> {
    proc_status_number("Threads:")
}

#[cfg(target_os = "macos")]
fn thread_count() -> Option<u64> {
    command_line_count("ps", &["-M", "-p", &std::process::id().to_string()])
        .map(|lines| lines.saturating_sub(1))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn thread_count() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn proc_status_number(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(target_os = "macos")]
fn command_number(program: &str, arguments: &[&str]) -> Option<u64> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output.status.success().then_some(())?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

#[cfg(target_os = "macos")]
fn command_line_count(program: &str, arguments: &[&str]) -> Option<u64> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output.status.success().then_some(())?;
    Some(String::from_utf8(output.stdout).ok()?.lines().count() as u64)
}
