//! End-to-end local connection setup benchmarks.
#![allow(missing_docs)]

use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use ipnet::IpNet;
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};
use socket2::Socket;

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

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_secs(1))
        .sample_size(20);
    targets = allowed_connect, denied_connect
}
criterion_main!(benches);
