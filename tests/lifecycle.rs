//! End-to-end lease lifecycle and tunnelling tests.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use sandbox_egress::{CloseErrorKind, PeerIdentity, Policy, Proxy, ProxyConfig};

fn local_policy(port: u16) -> Policy {
    Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("test CIDR"))
        .allow_port(port)
        .max_connections(16)
        .expect("positive limit")
        .build()
        .expect("valid policy")
}

fn start_echo() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind echo");
    let port = listener.local_addr().expect("echo address").port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept echo");
        let mut buffer = [0_u8; 128];
        while let Ok(read) = stream.read(&mut buffer) {
            if read == 0 {
                break;
            }
            if stream.write_all(&buffer[..read]).is_err() {
                break;
            }
        }
    });
    (port, handle)
}

fn attach_local(proxy: &Proxy, policy: Policy) -> sandbox_egress::Lease {
    proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach localhost")
}

#[test]
fn connect_tunnels_and_accounts_bytes() {
    let (port, echo) = start_echo();
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_local(&proxy, local_policy(port));
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
        .expect("write CONNECT");
    let mut response = [0_u8; 39];
    client
        .read_exact(&mut response)
        .expect("read CONNECT response");
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    client.write_all(b"ping").expect("write tunnel");
    let mut echoed = [0_u8; 4];
    client.read_exact(&mut echoed).expect("read tunnel");
    assert_eq!(&echoed, b"ping");
    client
        .shutdown(Shutdown::Write)
        .expect("finish tunnel upload");
    let mut trailing = Vec::new();
    client
        .read_to_end(&mut trailing)
        .expect("read tunnel shutdown");
    drop(client);
    echo.join().expect("echo thread");

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("certified close")
        .usage();
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.accepted_connections, 1);
    assert_eq!(final_usage.completed_connections, 1);
    assert_eq!(final_usage.uploaded_bytes, 4);
    assert_eq!(final_usage.downloaded_bytes, 4);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
}

#[test]
fn close_revokes_a_slow_header_without_waiting_for_the_peer() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_local(
        &proxy,
        Policy::builder()
            .allow_host("example.com")
            .expect("valid host")
            .build()
            .expect("valid policy"),
    );
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client.write_all(b"CON").expect("partial header");
    thread::sleep(Duration::from_millis(20));

    let started = Instant::now();
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close slow header")
        .usage();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(final_usage.active_connections, 0);

    client
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("read timeout");
    let mut byte = [0_u8; 1];
    assert!(matches!(client.read(&mut byte), Ok(0) | Err(_)));
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn failed_close_returns_the_still_owning_lease() {
    let config =
        ProxyConfig::default().with_identity_reuse_quiet_period(Duration::from_millis(100));
    let proxy = Proxy::start(config).expect("start proxy");
    let lease = attach_local(&proxy, Policy::builder().build().expect("valid policy"));

    let error = lease
        .close(Instant::now() + Duration::from_millis(1))
        .expect_err("quiet-period close should time out");
    assert_eq!(error.kind(), CloseErrorKind::DeadlineExceeded);
    let lease = error.into_lease();
    assert!(
        proxy
            .attach(
                PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                Policy::builder().build().expect("valid policy"),
            )
            .is_err()
    );

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("retry close");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn hostname_policy_denies_before_dns() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_local(&proxy, Policy::builder().build().expect("valid policy"));
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(b"CONNECT forbidden.invalid:443 HTTP/1.1\r\n\r\n")
        .expect("write CONNECT");
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read denial");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("host-denied"), "{response}");
    assert_eq!(lease.usage().denied_connections, 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}
