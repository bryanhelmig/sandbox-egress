//! Opt-in local tunnel data-plane throughput measurement.

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};
use socket2::SockRef;

const CONNECT_RESPONSE: &[u8; 39] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const CHUNK_BYTES: usize = 16 * 1_024;

#[derive(Clone, Copy, Debug)]
enum Direction {
    Upload,
    Download,
}

fn environment_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn direction() -> Direction {
    match std::env::var("SANDBOX_EGRESS_THROUGHPUT_DIRECTION").as_deref() {
        Ok("download") => Direction::Download,
        Ok("upload") | Err(_) => Direction::Upload,
        Ok(value) => panic!("unsupported throughput direction: {value}"),
    }
}

fn write_bytes(stream: &mut TcpStream, bytes: usize) {
    let chunk = [0_u8; CHUNK_BYTES];
    let mut remaining = bytes;
    while remaining > 0 {
        let count = remaining.min(chunk.len());
        stream.write_all(&chunk[..count]).expect("write payload");
        remaining -= count;
    }
}

fn read_bytes(stream: &mut TcpStream, bytes: usize) {
    let copied = std::io::copy(
        &mut stream.take(u64::try_from(bytes).expect("byte count fits u64")),
        &mut std::io::sink(),
    )
    .expect("read payload");
    assert_eq!(copied, u64::try_from(bytes).expect("byte count fits u64"));
}

fn handle_upstream(mut stream: TcpStream, direction: Direction, bytes: usize, start: &Barrier) {
    start.wait();
    match direction {
        Direction::Upload => {
            read_bytes(&mut stream, bytes);
            stream.write_all(b"x").expect("acknowledge upload");
        }
        Direction::Download => {
            SockRef::from(&stream)
                .set_linger(Some(Duration::ZERO))
                .expect("prepare completed download reset");
            write_bytes(&mut stream, bytes);
            stream.shutdown(Shutdown::Write).expect("finish download");
            let mut marker = [0_u8; 1];
            stream
                .read_exact(&mut marker)
                .expect("read download teardown marker");
        }
    }
}

fn start_upstream(
    concurrency: usize,
    direction: Direction,
    bytes: usize,
    start: Arc<Barrier>,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
    let port = listener.local_addr().expect("upstream address").port();
    let server = thread::spawn(move || {
        let mut handlers = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let (stream, _) = listener.accept().expect("accept proxy dial");
            let start = Arc::clone(&start);
            handlers.push(thread::spawn(move || {
                handle_upstream(stream, direction, bytes, &start);
            }));
        }
        for handler in handlers {
            handler.join().expect("upstream handler");
        }
    });
    (port, server)
}

fn open_tunnel(endpoint: SocketAddr, upstream_port: u16) -> TcpStream {
    let mut client = TcpStream::connect(endpoint).expect("connect proxy");
    SockRef::from(&client)
        .set_linger(Some(Duration::ZERO))
        .expect("reset client on failure");
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
    client
}

fn drive_tunnel(mut client: TcpStream, direction: Direction, bytes: usize, start: &Barrier) {
    start.wait();
    match direction {
        Direction::Upload => {
            write_bytes(&mut client, bytes);
            client.shutdown(Shutdown::Write).expect("finish upload");
            let mut ack = [0_u8; 1];
            client.read_exact(&mut ack).expect("read upload ack");
        }
        Direction::Download => {
            read_bytes(&mut client, bytes);
            client
                .write_all(b"x")
                .expect("send download teardown marker");
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set teardown timeout");
            let mut byte = [0_u8; 1];
            match client.read(&mut byte) {
                Ok(0) => {}
                Err(error) if error.kind() == ErrorKind::ConnectionReset => {}
                result => panic!("expected terminal tunnel teardown, got {result:?}"),
            }
        }
    }
}

#[test]
#[ignore = "throughput measurement is opt-in; run scripts/measure-throughput.sh"]
fn concurrent_tunnel_throughput() {
    let mebibytes = environment_usize("SANDBOX_EGRESS_THROUGHPUT_MIB", 32);
    let bytes = mebibytes.checked_mul(1_024 * 1_024).expect("byte count");
    let concurrency = environment_usize("SANDBOX_EGRESS_THROUGHPUT_CONCURRENCY", 8);
    let direction = direction();
    let start = Arc::new(Barrier::new(concurrency * 2 + 1));
    let (upstream_port, upstream_thread) =
        start_upstream(concurrency, direction, bytes, Arc::clone(&start));

    let proxy = Proxy::start(ProxyConfig::default().with_max_connections(concurrency * 2))
        .expect("start proxy");
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("loopback CIDR"))
        .allow_port(upstream_port)
        .max_connections(concurrency * 2)
        .expect("positive connection limit")
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");

    let endpoint = lease.endpoint().socket_addr();
    let mut clients = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let start = Arc::clone(&start);
        clients.push(thread::spawn(move || {
            let client = open_tunnel(endpoint, upstream_port);
            drive_tunnel(client, direction, bytes, &start);
        }));
    }
    start.wait();
    let started = Instant::now();
    for client in clients {
        client.join().expect("throughput client");
    }
    let elapsed = started.elapsed();
    upstream_thread.join().expect("upstream thread");

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(5))
        .expect("close measured lease")
        .usage();
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(
        final_usage.accepted_connections,
        u64::try_from(concurrency).expect("concurrency fits u64")
    );
    let total_bytes = u64::try_from(bytes.checked_mul(concurrency).expect("total byte count"))
        .expect("total bytes fit u64");
    let markers = u64::try_from(concurrency).expect("concurrency fits u64");
    match direction {
        Direction::Upload => {
            assert_eq!(final_usage.uploaded_bytes, total_bytes);
            assert_eq!(final_usage.downloaded_bytes, markers);
        }
        Direction::Download => {
            assert_eq!(final_usage.uploaded_bytes, markers);
            assert_eq!(final_usage.downloaded_bytes, total_bytes);
        }
    }
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");

    let total_mebibytes = mebibytes.checked_mul(concurrency).expect("total MiB count");
    let mib_per_second = f64::from(u32::try_from(total_mebibytes).expect("total MiB fits u32"))
        / elapsed.as_secs_f64();
    eprintln!(
        "throughput direction={direction:?} mebibytes_per_tunnel={mebibytes} concurrency={concurrency} elapsed_ms={} mebibytes_per_second={mib_per_second:.1} upload_bytes={} download_bytes={}",
        elapsed.as_millis(),
        final_usage.uploaded_bytes,
        final_usage.downloaded_bytes,
    );
}
