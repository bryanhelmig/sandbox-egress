//! End-to-end local connection setup benchmarks.
#![allow(missing_docs)]

use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use ipnet::IpNet;
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};
use socket2::Socket;

const LOCALHOST_CLIENT_HELLO: &[u8] = &[
    22, 3, 1, 0, 80, 1, 0, 0, 76, 3, 3, // record and handshake headers
    7, 7, 7, 7, 7, 7, 7, 7, // client random
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 0, 0, 2, 19, 1, 1, 0,
    0, 33, // session, cipher, compression, extensions
    0, 0, 0, 14, 0, 12, 0, 0, 9, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't', 0, 43, 0, 3,
    2, 3, 4, // supported versions
    0, 13, 0, 4, 0, 2, 4, 3, // signature algorithms
];

fn allowed_connect(criterion: &mut Criterion) {
    let (port, stop, upstream) = start_upstream();
    let proxy =
        Proxy::start(ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO))
            .expect("start benchmark proxy");
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("loopback CIDR"))
        .allow_port(port)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach benchmark lease");
    let endpoint = lease.endpoint().socket_addr();
    let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n");

    criterion.bench_function("connect_allowed_loopback", |bencher| {
        bencher.iter(|| {
            let mut client = TcpStream::connect(endpoint).expect("connect proxy");
            client.write_all(request.as_bytes()).expect("write CONNECT");
            let mut response = [0_u8; 39];
            client
                .read_exact(&mut response)
                .expect("read CONNECT response");
            black_box(response);
            reset_on_drop(client);
        });
    });

    lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("close benchmark lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("shutdown benchmark proxy");
    stop.store(true, Ordering::Release);
    let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, port));
    upstream.join().expect("join upstream");
}

fn allowed_hostname_connect(criterion: &mut Criterion, inspect_tls: bool) {
    let (upstream_address, stop, upstream) = start_receiving_upstream(LOCALHOST_CLIENT_HELLO);
    let port = upstream_address.port();
    let proxy =
        Proxy::start(ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO))
            .expect("start benchmark proxy");
    let mut policy = Policy::builder()
        .allow_host("localhost")
        .expect("valid hostname")
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("IPv4 loopback CIDR"))
        .allow_network("::1/128".parse::<IpNet>().expect("IPv6 loopback CIDR"))
        .allow_port(port);
    if inspect_tls {
        policy = policy.require_tls_sni();
    }
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy.build().expect("valid policy"),
        )
        .expect("attach benchmark lease");
    let endpoint = lease.endpoint().socket_addr();
    let mut request = format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n").into_bytes();
    request.extend_from_slice(LOCALHOST_CLIENT_HELLO);
    let name = if inspect_tls {
        "connect_allowed_visible_sni"
    } else {
        "connect_allowed_hostname"
    };

    criterion.bench_function(name, |bencher| {
        bencher.iter(|| {
            let mut client = TcpStream::connect(endpoint).expect("connect proxy");
            client.write_all(&request).expect("write CONNECT and hello");
            let mut response = [0_u8; 39];
            client
                .read_exact(&mut response)
                .expect("read CONNECT response");
            let mut acknowledgement = [0_u8; 1];
            client
                .read_exact(&mut acknowledgement)
                .expect("read upstream acknowledgement");
            black_box((response, acknowledgement));
            reset_on_drop(client);
        });
    });

    lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("close benchmark lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("shutdown benchmark proxy");
    stop.store(true, Ordering::Release);
    let _ = TcpStream::connect(upstream_address);
    upstream.join().expect("join upstream");
}

fn allowed_hostname(criterion: &mut Criterion) {
    allowed_hostname_connect(criterion, false);
}

fn allowed_visible_sni(criterion: &mut Criterion) {
    allowed_hostname_connect(criterion, true);
}

fn denied_connect(criterion: &mut Criterion) {
    let proxy =
        Proxy::start(ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO))
            .expect("start benchmark proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        )
        .expect("attach benchmark lease");
    let endpoint = lease.endpoint().socket_addr();

    criterion.bench_function("connect_denied_hostname", |bencher| {
        bencher.iter(|| {
            let mut client = TcpStream::connect(endpoint).expect("connect proxy");
            client
                .write_all(b"CONNECT forbidden.invalid:443 HTTP/1.1\r\n\r\n")
                .expect("write CONNECT");
            let mut response = [0_u8; 256];
            let bytes = client.read(&mut response).expect("read denial");
            black_box(&response[..bytes]);
            reset_on_drop(client);
        });
    });

    lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("close benchmark lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("shutdown benchmark proxy");
}

fn oversized_header(criterion: &mut Criterion) {
    const HEADER_BYTES: usize = 1024 * 1024;

    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_identity_reuse_quiet_period(Duration::ZERO)
            .with_max_header_bytes(HEADER_BYTES),
    )
    .expect("start benchmark proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        )
        .expect("attach benchmark lease");
    let endpoint = lease.endpoint().socket_addr();
    let request = vec![b'a'; HEADER_BYTES];

    criterion.bench_function("connect_oversized_header_1mib", |bencher| {
        bencher.iter(|| {
            let mut client = TcpStream::connect(endpoint).expect("connect proxy");
            client.write_all(&request).expect("write oversized header");
            let mut response = [0_u8; 256];
            let bytes = client.read(&mut response).expect("read denial");
            assert!(response[..bytes].starts_with(b"HTTP/1.1 431"));
            black_box(bytes);
            reset_on_drop(client);
        });
    });

    lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("close benchmark lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("shutdown benchmark proxy");
}

fn reset_on_drop(stream: TcpStream) {
    let socket = Socket::from(stream);
    socket
        .set_linger(Some(Duration::ZERO))
        .expect("set benchmark socket linger");
}

fn start_upstream() -> (u16, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
    let port = listener.local_addr().expect("upstream address").port();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        loop {
            let (stream, _) = listener.accept().expect("accept upstream");
            drop(stream);
            if thread_stop.load(Ordering::Acquire) {
                break;
            }
        }
    });
    (port, stop, handle)
}

fn start_receiving_upstream(
    expected: &'static [u8],
) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let address = ("localhost", 0)
        .to_socket_addrs()
        .expect("resolve localhost")
        .next()
        .expect("localhost address");
    let listener = TcpListener::bind(address).expect("bind hostname upstream");
    let address = listener.local_addr().expect("upstream address");
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().expect("accept upstream");
            if thread_stop.load(Ordering::Acquire) {
                break;
            }
            let mut received = vec![0_u8; expected.len()];
            stream
                .read_exact(&mut received)
                .expect("read forwarded ClientHello");
            assert_eq!(received, expected, "proxy changed the ClientHello");
            stream.write_all(b"x").expect("acknowledge ClientHello");
        }
    });
    (address, stop, handle)
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_secs(1))
        .sample_size(20);
    targets = allowed_connect, allowed_hostname, allowed_visible_sni, denied_connect, oversized_header
}
criterion_main!(benches);
