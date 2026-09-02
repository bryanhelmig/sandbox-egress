//! Small process-boundary fixture for the Linux host-network conformance lane.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};

fn echo_connection(mut stream: TcpStream) {
    let mut buffer = [0_u8; 8 * 1_024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) if stream.write_all(&buffer[..count]).is_err() => return,
            Ok(_) => {}
        }
    }
}

fn spawn_echo(listener: TcpListener, stopping: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    listener
        .set_nonblocking(true)
        .expect("configure fixture listener");
    thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    thread::spawn(move || echo_connection(stream));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let bind_address: SocketAddr = arguments
        .next()
        .ok_or("missing proxy bind address")?
        .parse()?;
    let peer_ip: IpAddr = arguments.next().ok_or("missing peer source IP")?.parse()?;
    if arguments.next().is_some() {
        return Err("usage: linux_host_proxy BIND_ADDRESS PEER_IP".into());
    }

    let upstream = TcpListener::bind("127.0.0.1:0")?;
    let upstream_address = upstream.local_addr()?;
    let bypass = TcpListener::bind(SocketAddr::new(bind_address.ip(), 0))?;
    let bypass_address = bypass.local_addr()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let upstream_thread = spawn_echo(upstream, Arc::clone(&stopping));
    let bypass_thread = spawn_echo(bypass, Arc::clone(&stopping));

    let proxy = Proxy::start(ProxyConfig::default().with_bind_address(bind_address))?;
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse()?)
        .allow_port(upstream_address.port())
        .connection_attempt_rate(100, 25)?
        .build()?;
    let lease = proxy.attach(PeerIdentity::SourceIp(peer_ip), policy)?;

    println!("PROXY_ADDR={}", lease.endpoint().socket_addr());
    println!("UPSTREAM_ADDR={upstream_address}");
    println!("BYPASS_ADDR={bypass_address}");
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;

    let usage = lease
        .close(Instant::now() + Duration::from_secs(3))?
        .usage();
    proxy.shutdown(Instant::now() + Duration::from_secs(3))?;
    stopping.store(true, Ordering::Release);
    upstream_thread
        .join()
        .map_err(|_| "upstream thread panicked")?;
    bypass_thread.join().map_err(|_| "bypass thread panicked")?;
    println!(
        "FINAL accepted={} active={} denied={} completed={} upload={} download={}",
        usage.accepted_connections,
        usage.active_connections,
        usage.denied_connections,
        usage.completed_connections,
        usage.uploaded_bytes,
        usage.downloaded_bytes,
    );
    Ok(())
}
