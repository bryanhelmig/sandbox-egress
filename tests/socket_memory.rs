//! Opt-in measurement of host kernel socket memory held by a stalled tunnel.
//!
//! A guest opens its full connection allowance to an allowed upstream that
//! sends data, then stops reading. Nothing in the proxy sets `SO_SNDBUF` or
//! `SO_RCVBUF`, so each direction of each tunnel keeps whatever the host
//! kernel's autotuning grants. This measures how deep that is.

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};
use socket2::SockRef;

const CONNECT_RESPONSE: &[u8; 39] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const CHUNK_BYTES: usize = 64 * 1_024;
/// One `write` waits this long for the kernel to accept any byte.
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
/// Consecutive timed-out writes that count as a settled stall.
const STALL_TIMEOUTS: u32 = 8;
/// Clamp the upstream's own send buffer. Bytes parked there belong to the
/// remote peer, not the host, so a small value keeps that term out of the
/// measurement. Set to 0 to leave the peer's autotuning alone.
const UPSTREAM_SEND_BUFFER_BYTES: usize = 64 * 1_024;

fn environment_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// Bytes the upstream pushed in, and its own send buffer at the stall.
struct UpstreamStall {
    accepted_bytes: u64,
    send_buffer_bytes: usize,
}

/// Write until the kernel refuses every byte for `STALL_TIMEOUTS` attempts.
fn fill_until_stalled(mut stream: TcpStream) -> UpstreamStall {
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .expect("bound each upstream write");
    let clamp = environment_usize(
        "SANDBOX_EGRESS_SOCKET_MEMORY_UPSTREAM_SNDBUF",
        UPSTREAM_SEND_BUFFER_BYTES,
    );
    if clamp > 0 {
        SockRef::from(&stream)
            .set_send_buffer_size(clamp)
            .expect("clamp upstream send buffer");
    }
    let chunk = vec![0x5a_u8; CHUNK_BYTES].into_boxed_slice();
    let mut accepted_bytes = 0_u64;
    let mut timeouts = 0;
    while timeouts < STALL_TIMEOUTS {
        match stream.write(&chunk) {
            Ok(0) => break,
            Ok(written) => {
                accepted_bytes += written as u64;
                timeouts = 0;
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                timeouts += 1;
            }
            Err(error) => panic!("upstream write failed: {error}"),
        }
    }
    let send_buffer_bytes = SockRef::from(&stream)
        .send_buffer_size()
        .expect("read upstream send buffer");
    UpstreamStall {
        accepted_bytes,
        send_buffer_bytes,
    }
}

fn start_upstream(
    concurrency: usize,
    ready: Arc<Barrier>,
    results: Sender<UpstreamStall>,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
    let port = listener.local_addr().expect("upstream address").port();
    let server = thread::spawn(move || {
        let mut handlers = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let (stream, _) = listener.accept().expect("accept proxy dial");
            let ready = Arc::clone(&ready);
            let results = results.clone();
            handlers.push(thread::spawn(move || {
                ready.wait();
                results
                    .send(fill_until_stalled(stream))
                    .expect("report stall depth");
            }));
        }
        for handler in handlers {
            handler.join().expect("upstream handler");
        }
    });
    (port, server)
}

/// A guest tunnel that completes CONNECT and then never reads again.
fn open_silent_tunnel(endpoint: SocketAddr, upstream_port: u16) -> TcpStream {
    let mut client = TcpStream::connect(endpoint).expect("connect proxy");
    SockRef::from(&client)
        .set_linger(Some(Duration::ZERO))
        .expect("reset client on teardown");
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

#[test]
#[ignore = "socket memory measurement is opt-in; it deliberately stalls every tunnel"]
fn stalled_tunnels_hold_unbounded_kernel_socket_memory() {
    let concurrency = environment_usize("SANDBOX_EGRESS_SOCKET_MEMORY_CONNECTIONS", 8);
    let ready = Arc::new(Barrier::new(concurrency + 1));
    let (results, stalls) = channel();
    let (upstream_port, upstream_thread) = start_upstream(concurrency, Arc::clone(&ready), results);

    let proxy = Proxy::start(ProxyConfig::default().with_max_connections(concurrency))
        .expect("start proxy");
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("loopback CIDR"))
        .allow_port(upstream_port)
        .max_connections(concurrency)
        .expect("positive connection limit")
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");

    // Open the whole allowance, then stop reading on every tunnel.
    let endpoint = lease.endpoint().socket_addr();
    let clients: Vec<TcpStream> = (0..concurrency)
        .map(|_| open_silent_tunnel(endpoint, upstream_port))
        .collect();
    assert_eq!(clients.len(), concurrency);

    let started = Instant::now();
    ready.wait();
    let mut accepted_bytes = 0_u64;
    let mut upstream_send_buffer_bytes = 0_usize;
    for _ in 0..concurrency {
        let stall = stalls
            .recv_timeout(Duration::from_secs(120))
            .expect("every tunnel reaches a settled stall");
        accepted_bytes += stall.accepted_bytes;
        upstream_send_buffer_bytes = upstream_send_buffer_bytes.max(stall.send_buffer_bytes);
    }
    let elapsed = started.elapsed();

    // Guest-side receive buffers are the guest's memory, not the host's.
    let guest_receive_buffer_bytes = clients
        .iter()
        .map(|client| {
            SockRef::from(client)
                .recv_buffer_size()
                .expect("read guest receive buffer")
        })
        .max()
        .expect("at least one tunnel");

    // Bytes the proxy pulled out of the upstream sockets while the guest read
    // nothing: they now sit in host send buffers, the copy buffer, or the
    // guest's receive buffer.
    let usage = lease.usage();
    let downloaded_bytes = usage.downloaded_bytes;
    assert_eq!(usage.active_connections, concurrency as u64);

    drop(clients);
    lease
        .close(Instant::now() + Duration::from_secs(30))
        .expect("close stalled lease");
    upstream_thread.join().expect("upstream thread");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(10))
        .expect("proxy shutdown");

    let per_tunnel = accepted_bytes / concurrency as u64;
    let host_held_per_tunnel = per_tunnel
        .saturating_sub(upstream_send_buffer_bytes as u64)
        .saturating_sub(guest_receive_buffer_bytes as u64);
    eprintln!(
        "socket_memory connections={concurrency} elapsed_ms={} \
         accepted_bytes_total={accepted_bytes} accepted_bytes_per_tunnel={per_tunnel} \
         downloaded_bytes_total={downloaded_bytes} \
         upstream_send_buffer_bytes={upstream_send_buffer_bytes} \
         guest_recv_buffer_bytes={guest_receive_buffer_bytes} \
         host_held_bytes_per_tunnel_estimate={host_held_per_tunnel} \
         host_held_bytes_total_estimate={}",
        elapsed.as_millis(),
        host_held_per_tunnel * concurrency as u64,
    );

    // The proxy sets no buffer sizes, so this is a kernel-default measurement,
    // not a contract. Assert only that a stall really parks payload bytes in
    // host-side buffers rather than refusing them at the first byte.
    assert!(
        downloaded_bytes > 0,
        "expected the proxy to buffer upstream payload while the guest stalls"
    );
}
