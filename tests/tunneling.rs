//! Tunnel byte-ceiling and bidirectional shutdown conformance tests.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};

const CONNECT_RESPONSE: &[u8; 39] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

fn assert_terminal_read(result: std::io::Result<usize>) {
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
        Ok(bytes) => panic!("expected terminal socket, read {bytes} bytes"),
        Err(error) => panic!("expected terminal socket, got {error:?}"),
    }
}

fn start_sender(payload: &'static [u8]) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind sender");
    let port = listener.local_addr().expect("sender address").port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept proxy");
        stream.write_all(payload).expect("send payload");
        stream.shutdown(Shutdown::Write).expect("finish payload");
    });
    (port, handle)
}

fn start_blocked_peer() -> (
    u16,
    mpsc::Receiver<()>,
    mpsc::Receiver<std::io::Result<usize>>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind blocked peer");
    let port = listener.local_addr().expect("blocked peer address").port();
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (closed_tx, closed_rx) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept proxy");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer read timeout");
        accepted_tx.send(()).expect("report accept");
        let result = stream.read(&mut [0_u8; 1]);
        closed_tx.send(result).expect("report peer closure");
    });
    (port, accepted_rx, closed_rx, handle)
}

fn start_nonreading_peer() -> (
    u16,
    mpsc::Receiver<()>,
    mpsc::SyncSender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind nonreader");
    let port = listener.local_addr().expect("nonreader address").port();
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let (_stream, _) = listener.accept().expect("accept proxy");
        accepted_tx.send(()).expect("report accept");
        release_rx.recv().expect("release nonreader");
    });
    (port, accepted_rx, release_tx, handle)
}

fn start_flooding_peer() -> (
    u16,
    mpsc::Receiver<()>,
    mpsc::Receiver<std::io::ErrorKind>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind flooding peer");
    let port = listener.local_addr().expect("flooding peer address").port();
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept proxy");
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("peer write timeout");
        accepted_tx.send(()).expect("report accept");
        let block = [0x5a_u8; 16 * 1024];
        let error = loop {
            if let Err(error) = stream.write_all(&block) {
                break error.kind();
            }
        };
        stopped_tx.send(error).expect("report writer stop");
    });
    (port, accepted_rx, stopped_rx, handle)
}

fn wait_for_bytes(mut current: impl FnMut() -> u64) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while current() == 0 {
        assert!(Instant::now() < deadline, "transfer never became active");
        thread::yield_now();
    }
}

fn attach_for_ports(
    proxy: &Proxy,
    ports: &[u16],
    max_download_bytes: Option<u64>,
) -> sandbox_egress::Lease {
    let mut policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("loopback test CIDR"));
    for port in ports {
        policy = policy.allow_port(*port);
    }
    if let Some(limit) = max_download_bytes {
        policy = policy.max_download_bytes(limit);
    }
    proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy.build().expect("valid policy"),
        )
        .expect("attach lease")
}

fn open_tunnel(endpoint: SocketAddr, port: u16) -> TcpStream {
    let mut client = TcpStream::connect(endpoint).expect("connect proxy");
    client
        .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
        .expect("write CONNECT");
    let mut response = [0_u8; 39];
    client
        .read_exact(&mut response)
        .expect("read CONNECT response");
    assert_eq!(&response, CONNECT_RESPONSE);
    client
}

#[test]
fn zero_download_limit_never_forwards_upstream_payload() {
    let (port, sender) = start_sender(b"secret");
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_for_ports(&proxy, &[port], Some(0));
    let mut client = open_tunnel(lease.endpoint().socket_addr(), port);
    let mut payload = Vec::new();
    client
        .read_to_end(&mut payload)
        .expect("read tunnel closure");

    assert_eq!(payload, b"");
    sender.join().expect("sender thread");
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(final_usage.downloaded_bytes, 6);
    assert_eq!(final_usage.denied_connections, 1);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn exact_download_limit_is_independent_for_each_tunnel() {
    let (first_port, first_sender) = start_sender(b"x");
    let (second_port, second_sender) = start_sender(b"y");
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_for_ports(&proxy, &[first_port, second_port], Some(1));

    for (port, expected) in [(first_port, b"x"), (second_port, b"y")] {
        let mut client = open_tunnel(lease.endpoint().socket_addr(), port);
        let mut payload = Vec::new();
        client.read_to_end(&mut payload).expect("read payload");
        assert_eq!(&payload, expected);
    }

    first_sender.join().expect("first sender thread");
    second_sender.join().expect("second sender thread");
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(final_usage.downloaded_bytes, 2);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn close_interrupts_both_sides_of_an_idle_tunnel() {
    let (port, accepted, peer_closed, peer_thread) = start_blocked_peer();
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_for_ports(&proxy, &[port], None);
    let mut client = open_tunnel(lease.endpoint().socket_addr(), port);
    accepted
        .recv_timeout(Duration::from_secs(1))
        .expect("upstream accepted proxy");

    let started = Instant::now();
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close idle tunnel")
        .usage();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(final_usage.active_connections, 0);

    client
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("client read timeout");
    assert_terminal_read(client.read(&mut [0_u8; 1]));
    assert_terminal_read(
        peer_closed
            .recv_timeout(Duration::from_secs(1))
            .expect("upstream closure result"),
    );
    peer_thread.join().expect("blocked peer thread");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn close_interrupts_an_uploader_when_upstream_never_reads() {
    let (port, accepted, release_peer, peer_thread) = start_nonreading_peer();
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_for_ports(&proxy, &[port], None);
    let mut client = open_tunnel(lease.endpoint().socket_addr(), port);
    client
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("client write timeout");
    accepted
        .recv_timeout(Duration::from_secs(1))
        .expect("upstream accepted proxy");
    let (writer_tx, writer_rx) = mpsc::sync_channel(1);
    let writer = thread::spawn(move || {
        let block = [0xa5_u8; 16 * 1024];
        let error = loop {
            if let Err(error) = client.write_all(&block) {
                break error.kind();
            }
        };
        writer_tx.send(error).expect("report uploader stop");
    });
    wait_for_bytes(|| lease.usage().uploaded_bytes);

    let started = Instant::now();
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close blocked upload")
        .usage();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(final_usage.active_connections, 0);
    assert!(matches!(
        writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("uploader closure"),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
    ));
    writer.join().expect("uploader thread");
    release_peer.send(()).expect("release upstream");
    peer_thread.join().expect("nonreading peer thread");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn close_interrupts_a_downloader_when_guest_never_reads() {
    let (port, accepted, writer_stopped, peer_thread) = start_flooding_peer();
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_for_ports(&proxy, &[port], None);
    let _client = open_tunnel(lease.endpoint().socket_addr(), port);
    accepted
        .recv_timeout(Duration::from_secs(1))
        .expect("upstream accepted proxy");
    wait_for_bytes(|| lease.usage().downloaded_bytes);

    let started = Instant::now();
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close blocked download")
        .usage();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(final_usage.active_connections, 0);
    assert!(matches!(
        writer_stopped
            .recv_timeout(Duration::from_secs(1))
            .expect("downloader closure"),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
    ));
    peer_thread.join().expect("flooding peer thread");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}
