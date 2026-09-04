//! Small process-boundary fixture for the Linux host-network conformance lane.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig, Usage};

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

fn command(expected: &str) -> io::Result<()> {
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if line.trim() != expected {
        return Err(io::Error::other(format!(
            "expected fixture command {expected}"
        )));
    }
    Ok(())
}

fn policy(port: u16) -> Result<Policy, sandbox_egress::PolicyError> {
    Policy::builder()
        .allow_network("127.0.0.0/8".parse().expect("fixture CIDR"))
        .allow_port(port)
        .build()
}

fn print_final(generation: u8, usage: Usage) {
    assert_eq!(usage.active_connections, 0);
    println!(
        "FINAL generation={generation} accepted={} active={} denied={} completed={} upload={} download={}",
        usage.accepted_connections,
        usage.active_connections,
        usage.denied_connections,
        usage.completed_connections,
        usage.uploaded_bytes,
        usage.downloaded_bytes,
    );
    io::stdout().flush().expect("flush fixture certificate");
}

fn wait_for_progress(exchanges: &AtomicU64, previous: u64) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while exchanges.load(Ordering::Acquire) <= previous {
        assert!(
            Instant::now() < deadline,
            "unrelated tunnel stopped making progress"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn start_bystander(
    endpoint: SocketAddr,
    port: u16,
    stopping: Arc<AtomicBool>,
    exchanges: Arc<AtomicU64>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // One socket for the entire two-generation certificate. Reconnecting
        // here would hide accidental cancellation of the unrelated lease.
        let mut client = TcpStream::connect_timeout(&endpoint, Duration::from_secs(2)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(
            client,
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
        )
        .unwrap();
        let mut response = [0; 39];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        while !stopping.load(Ordering::Acquire) {
            client.write_all(b"steady").unwrap();
            let mut echo = [0; 6];
            client.read_exact(&mut echo).unwrap();
            assert_eq!(&echo, b"steady");
            exchanges.fetch_add(1, Ordering::Release);
            thread::sleep(Duration::from_millis(5));
        }
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut end = [0];
        assert_eq!(client.read(&mut end).unwrap(), 0);
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let bind_address: SocketAddr = arguments.next().ok_or("missing bind address")?.parse()?;
    let peer_ip: IpAddr = arguments.next().ok_or("missing peer source IP")?.parse()?;
    if arguments.next().is_some() || bind_address.ip() == peer_ip {
        return Err("usage: linux_host_proxy BIND_ADDRESS DISTINCT_PEER_IP".into());
    }

    let first = TcpListener::bind("127.0.0.1:0")?;
    let second = TcpListener::bind("127.0.0.1:0")?;
    let first_address = first.local_addr()?;
    let second_address = second.local_addr()?;
    let bypass = TcpListener::bind(SocketAddr::new(bind_address.ip(), 0))?;
    let bypass_address = bypass.local_addr()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let servers =
        [first, second, bypass].map(|listener| spawn_echo(listener, Arc::clone(&stopping)));

    let proxy = Proxy::start(ProxyConfig::default().with_bind_address(bind_address))?;
    let identity = PeerIdentity::SourceIp(peer_ip);
    let old = proxy.attach(identity.clone(), policy(first_address.port())?)?;
    let old_id = old.id();
    let bystander = proxy.attach(
        PeerIdentity::SourceIp(bind_address.ip()),
        policy(first_address.port())?,
    )?;
    let exchanges = Arc::new(AtomicU64::new(0));
    let bystander_stop = Arc::new(AtomicBool::new(false));
    let worker = start_bystander(
        proxy.endpoint().socket_addr(),
        first_address.port(),
        Arc::clone(&bystander_stop),
        Arc::clone(&exchanges),
    );
    wait_for_progress(&exchanges, 0);

    println!("PROXY_ADDR={}", proxy.endpoint().socket_addr());
    println!("UPSTREAM_ADDR={first_address}");
    println!("REPLACEMENT_ADDR={second_address}");
    println!("BYPASS_ADDR={bypass_address}");
    println!("PROXY_PID={}", std::process::id());
    io::stdout().flush()?;

    command("close")?; // The host fences the old guest first.
    let before = exchanges.load(Ordering::Acquire);
    let old_usage = old.close(Instant::now() + Duration::from_secs(3))?.usage();
    print_final(1, old_usage);
    wait_for_progress(&exchanges, before);
    command("attach")?; // Only after old host resources have been removed.
    let replacement = proxy.attach(identity, policy(second_address.port())?)?;
    assert_ne!(replacement.id(), old_id);
    assert_eq!(replacement.usage(), Usage::default());
    println!(
        "ATTACHED generation=2 endpoint={}",
        replacement.endpoint().socket_addr()
    );
    io::stdout().flush()?;

    command("finish")?;
    let before = exchanges.load(Ordering::Acquire);
    let new_usage = replacement
        .close(Instant::now() + Duration::from_secs(3))?
        .usage();
    assert_eq!(
        new_usage.denied_connections, 1,
        "old destination must be denied"
    );
    assert!(new_usage.uploaded_bytes > 0 && new_usage.downloaded_bytes > 0);
    print_final(2, new_usage);
    wait_for_progress(&exchanges, before);
    bystander_stop.store(true, Ordering::Release);
    worker.join().map_err(|_| "bystander tunnel failed")?;
    let usage = bystander
        .close(Instant::now() + Duration::from_secs(3))?
        .usage();
    let bytes = exchanges.load(Ordering::Acquire) * 6;
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.completed_connections, 1);
    assert_eq!(usage.denied_connections, 0);
    assert_eq!(usage.active_connections, 0);
    assert_eq!(usage.uploaded_bytes, bytes);
    assert_eq!(usage.downloaded_bytes, bytes);
    println!("BYSTANDER exchanges={} exact_bytes={bytes}", bytes / 6);
    proxy.shutdown(Instant::now() + Duration::from_secs(3))?;
    stopping.store(true, Ordering::Release);
    for server in servers {
        server.join().map_err(|_| "fixture server failed")?;
    }
    Ok(())
}
