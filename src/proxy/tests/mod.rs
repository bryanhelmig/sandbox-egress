
use super::*;

mod deadlines;
mod dial_budget;
mod dns_wire;

struct BoundaryReader {
    step: u8,
    extra_before_error: bool,
}

impl AsyncRead for BoundaryReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.step {
            0 => {
                buffer.put_slice(b"abc");
                self.step = 1;
                Poll::Ready(Ok(()))
            }
            1 if self.extra_before_error => {
                buffer.put_slice(b"x");
                self.step = 2;
                Poll::Ready(Ok(()))
            }
            _ => Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionReset))),
        }
    }
}

#[test]
fn byte_limit_requires_an_observed_excess_byte() {
    fn boundary_result(extra_before_error: bool) -> (io::Error, Usage) {
        let counters = Arc::new(Counters::default());
        let mut reader = Metered::new(
            BoundaryReader {
                step: 0,
                extra_before_error,
            },
            Arc::clone(&counters),
            Direction::Upload,
            Some(3),
            0,
            None,
        );
        let runtime = RuntimeBuilder::new_current_thread()
            .build()
            .expect("test runtime");
        let error = runtime.block_on(async {
            let mut buffer = [0_u8; 8];
            assert_eq!(reader.read(&mut buffer).await.expect("boundary read"), 3);
            reader
                .read(&mut buffer)
                .await
                .expect_err("boundary outcome")
        });
        (error, counters.snapshot())
    }

    let (reset, reset_usage) = boundary_result(false);
    assert_eq!(reset.kind(), io::ErrorKind::ConnectionReset);
    assert!(!is_transfer_limit_error(&reset));
    assert_eq!(reset_usage.uploaded_bytes, 3);

    let (limited, limited_usage) = boundary_result(true);
    assert!(is_transfer_limit_error(&limited));
    assert_eq!(limited_usage.uploaded_bytes, 4);
}

struct ActiveLookup(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ActiveLookup {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct PendingResolver {
    entered: mpsc::Sender<()>,
    active: Arc<std::sync::atomic::AtomicUsize>,
}

impl TestResolver for PendingResolver {
    fn lookup<'a>(
        &'a self,
        _hostname: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async move {
            self.active.fetch_add(1, Ordering::AcqRel);
            let _active = ActiveLookup(Arc::clone(&self.active));
            self.entered.send(()).expect("report DNS entry");
            std::future::pending::<io::Result<Vec<IpAddr>>>().await
        })
    }
}

struct LateAnswerResolver {
    started: mpsc::Sender<()>,
    answer: Mutex<Option<tokio::sync::oneshot::Receiver<Vec<IpAddr>>>>,
}

struct FixedAnswerResolver(Vec<IpAddr>);

struct StartupDropProbe(Arc<std::sync::atomic::AtomicBool>);

impl Drop for StartupDropProbe {
    fn drop(&mut self) {
        std::thread::sleep(Duration::from_millis(100));
        self.0.store(true, Ordering::Release);
    }
}

struct FailingResolver;

struct CapturingResolver(mpsc::Sender<String>);

struct CapturingAnswerResolver {
    captured: mpsc::Sender<String>,
    answers: Vec<IpAddr>,
}

fn read_blocking_header(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        std::io::Read::read_exact(stream, &mut byte).expect("read complete header");
        header.push(byte[0]);
    }
    header
}

fn start_refusing_upstream() -> (
    SocketAddr,
    mpsc::Receiver<Vec<Vec<u8>>>,
    thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind upstream proxy");
    let address = listener.local_addr().expect("upstream proxy address");
    let (requests_tx, requests_rx) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        let (mut refused, _) = listener.accept().expect("accept first attempt");
        refused
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set first read timeout");
        requests.push(read_blocking_header(&mut refused));
        std::io::Write::write_all(&mut refused, b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
            .expect("refuse first target");
        drop(refused);

        let (mut accepted, _) = listener.accept().expect("accept fallback attempt");
        accepted
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set fallback read timeout");
        requests.push(read_blocking_header(&mut accepted));
        std::io::Write::write_all(
            &mut accepted,
            b"HTTP/1.1 200 Connection Established\r\n\r\nhello",
        )
        .expect("approve fallback target");
        let mut upload = [0_u8; 4];
        std::io::Read::read_exact(&mut accepted, &mut upload).expect("read tunnel upload");
        assert_eq!(&upload, b"ping");
        std::io::Write::write_all(&mut accepted, b"pong").expect("write tunnel download");
        std::io::Read::read_to_end(&mut accepted, &mut Vec::new())
            .expect("observe upload shutdown");
        requests_tx.send(requests).expect("send CONNECT requests");
    });
    (address, requests_rx, server)
}

fn start_local_dns(
    expected_queries: usize,
    respond: fn(&[u8]) -> Vec<u8>,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind local DNS server");
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set DNS server timeout");
    let address = socket.local_addr().expect("local DNS address");
    let server = thread::spawn(move || {
        let mut packet = [0_u8; 2_048];
        for _ in 0..expected_queries {
            let (length, peer) = socket.recv_from(&mut packet).expect("receive DNS query");
            let response = respond(&packet[..length]);
            socket.send_to(&response, peer).expect("send DNS response");
        }
    });
    (address, server)
}

fn start_truncated_udp_dns() -> (SocketAddr, thread::JoinHandle<()>) {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind local TCP DNS server");
    listener
        .set_nonblocking(true)
        .expect("configure TCP DNS server");
    let address = listener.local_addr().expect("local TCP DNS address");
    let socket = std::net::UdpSocket::bind(address).expect("bind matching UDP DNS server");
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set UDP DNS server timeout");

    let server = thread::spawn(move || {
        let mut udp_query = [0_u8; 2_048];
        let (length, peer) = socket
            .recv_from(&mut udp_query)
            .expect("receive UDP DNS query");
        let question_end = local_dns_question_end(&udp_query[..length]);
        let mut truncated = Vec::with_capacity(question_end);
        truncated.extend_from_slice(&udp_query[..2]);
        truncated.extend_from_slice(&[0x83, 0x80]);
        truncated.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
        truncated.extend_from_slice(&udp_query[12..question_end]);
        socket
            .send_to(&truncated, peer)
            .expect("send truncated UDP DNS response");

        let accept_deadline = Instant::now() + Duration::from_secs(2);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < accept_deadline,
                        "TCP DNS fallback was not attempted"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept TCP DNS fallback: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set TCP DNS read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("set TCP DNS write timeout");
        let mut length_bytes = [0_u8; 2];
        stream
            .read_exact(&mut length_bytes)
            .expect("read TCP DNS query length");
        let query_length = usize::from(u16::from_be_bytes(length_bytes));
        let mut tcp_query = vec![0_u8; query_length];
        stream
            .read_exact(&mut tcp_query)
            .expect("read TCP DNS query");
        let response = local_a_response(&tcp_query);
        let response_length = u16::try_from(response.len()).expect("bounded DNS response");
        stream
            .write_all(&response_length.to_be_bytes())
            .expect("write TCP DNS response length");
        stream.write_all(&response).expect("write TCP DNS response");
    });
    (address, server)
}

fn local_a_response(query: &[u8]) -> Vec<u8> {
    let question_end = local_dns_question_end(query);
    let mut response = Vec::with_capacity(question_end + 16);
    response.extend_from_slice(&query[..2]);
    response.extend_from_slice(&[0x81, 0x80]);
    response.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 0]);
    response.extend_from_slice(&query[12..question_end]);
    response.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1]);
    response
}

fn local_nxdomain_response(query: &[u8]) -> Vec<u8> {
    let question_end = local_dns_question_end(query);
    let mut response = Vec::with_capacity(question_end + 36);
    response.extend_from_slice(&query[..2]);
    response.extend_from_slice(&[0x81, 0x83]);
    response.extend_from_slice(&[0, 1, 0, 0, 0, 1, 0, 0]);
    response.extend_from_slice(&query[12..question_end]);
    response.extend_from_slice(&[
        0xc0, 0x0c, 0, 6, 0, 1, 0, 0, 0, 60, 0, 24, 0xc0, 0x0c, 0xc0, 0x0c, 0, 0, 0, 1, 0, 0, 0,
        60, 0, 0, 0, 60, 0, 0, 0, 60, 0, 0, 0, 60, 0, 0, 0, 60,
    ]);
    response
}

fn local_servfail_response(query: &[u8]) -> Vec<u8> {
    let question_end = local_dns_question_end(query);
    let mut response = Vec::with_capacity(question_end);
    response.extend_from_slice(&query[..2]);
    response.extend_from_slice(&[0x81, 0x82]);
    response.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    response.extend_from_slice(&query[12..question_end]);
    response
}

fn local_dns_question_end(query: &[u8]) -> usize {
    assert!(query.len() >= 17, "DNS query is too short");
    let name_end = query[12..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|offset| 13 + offset)
        .expect("DNS question terminator");
    let question_end = name_end.checked_add(4).expect("bounded DNS question");
    assert!(question_end <= query.len(), "complete DNS question");
    question_end
}

fn local_dns_resolver(address: SocketAddr, config: &ProxyConfig) -> TokioResolver {
    let mut connection = hickory_resolver::config::ConnectionConfig::udp();
    connection.port = address.port();
    let name_server =
        hickory_resolver::config::NameServerConfig::new(address.ip(), true, vec![connection]);
    let mut resolver_config = hickory_resolver::config::ResolverConfig::default();
    resolver_config.add_name_server(name_server);
    let mut builder = TokioResolver::builder_with_config(
        resolver_config,
        hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
    );
    let options = builder.options_mut();
    options.ip_strategy = hickory_resolver::config::LookupIpStrategy::Ipv4Only;
    options.use_hosts_file = hickory_resolver::config::ResolveHosts::Never;
    options.attempts = 1;
    options.timeout = Duration::from_secs(1);
    apply_resolver_cache_options(options, config);
    builder.build().expect("build local DNS resolver")
}

impl TestResolver for FixedAnswerResolver {
    fn lookup<'a>(
        &'a self,
        _hostname: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

impl TestResolver for StartupDropProbe {
    fn lookup<'a>(
        &'a self,
        _hostname: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl TestResolver for FailingResolver {
    fn lookup<'a>(
        &'a self,
        _hostname: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async { Err(io::Error::other("controlled DNS failure")) })
    }
}

impl TestResolver for CapturingResolver {
    fn lookup<'a>(
        &'a self,
        hostname: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        self.0
            .send(hostname.to_owned())
            .expect("capture resolver hostname");
        Box::pin(async { Ok(vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)]) })
    }
}

impl TestResolver for CapturingAnswerResolver {
    fn lookup<'a>(
        &'a self,
        hostname: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        self.captured
            .send(hostname.to_owned())
            .expect("capture resolver hostname");
        Box::pin(async { Ok(self.answers.clone()) })
    }
}

fn assert_dns_terminal_denial(hostname: &str, resolver: Arc<dyn TestResolver>, reason: &str) {
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start DNS terminal-result proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy(hostname, 443),
        )
        .expect("attach DNS terminal-result lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT {hostname}:443 HTTP/1.1\r\nHost: {hostname}\r\n\r\n").as_bytes(),
    )
    .expect("write CONNECT");

    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read DNS denial");
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert!(response.contains(reason), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    assert_eq!(lease.usage().denied_connections, 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close DNS terminal-result lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

struct ActiveDial(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ActiveDial {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct PendingConnector {
    entered: mpsc::Sender<SocketAddr>,
    active: Arc<std::sync::atomic::AtomicUsize>,
}

struct SlowCancelConnector {
    entered: mpsc::Sender<SocketAddr>,
    cleanup_delay: Duration,
}

struct SlowCancelFuture {
    entered: Option<mpsc::Sender<SocketAddr>>,
    address: SocketAddr,
    cleanup_delay: Duration,
}

impl Future for SlowCancelFuture {
    type Output = io::Result<TcpStream>;

    fn poll(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(entered) = self.entered.take() {
            entered.send(self.address).expect("report slow dial entry");
        }
        Poll::Pending
    }
}

impl Drop for SlowCancelFuture {
    fn drop(&mut self) {
        thread::sleep(self.cleanup_delay);
    }
}

struct RejectingConnector(Arc<std::sync::atomic::AtomicUsize>);

struct PendingThenLoopbackConnector {
    pending: SocketAddr,
    loopback: SocketAddr,
    attempts: Arc<Mutex<Vec<SocketAddr>>>,
}

struct RefuseAfterReleaseConnector {
    first: SocketAddr,
    entered: mpsc::Sender<()>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    attempts: Arc<Mutex<Vec<SocketAddr>>>,
}

impl TestConnector for PendingConnector {
    fn connect(
        &self,
        address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send + '_>> {
        Box::pin(async move {
            self.active.fetch_add(1, Ordering::AcqRel);
            let _active = ActiveDial(Arc::clone(&self.active));
            self.entered.send(address).expect("report dial entry");
            std::future::pending::<io::Result<TcpStream>>().await
        })
    }
}

impl TestConnector for SlowCancelConnector {
    fn connect(
        &self,
        address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send + '_>> {
        Box::pin(SlowCancelFuture {
            entered: Some(self.entered.clone()),
            address,
            cleanup_delay: self.cleanup_delay,
        })
    }
}

impl TestConnector for RejectingConnector {
    fn connect(
        &self,
        _address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send + '_>> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Err(io::Error::other("test connector rejected dial")) })
    }
}

impl TestConnector for PendingThenLoopbackConnector {
    fn connect(
        &self,
        address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send + '_>> {
        self.attempts
            .lock()
            .expect("attempt list poisoned")
            .push(address);
        if address == self.pending {
            Box::pin(std::future::pending())
        } else {
            Box::pin(TcpStream::connect(self.loopback))
        }
    }
}

impl TestConnector for RefuseAfterReleaseConnector {
    fn connect(
        &self,
        address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send + '_>> {
        self.attempts
            .lock()
            .expect("attempt list poisoned")
            .push(address);
        if address != self.first {
            return Box::pin(async { Err(io::Error::other("unexpected fallback dial")) });
        }
        let release = self
            .release
            .lock()
            .expect("release receiver poisoned")
            .take()
            .expect("first attempt is unique");
        self.entered.send(()).expect("report first attempt");
        Box::pin(async move {
            release.await.map_err(io::Error::other)?;
            Err(io::Error::other("controlled first refusal"))
        })
    }
}

impl TestResolver for LateAnswerResolver {
    fn lookup<'a>(
        &'a self,
        _hostname: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        let answer = self
            .answer
            .lock()
            .expect("answer receiver poisoned")
            .take()
            .expect("one lookup expected");
        self.started.send(()).expect("report DNS entry");
        Box::pin(async move { answer.await.map_err(io::Error::other) })
    }
}

fn hostname_policy(hostname: &str, port: u16) -> Policy {
    Policy::builder()
        .allow_host(hostname)
        .expect("valid test hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid loopback test network"))
        .allow_port(port)
        .dns_timeout(Duration::from_secs(2))
        .handshake_timeout(Duration::from_secs(2))
        .build()
        .expect("valid policy")
}

fn ip_policy(port: u16, handshake_timeout: Duration) -> Policy {
    Policy::builder()
        .allow_network("127.0.0.0/8".parse().expect("valid loopback test network"))
        .allow_port(port)
        .dns_timeout(handshake_timeout)
        .handshake_timeout(handshake_timeout)
        .build()
        .expect("valid policy")
}

struct PendingDialFixture {
    proxy: Proxy,
    lease: Lease,
    client: std::net::TcpStream,
    active: Arc<std::sync::atomic::AtomicUsize>,
}

fn pending_dial_fixture(port: u16) -> PendingDialFixture {
    let (entered_tx, entered_rx) = mpsc::channel();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(PendingConnector {
        entered: entered_tx,
        active: Arc::clone(&active),
    });
    let proxy =
        Proxy::start_with_test_connector(ProxyConfig::default(), connector).expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            ip_policy(port, Duration::from_secs(2)),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
    )
    .expect("write CONNECT");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("dial entered");
    PendingDialFixture {
        proxy,
        lease,
        client,
        active,
    }
}

fn assert_client_stopped(mut client: std::net::TcpStream) {
    client
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("set client timeout");
    let mut byte = [0_u8; 1];
    match std::io::Read::read(&mut client, &mut byte) {
        Ok(0) => {}
        Err(error)
            if !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) => {}
        outcome => panic!("guest socket remained open: {outcome:?}"),
    }
}

#[test]
fn pending_first_address_cannot_starve_a_reachable_second_address() {
    let target = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind fallback target");
    let target_address = target.local_addr().expect("fallback target address");
    let first = SocketAddr::new("192.0.2.1".parse().expect("first test IP"), 443);
    let second = SocketAddr::new("198.51.100.1".parse().expect("second test IP"), 443);
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let connector = ConnectorBackend::Test(Arc::new(PendingThenLoopbackConnector {
        pending: first,
        loopback: target_address,
        attempts: Arc::clone(&attempts),
    }));
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime");

    let connected = runtime.block_on(dial_approved_addresses(
        vec![first, second],
        &connector,
        &CancellationToken::new(),
        TokioInstant::now() + Duration::from_millis(400),
    ));
    assert!(connected.is_some(), "reachable fallback was not attempted");
    assert_eq!(
        *attempts.lock().expect("attempt list poisoned"),
        vec![first, second]
    );
}

#[test]
fn revocation_before_refusal_prevents_a_fallback_dial() {
    let first = SocketAddr::new("192.0.2.1".parse().expect("first test IP"), 443);
    let second = SocketAddr::new("192.0.2.2".parse().expect("second test IP"), 443);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let connector = ConnectorBackend::Test(Arc::new(RefuseAfterReleaseConnector {
        first,
        entered: entered_tx,
        release: Mutex::new(Some(release_rx)),
        attempts: Arc::clone(&attempts),
    }));
    let cancellation = CancellationToken::new();
    let cancel_from_thread = cancellation.clone();
    let coordinator = thread::spawn(move || {
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observe first attempt");
        cancel_from_thread.cancel();
        release_tx.send(()).expect("release first refusal");
    });
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime");

    let connected = runtime.block_on(dial_approved_addresses(
        vec![first, second],
        &connector,
        &cancellation,
        TokioInstant::now() + Duration::from_secs(1),
    ));
    coordinator.join().expect("join revocation coordinator");
    assert!(connected.is_none());
    assert_eq!(
        *attempts.lock().expect("attempt list poisoned"),
        vec![first]
    );
}

#[test]
fn upstream_proxy_refusal_falls_back_without_another_lookup() {
    let first: SocketAddr = "192.0.2.1:443".parse().expect("first target");
    let second: SocketAddr = "192.0.2.2:443".parse().expect("second target");
    let (upstream_proxy, requests_rx, server) = start_refusing_upstream();

    let (hostname_tx, hostname_rx) = mpsc::channel();
    let resolver = Arc::new(CapturingAnswerResolver {
        captured: hostname_tx,
        answers: vec![first.ip(), second.ip()],
    });
    let proxy = Proxy::start_with_test_resolver(
        ProxyConfig::default().with_upstream_proxy(upstream_proxy),
        resolver,
    )
    .expect("start proxy");
    let policy = Policy::builder()
        .allow_host("fallback.test")
        .expect("valid hostname")
        .allow_network("192.0.2.0/24".parse().expect("test network"))
        .allow_port(443)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach localhost");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect guest");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT fallback.test:443 HTTP/1.1\r\nHost: fallback.test\r\n\r\n",
    )
    .expect("write guest CONNECT");
    let mut response = [0_u8; 39];
    std::io::Read::read_exact(&mut client, &mut response).expect("read CONNECT success");
    assert_eq!(&response, CONNECT_SUCCESS_RESPONSE);
    let mut greeting = [0_u8; 5];
    std::io::Read::read_exact(&mut client, &mut greeting).expect("read greeting");
    assert_eq!(&greeting, b"hello");
    std::io::Write::write_all(&mut client, b"ping").expect("write tunnel upload");
    let mut pong = [0_u8; 4];
    std::io::Read::read_exact(&mut client, &mut pong).expect("read tunnel download");
    assert_eq!(&pong, b"pong");
    std::net::TcpStream::shutdown(&client, std::net::Shutdown::Write)
        .expect("finish tunnel upload");
    std::io::Read::read_to_end(&mut client, &mut Vec::new()).expect("read tunnel shutdown");
    server.join().expect("join upstream proxy");

    assert_eq!(
        hostname_rx.recv().expect("receive lookup"),
        "fallback.test."
    );
    assert!(hostname_rx.try_recv().is_err(), "unexpected second lookup");
    assert_eq!(
        requests_rx.recv().expect("receive CONNECT requests"),
        vec![
            format!("CONNECT {first} HTTP/1.1\r\nHost: {first}\r\n\r\n").into_bytes(),
            format!("CONNECT {second} HTTP/1.1\r\nHost: {second}\r\n\r\n").into_bytes(),
        ]
    );
    let usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("certified close")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.completed_connections, 1);
    assert_eq!(usage.denied_connections, 0);
    assert_eq!(usage.uploaded_bytes, 4);
    assert_eq!(usage.downloaded_bytes, 9);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
}

#[test]
fn dns_concurrency_is_bounded_and_queued_lookups_cancel_on_close() {
    const CLIENTS: usize = 5;
    const DNS_LIMIT: usize = 2;

    let (entered_tx, entered_rx) = mpsc::channel();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resolver = Arc::new(PendingResolver {
        entered: entered_tx,
        active: Arc::clone(&active),
    });
    let proxy = Proxy::start_with_test_resolver(
        ProxyConfig::default()
            .with_max_connections(CLIENTS)
            .with_max_concurrent_dns(DNS_LIMIT),
        resolver,
    )
    .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("pending.test", 443),
        )
        .expect("attach lease");

    let mut clients = Vec::with_capacity(CLIENTS);
    for _ in 0..CLIENTS {
        let mut client =
            std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        std::io::Write::write_all(
            &mut client,
            b"CONNECT pending.test:443 HTTP/1.1\r\nHost: pending.test\r\n\r\n",
        )
        .expect("write CONNECT");
        clients.push(client);
    }
    for _ in 0..DNS_LIMIT {
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("lookup entered");
    }
    thread::sleep(Duration::from_millis(20));
    assert!(matches!(
        entered_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(active.load(Ordering::Acquire), DNS_LIMIT);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close DNS-bound lease");
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert!(matches!(
        entered_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    drop(clients);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn dns_deadline_has_a_distinct_denial_and_never_dials() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resolver = Arc::new(PendingResolver {
        entered: entered_tx,
        active: Arc::clone(&active),
    });
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start DNS deadline proxy");
    let policy = Policy::builder()
        .allow_host("pending.test")
        .expect("valid test hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid loopback test network"))
        .allow_port(443)
        .dns_timeout(Duration::from_millis(20))
        .handshake_timeout(Duration::from_secs(1))
        .build()
        .expect("valid DNS deadline policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach DNS deadline lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT pending.test:443 HTTP/1.1\r\nHost: pending.test\r\n\r\n",
    )
    .expect("write CONNECT");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lookup entered");

    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read DNS timeout denial");
    assert!(response.starts_with("HTTP/1.1 504"), "{response}");
    assert!(response.contains("dns-timeout"), "{response}");
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    assert_eq!(lease.usage().denied_connections, 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close DNS deadline lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn dns_terminal_results_remain_distinct_and_never_dial() {
    assert_dns_terminal_denial("failed.test", Arc::new(FailingResolver), "dns-failed");
    assert_dns_terminal_denial(
        "empty.test",
        Arc::new(FixedAnswerResolver(Vec::new())),
        "dns-empty",
    );
}

#[test]
fn late_dns_answer_cannot_dial_after_lease_close() {
    let target =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind target");
    target.set_nonblocking(true).expect("nonblocking target");
    let port = target.local_addr().expect("target address").port();
    let (started_tx, started_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
    let resolver = Arc::new(LateAnswerResolver {
        started: started_tx,
        answer: Mutex::new(Some(answer_rx)),
    });
    let proxy =
        Proxy::start_with_test_resolver(ProxyConfig::default(), resolver).expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("late.test", port),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT late.test:{port} HTTP/1.1\r\nHost: late.test\r\n\r\n").as_bytes(),
    )
    .expect("write CONNECT");
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lookup entered");

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close resolving lease");
    assert!(
        answer_tx
            .send(vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)])
            .is_err(),
        "revocation must drop the late-answer receiver"
    );
    assert!(matches!(
        target.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn test_and_system_resolvers_receive_the_same_absolute_hostname() {
    let (hostname_tx, hostname_rx) = mpsc::channel();
    let resolver = Arc::new(CapturingResolver(hostname_tx));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("mixed.case.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT Mixed.Case.Test.:443 HTTP/1.1\r\nHost: mixed.case.test\r\n\r\n",
    )
    .expect("write CONNECT");

    assert_eq!(
        hostname_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("resolver hostname"),
        "mixed.case.test."
    );
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read dial denial");
    assert!(response.contains("dial-failed"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn system_resolver_uses_the_configured_cache_bounds() {
    let config = ProxyConfig::default().with_dns_cache(17, Duration::from_secs(19));
    let resolver = build_system_resolver(&config).expect("build system resolver");
    assert_eq!(resolver.options().cache_size, 17);
    assert_eq!(
        resolver.options().positive_max_ttl,
        Some(Duration::from_secs(19))
    );
    assert_eq!(
        resolver.options().negative_max_ttl,
        Some(Duration::from_secs(19))
    );
    assert!(resolver.options().try_tcp_on_error);
}

#[test]
fn explicit_dns_server_bypasses_host_configuration() {
    let (address, server) = start_local_dns(1, local_a_response);
    let config = ProxyConfig::default().with_dns_server(address);
    let resolver = build_system_resolver(&config).expect("build explicit resolver");
    assert_eq!(
        resolver.options().use_hosts_file,
        hickory_resolver::config::ResolveHosts::Never
    );
    assert!(resolver.options().try_tcp_on_error);

    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");
    runtime.block_on(async {
        let answer = resolver
            .ipv4_lookup("explicit-resolver.test.")
            .await
            .expect("resolve through configured DNS server");
        assert_eq!(answer.answers().len(), 1);
        assert_eq!(answer.answers()[0].data.to_string(), "127.0.0.1");
    });
    server.join().expect("join local DNS server");
}

#[test]
fn explicit_dns_server_retries_truncated_udp_over_tcp() {
    let (address, server) = start_truncated_udp_dns();
    let config = ProxyConfig::default().with_dns_server(address);
    let resolver = build_system_resolver(&config).expect("build explicit resolver");
    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");

    runtime.block_on(async {
        let answer = resolver
            .ipv4_lookup("tcp-fallback.test.")
            .await
            .expect("retry truncated response over TCP");
        assert_eq!(answer.answers().len(), 1);
        assert_eq!(answer.answers()[0].data.to_string(), "127.0.0.1");
    });
    server.join().expect("join local DNS server");
}

#[test]
fn lease_close_stops_real_dns_retries_after_late_failure() {
    const INITIAL_QUERIES: usize = 2;

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind late-response TCP DNS server");
    listener
        .set_nonblocking(true)
        .expect("configure late-response TCP DNS server");
    let dns_address = listener.local_addr().expect("late-response DNS address");
    let socket = std::net::UdpSocket::bind(dns_address).expect("bind late-response UDP DNS server");
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set initial DNS timeout");
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut packet = [0_u8; 2_048];
        let mut requests = Vec::with_capacity(INITIAL_QUERIES);
        for _ in 0..INITIAL_QUERIES {
            let (length, peer) = socket.recv_from(&mut packet).expect("receive DNS query");
            requests.push((packet[..length].to_vec(), peer));
        }
        ready_tx.send(()).expect("report initial DNS queries");
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release late DNS responses");
        for (query, peer) in &requests {
            socket
                .send_to(&local_servfail_response(query), peer)
                .expect("send late DNS failure");
        }

        socket
            .set_nonblocking(true)
            .expect("configure retry observation");
        let mut retries = 0;
        let observation_deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < observation_deadline {
            match socket.recv_from(&mut packet) {
                Ok(_) => retries += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("observe DNS retries: {error}"),
            }
            match listener.accept() {
                Ok(_) => retries += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("observe TCP DNS retries: {error}"),
            }
            thread::sleep(Duration::from_millis(5));
        }
        retries
    });

    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_dns_server(dns_address)
            .with_dns_cache(0, Duration::ZERO),
    )
    .expect("start explicit DNS proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("cancel-wire.test", 443),
        )
        .expect("attach DNS lease");
    let mut client = std::net::TcpStream::connect(lease.endpoint().socket_addr())
        .expect("connect explicit DNS proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT cancel-wire.test:443 HTTP/1.1\r\nHost: cancel-wire.test\r\n\r\n",
    )
    .expect("write DNS-bound CONNECT");
    ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("observe initial wire queries");

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close DNS-bound lease");
    let final_usage = final_usage.usage();
    assert_eq!(final_usage.accepted_connections, 1);
    assert_eq!(final_usage.active_connections, 0);
    release_tx.send(()).expect("release late DNS failures");
    assert_client_stopped(client);
    assert_eq!(
        server.join().expect("join late-response DNS server"),
        0,
        "cancelled lookup must not retry after a late DNS failure"
    );
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("shutdown explicit DNS proxy");
}

#[test]
fn zero_capacity_resolver_cache_requeries_local_dns() {
    let (address, server) = start_local_dns(2, local_a_response);
    let config = ProxyConfig::default().with_dns_cache(0, Duration::from_secs(60));
    let resolver = local_dns_resolver(address, &config);
    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");

    runtime.block_on(async {
        for _ in 0..2 {
            let answer = resolver
                .lookup_ip("cache-disabled.test.")
                .await
                .expect("resolve through local DNS");
            assert_eq!(
                answer.iter().collect::<Vec<_>>(),
                vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)]
            );
        }
    });
    server.join().expect("join local DNS server");
}

#[test]
fn resolver_cache_ttl_ceiling_expires_local_dns_answer() {
    let (address, server) = start_local_dns(2, local_a_response);
    let config = ProxyConfig::default().with_dns_cache(8, Duration::from_secs(1));
    let resolver = local_dns_resolver(address, &config);
    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");

    runtime.block_on(async {
        let first = resolver
            .lookup_ip("cache-expiry.test.")
            .await
            .expect("resolve initial local answer");
        let cached = resolver
            .lookup_ip("cache-expiry.test.")
            .await
            .expect("resolve cached local answer");
        assert_eq!(
            first.iter().collect::<Vec<_>>(),
            cached.iter().collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let expired = resolver
            .lookup_ip("cache-expiry.test.")
            .await
            .expect("resolve expired local answer again");
        assert_eq!(
            first.iter().collect::<Vec<_>>(),
            expired.iter().collect::<Vec<_>>()
        );
    });
    server.join().expect("join local DNS server");
}

#[test]
fn resolver_cache_ttl_ceiling_expires_local_nxdomain() {
    let (address, server) = start_local_dns(2, local_nxdomain_response);
    let config = ProxyConfig::default().with_dns_cache(8, Duration::from_secs(1));
    let resolver = local_dns_resolver(address, &config);
    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");

    runtime.block_on(async {
        resolver
            .lookup_ip("negative-cache.test.")
            .await
            .expect_err("initial NXDOMAIN must fail");
        resolver
            .lookup_ip("negative-cache.test.")
            .await
            .expect_err("cached NXDOMAIN must fail");
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        resolver
            .lookup_ip("negative-cache.test.")
            .await
            .expect_err("expired NXDOMAIN must be queried again");
    });
    server.join().expect("join local DNS server");
}

#[test]
fn resolver_answers_are_rechecked_after_identity_reuse() {
    let resolver = Arc::new(FixedAnswerResolver(vec![IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    )]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(
        ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO),
        resolver,
        connector,
    )
    .expect("start identity-reuse proxy");
    let identity = PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let first_policy = Policy::builder()
        .allow_host("reused.test")
        .expect("valid hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid loopback network"))
        .allow_port(443)
        .build()
        .expect("valid first policy");
    let first = proxy
        .attach(identity.clone(), first_policy)
        .expect("attach first lease");
    let endpoint = first.endpoint().socket_addr();
    let mut first_client = std::net::TcpStream::connect(endpoint).expect("connect first lease");
    std::io::Write::write_all(
        &mut first_client,
        b"CONNECT reused.test:443 HTTP/1.1\r\nHost: reused.test\r\n\r\n",
    )
    .expect("write first CONNECT");
    let mut first_response = String::new();
    std::io::Read::read_to_string(&mut first_client, &mut first_response)
        .expect("read first denial");
    assert!(first_response.contains("dial-failed"), "{first_response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);
    first
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close first lease");

    let second_policy = Policy::builder()
        .allow_host("reused.test")
        .expect("valid hostname")
        .allow_port(443)
        .build()
        .expect("valid second policy");
    let second = proxy
        .attach(identity, second_policy)
        .expect("attach second lease");
    let mut second_client = std::net::TcpStream::connect(endpoint).expect("connect second lease");
    std::io::Write::write_all(
        &mut second_client,
        b"CONNECT reused.test:443 HTTP/1.1\r\nHost: reused.test\r\n\r\n",
    )
    .expect("write second CONNECT");
    let mut second_response = String::new();
    std::io::Read::read_to_string(&mut second_client, &mut second_response)
        .expect("read second denial");
    assert!(
        second_response.contains("resolved-address-denied"),
        "{second_response}"
    );
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);
    second
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close second lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn explicit_public_network_denial_blocks_dns_and_literal_paths_before_dial() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "93.184.216.34".parse().expect("public test address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let policy = Policy::builder()
        .allow_host("blocked-public.test")
        .expect("valid test hostname")
        .allow_network("0.0.0.0/0".parse().expect("valid catch-all grant"))
        .deny_network(
            "93.184.216.0/24"
                .parse()
                .expect("valid public denial network"),
        )
        .allow_port(443)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");

    for (authority, reason) in [
        ("blocked-public.test", "resolved-address-denied"),
        ("93.184.216.34", "ip-literal-denied"),
    ] {
        let mut client =
            std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        std::io::Write::write_all(
            &mut client,
            format!("CONNECT {authority}:443 HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes(),
        )
        .expect("write CONNECT");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut client, &mut response).expect("read address denial");
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(response.contains(reason), "{response}");
    }

    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 2);
    assert_eq!(usage.denied_connections, 2);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn azure_wireserver_dns_answer_is_rejected_before_dial() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "168.63.129.16".parse().expect("Azure WireServer address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("wireserver.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT wireserver.test:443 HTTP/1.1\r\nHost: wireserver.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read address denial");

    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("resolved-address-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn hostname_resolving_to_proxy_listener_is_rejected_before_dial() {
    let resolver = Arc::new(FixedAnswerResolver(vec![IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    )]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let endpoint = proxy.endpoint().socket_addr();
    let policy = Policy::builder()
        .allow_host("self.test")
        .expect("valid hostname")
        .allow_network("127.0.0.0/8".parse().expect("loopback grant"))
        .allow_port(endpoint.port())
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");
    let mut client = std::net::TcpStream::connect(endpoint).expect("connect proxy listener");
    std::io::Write::write_all(
        &mut client,
        format!(
            "CONNECT self.test:{} HTTP/1.1\r\nHost: self.test\r\n\r\n",
            endpoint.port()
        )
        .as_bytes(),
    )
    .expect("write self-directed hostname CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read proxy endpoint denial");

    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("proxy-endpoint-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn wildcard_listener_rejects_every_same_port_destination_before_dial() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "93.184.216.34".parse().expect("public test address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(
        ProxyConfig::default().with_bind_address("[::]:0".parse().expect("wildcard bind")),
        resolver,
        connector,
    )
    .expect("start wildcard proxy");
    let port = proxy.endpoint().socket_addr().port();
    let policy = Policy::builder()
        .allow_host("same-port.test")
        .expect("valid hostname")
        .allow_port(port)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");
    let mut client = std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .expect("connect wildcard listener");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT same-port.test:{port} HTTP/1.1\r\nHost: same-port.test\r\n\r\n")
            .as_bytes(),
    )
    .expect("write same-port CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response)
        .expect("read wildcard endpoint denial");

    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("proxy-endpoint-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn proxy_endpoint_matching_covers_transport_spellings_and_wildcard_port() {
    let endpoint: SocketAddr = "127.0.0.1:4750".parse().expect("IPv4 endpoint");
    assert!(is_proxy_endpoint(endpoint, endpoint));
    assert!(is_proxy_endpoint(
        "[::ffff:127.0.0.1]:4750".parse().expect("mapped endpoint"),
        endpoint,
    ));
    assert!(is_proxy_endpoint(
        "127.0.0.1:4750".parse().expect("IPv4 loopback"),
        "[::]:4750".parse().expect("dual-stack wildcard"),
    ));
    assert!(is_proxy_endpoint(
        "127.0.0.1:4750".parse().expect("IPv4 loopback"),
        "0.0.0.0:4750".parse().expect("IPv4 wildcard"),
    ));
    assert!(is_proxy_endpoint(
        "93.184.216.34:4750".parse().expect("remote endpoint"),
        "[::]:4750".parse().expect("dual-stack wildcard"),
    ));
    assert!(is_proxy_endpoint(
        "10.0.0.8:4750"
            .parse()
            .expect("private interface candidate"),
        "0.0.0.0:4750".parse().expect("IPv4 wildcard"),
    ));
    assert!(!is_proxy_endpoint(
        "127.0.0.1:4751".parse().expect("different port"),
        endpoint,
    ));
}

#[test]
fn failed_startup_joins_the_owned_runtime_thread() {
    let reservation =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("reserve listener");
    let address = reservation.local_addr().expect("reserved address");
    drop(reservation);
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let result = Proxy::start_with_test_resolver(
        ProxyConfig::default()
            .with_bind_address(address)
            .with_upstream_proxy(address),
        Arc::new(StartupDropProbe(Arc::clone(&dropped))),
    );

    assert!(result.is_err(), "self-referencing startup succeeded");
    assert!(
        dropped.load(Ordering::Acquire),
        "startup returned before its runtime-owned resolver was dropped"
    );
}

#[test]
fn explicit_hostname_denial_stops_before_dns_and_dial() {
    let (resolved_tx, resolved_rx) = mpsc::channel();
    let resolver = Arc::new(CapturingResolver(resolved_tx));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let policy = Policy::builder()
        .allow_host("*.example.test")
        .expect("valid wildcard grant")
        .deny_host("admin.example.test")
        .expect("valid hostname denial")
        .deny_host("*.internal.example.test")
        .expect("valid wildcard denial")
        .allow_port(443)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");
    for authority in [
        "AdMiN.ExAmPlE.TeSt.:443",
        "deep.secret.internal.example.test:443",
    ] {
        let mut client =
            std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        std::io::Write::write_all(&mut client, request.as_bytes()).expect("write CONNECT");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut client, &mut response).expect("read hostname denial");

        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(response.contains("host-denied"), "{response}");
    }
    assert!(matches!(
        resolved_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 2);
    assert_eq!(usage.denied_connections, 2);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn dns_answer_with_ipv4_compatible_metadata_address_is_rejected_as_a_set() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "93.184.216.34".parse().expect("public test address"),
        "::169.254.169.254"
            .parse()
            .expect("compatible metadata address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("mixed.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT mixed.test:443 HTTP/1.1\r\nHost: mixed.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read DNS denial");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("resolved-address-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    assert_eq!(lease.usage().denied_connections, 1);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn legacy_numeric_spellings_cannot_bypass_the_system_resolver_address_floor() {
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for hostname in ["127.1", "0177.0.0.1", "0x7f000001", "2130706433"] {
        assert!(hostname.parse::<IpAddr>().is_err(), "{hostname}");
        let (dns_address, dns_server) = start_local_dns(2, local_a_response);
        let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
        let proxy = Proxy::start_with_test_connector(
            ProxyConfig::default().with_dns_server(dns_address),
            connector,
        )
        .expect("start explicit-DNS proxy");
        let lease = proxy
            .attach(
                PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
                Policy::builder()
                    .allow_host(hostname)
                    .expect("valid legacy-looking hostname")
                    .allow_port(443)
                    .build()
                    .expect("valid policy"),
            )
            .expect("attach lease");
        let mut client =
            std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        std::io::Write::write_all(
            &mut client,
            format!("CONNECT {hostname}:443 HTTP/1.1\r\nHost: {hostname}\r\n\r\n").as_bytes(),
        )
        .expect("write CONNECT");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut client, &mut response).expect("read address denial");
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "{hostname}: {response}"
        );
        assert!(
            response.contains("resolved-address-denied"),
            "{hostname}: {response}"
        );
        assert_eq!(dial_attempts.load(Ordering::Acquire), 0, "{hostname}");

        lease
            .close(Instant::now() + Duration::from_secs(1))
            .expect("close lease");
        proxy
            .shutdown(Instant::now() + Duration::from_secs(1))
            .expect("proxy shutdown");
        dns_server.join().expect("join explicit DNS server");
    }
}

#[test]
fn configured_nat64_prefix_rejects_an_embedded_metadata_destination() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "2600:1f18:abcd:1234::a9fe:a9fe"
            .parse()
            .expect("network-specific NAT64 metadata address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let config = ProxyConfig::default().with_nat64_prefix(
        "2600:1f18:abcd:1234::/96"
            .parse()
            .expect("network-specific NAT64 prefix"),
    );
    let proxy = Proxy::start_with_test_backends(config, resolver, connector)
        .expect("start NAT64-aware proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("nat64.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT nat64.test:443 HTTP/1.1\r\nHost: nat64.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read NAT64 denial");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("resolved-address-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn oversized_dns_answer_is_rejected_before_any_dial() {
    let loopback = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    let resolver = Arc::new(FixedAnswerResolver(vec![loopback; 65]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("large-answer.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT large-answer.test:443 HTTP/1.1\r\nHost: large-answer.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read DNS denial");
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert!(response.contains("dns-answer-too-large"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    assert_eq!(lease.usage().denied_connections, 1);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn duplicate_dns_answers_produce_one_dial_attempt() {
    let loopback = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    let resolver = Arc::new(FixedAnswerResolver(vec![loopback; 64]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("duplicate-answer.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT duplicate-answer.test:443 HTTP/1.1\r\nHost: duplicate-answer.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read dial denial");
    assert!(response.contains("dial-failed"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn equivalent_dns_address_spellings_produce_one_dial_attempt() {
    let ipv4 = std::net::Ipv4Addr::LOCALHOST;
    let mapped = IpAddr::V6(ipv4.to_ipv6_mapped());
    let resolver = Arc::new(FixedAnswerResolver(vec![IpAddr::V4(ipv4), mapped]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let policy = Policy::builder()
        .allow_host("equivalent-answer.test")
        .expect("valid hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid IPv4 loopback grant"))
        .allow_network(
            "::ffff:127.0.0.1/128"
                .parse()
                .expect("valid mapped loopback grant"),
        )
        .allow_port(443)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT equivalent-answer.test:443 HTTP/1.1\r\nHost: equivalent-answer.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read dial denial");
    assert!(response.contains("dial-failed"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn close_cancels_an_in_progress_dial() {
    let port = 19_443;
    let (entered_tx, entered_rx) = mpsc::channel();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(PendingConnector {
        entered: entered_tx,
        active: Arc::clone(&active),
    });
    let proxy =
        Proxy::start_with_test_connector(ProxyConfig::default(), connector).expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            ip_policy(port, Duration::from_secs(2)),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
    )
    .expect("write CONNECT");

    assert_eq!(
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("dial entered"),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)
    );
    assert_eq!(active.load(Ordering::Acquire), 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close dialing lease");
    assert_eq!(active.load(Ordering::Acquire), 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn failed_proxy_shutdown_retains_a_stopping_proxy_for_retry() {
    let port = 19_445;
    let (entered_tx, entered_rx) = mpsc::channel();
    let connector = Arc::new(SlowCancelConnector {
        entered: entered_tx,
        cleanup_delay: Duration::from_millis(150),
    });
    let proxy =
        Proxy::start_with_test_connector(ProxyConfig::default(), connector).expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            ip_policy(port, Duration::from_secs(2)),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
    )
    .expect("write CONNECT");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("dial entered");

    let error = proxy
        .shutdown(Instant::now() + Duration::from_millis(20))
        .expect_err("slow cancellation must exceed the first deadline");
    assert_eq!(error.kind(), crate::ShutdownErrorKind::DeadlineExceeded);
    let proxy = error.into_proxy();
    assert!(matches!(
        proxy.attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2))),
            Policy::builder().build().expect("valid policy"),
        ),
        Err(AttachError::ProxyStopping)
    ));

    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("retry proxy shutdown");
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("observe proxy-wide certificate")
        .usage();
    assert_eq!(usage.active_connections, 0);
}

#[test]
fn proxy_drop_racing_lease_close_preserves_the_certificate() {
    let PendingDialFixture {
        mut proxy,
        lease,
        client,
        active,
    } = pending_dial_fixture(19_446);

    let runtime = proxy.thread.take().expect("runtime handle");
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let proxy_barrier = Arc::clone(&barrier);
    let proxy_drop = thread::spawn(move || {
        proxy_barrier.wait();
        drop(proxy);
    });
    let lease_barrier = Arc::clone(&barrier);
    let lease_close = thread::spawn(move || {
        lease_barrier.wait();
        lease.close(Instant::now() + Duration::from_secs(2))
    });

    barrier.wait();
    proxy_drop.join().expect("drop proxy");
    let usage = lease_close
        .join()
        .expect("join lease close")
        .expect("certified lease close")
        .usage();
    runtime.join().expect("join runtime");

    assert_eq!(usage.active_connections, 0);
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert_client_stopped(client);
}

#[test]
fn proxy_drop_racing_lease_drop_releases_all_ownership() {
    let PendingDialFixture {
        mut proxy,
        lease,
        client,
        active,
    } = pending_dial_fixture(19_447);
    let state = Arc::downgrade(lease.state.as_ref().expect("lease state"));

    let runtime = proxy.thread.take().expect("runtime handle");
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let proxy_barrier = Arc::clone(&barrier);
    let proxy_drop = thread::spawn(move || {
        proxy_barrier.wait();
        drop(proxy);
    });
    let lease_barrier = Arc::clone(&barrier);
    let lease_drop = thread::spawn(move || {
        lease_barrier.wait();
        drop(lease);
    });

    barrier.wait();
    proxy_drop.join().expect("drop proxy");
    lease_drop.join().expect("drop lease");
    runtime.join().expect("join runtime");

    assert_eq!(active.load(Ordering::Acquire), 0);
    assert!(state.upgrade().is_none());
    assert_client_stopped(client);
}

#[test]
fn proxy_shutdown_racing_lease_close_preserves_both_certificates() {
    let PendingDialFixture {
        proxy,
        lease,
        client,
        active,
    } = pending_dial_fixture(19_448);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let proxy_barrier = Arc::clone(&barrier);
    let proxy_shutdown = thread::spawn(move || {
        proxy_barrier.wait();
        proxy.shutdown(Instant::now() + Duration::from_secs(2))
    });
    let lease_barrier = Arc::clone(&barrier);
    let lease_close = thread::spawn(move || {
        lease_barrier.wait();
        lease.close(Instant::now() + Duration::from_secs(2))
    });

    barrier.wait();
    proxy_shutdown
        .join()
        .expect("join proxy shutdown")
        .expect("certified proxy shutdown");
    let usage = lease_close
        .join()
        .expect("join lease close")
        .expect("certified lease close")
        .usage();

    assert_eq!(usage.active_connections, 0);
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert_client_stopped(client);
}

#[test]
fn proxy_shutdown_racing_lease_drop_releases_all_ownership() {
    let PendingDialFixture {
        proxy,
        lease,
        client,
        active,
    } = pending_dial_fixture(19_449);
    let state = Arc::downgrade(lease.state.as_ref().expect("lease state"));
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let proxy_barrier = Arc::clone(&barrier);
    let proxy_shutdown = thread::spawn(move || {
        proxy_barrier.wait();
        proxy.shutdown(Instant::now() + Duration::from_secs(2))
    });
    let lease_barrier = Arc::clone(&barrier);
    let lease_drop = thread::spawn(move || {
        lease_barrier.wait();
        drop(lease);
    });

    barrier.wait();
    proxy_shutdown
        .join()
        .expect("join proxy shutdown")
        .expect("certified proxy shutdown");
    lease_drop.join().expect("drop lease");

    assert_eq!(active.load(Ordering::Acquire), 0);
    assert!(state.upgrade().is_none());
    assert_client_stopped(client);
}

#[test]
fn unobserved_proxy_shutdown_success_remains_retryable() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let (reply, receiver) = mpsc::sync_channel(0);
    drop(receiver);
    proxy
        .commands
        .send(Command::Shutdown {
            deadline: Instant::now() + Duration::from_secs(1),
            reply,
            retryable: true,
        })
        .expect("request abandoned shutdown");

    assert!(matches!(
        proxy.attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        ),
        Err(AttachError::ProxyStopping)
    ));
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("retry unobserved shutdown");
}

#[test]
fn absolute_handshake_deadline_cancels_an_in_progress_dial() {
    let port = 19_444;
    let (entered_tx, entered_rx) = mpsc::channel();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(PendingConnector {
        entered: entered_tx,
        active: Arc::clone(&active),
    });
    let proxy =
        Proxy::start_with_test_connector(ProxyConfig::default(), connector).expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            ip_policy(port, Duration::from_millis(50)),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
    )
    .expect("write CONNECT");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("dial entered");

    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read dial denial");
    assert!(
        response.is_empty(),
        "expired denial wrote bytes: {response}"
    );
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert_eq!(lease.usage().denied_connections, 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close timed-out lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn absolute_handshake_deadline_cancels_buffered_upload_forwarding() {
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let (mut upstream, _blocked_reader) = tokio::io::duplex(1);
        upstream.write_all(b"x").await.expect("fill upstream");
        let state = LeaseState::new(
            1,
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
            DiagnosticReporter::default(),
        );

        let result = forward_uninspected_upload(
            &mut upstream,
            &state,
            b"buffered tunnel bytes",
            TokioInstant::now() + Duration::from_millis(20),
        )
        .await;

        assert_eq!(result, Err("initial-upload-timeout"));
        assert_eq!(state.counters.snapshot().uploaded_bytes, 21);

        let (mut failed_upstream, failed_peer) = tokio::io::duplex(1);
        drop(failed_peer);
        let result = forward_uninspected_upload(
            &mut failed_upstream,
            &state,
            b"x",
            TokioInstant::now() + Duration::from_secs(1),
        )
        .await;
        assert_eq!(result, Err("upstream-write-failed"));
        assert_eq!(state.counters.snapshot().uploaded_bytes, 22);
    });
}

#[test]
fn handshake_deadline_includes_time_before_connection_task_starts() {
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind test listener");
        let endpoint = listener.local_addr().expect("test listener address");
        let connect = TcpStream::connect(endpoint);
        let accept = listener.accept();
        let (client, accepted) = tokio::join!(connect, accept);
        let mut client = client.expect("connect test client");
        let (server, _) = accepted.expect("accept test client");
        let state = Arc::new(LeaseState::new(
            1,
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Policy::builder()
                .dns_timeout(Duration::from_millis(10))
                .handshake_timeout(Duration::from_millis(10))
                .build()
                .expect("valid deadline policy"),
            DiagnosticReporter::default(),
        ));
        let resolver = ResolverBackend::Test(Arc::new(FixedAnswerResolver(Vec::new())));
        let phase_permits = PhasePermits {
            dns: Semaphore::new(1),
            dial: Semaphore::new(1),
        };
        let config = ProxyConfig::default();

        let disposition = serve_connect(
            server,
            &state,
            &resolver,
            &phase_permits,
            &ConnectorBackend::Direct,
            &config,
            TokioInstant::now() - Duration::from_millis(20),
        )
        .await
        .expect("write deadline denial");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .await
            .expect("read deadline denial");

        assert_eq!(disposition, ConnectionDisposition::Denied);
        assert!(
            response.is_empty(),
            "expired denial wrote bytes: {response}"
        );
        assert_eq!(state.counters.snapshot().denied_connections, 1);
    });
}

#[test]
fn cleanup_readiness_does_not_release_identity_before_success_is_observed() {
    let state = Arc::new(LeaseState::new(
        1,
        PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        Policy::builder().build().expect("valid policy"),
        DiagnosticReporter::default(),
    ));
    state.begin_close();

    let runtime = RuntimeBuilder::new_multi_thread()
        .worker_threads(1)
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime");
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    {
        let _runtime_guard = runtime.enter();
        spawn_close_wait(
            Arc::clone(&state),
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
            reply_tx,
            None,
        );
    }
    let usage = reply_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("cleanup reply")
        .expect("cleanup ready");

    assert_eq!(usage.usage().active_connections, 0);
    assert_eq!(*state.phase.lock().expect("lease phase"), Phase::Quiesced);
    assert!(
        !state.is_closed(),
        "cleanup alone must retain identity ownership"
    );
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind late client test");
    let mut client = std::net::TcpStream::connect(listener.local_addr().expect("test address"))
        .expect("connect late client test");
    let (server, _) = listener.accept().expect("accept late client test");
    server.set_nonblocking(true).expect("nonblocking server");
    let server = {
        let _runtime_guard = runtime.enter();
        TcpStream::from_std(server).expect("Tokio late client stream")
    };
    state.reject_unadmitted(server, "late-test-connection");
    assert_eq!(state.counters.snapshot(), usage.usage());
    let mut byte = [0];
    assert!(matches!(std::io::Read::read(&mut client, &mut byte), Ok(0)));

    let (retry_tx, retry_rx) = mpsc::sync_channel(1);
    {
        let _runtime_guard = runtime.enter();
        spawn_close_wait(
            Arc::clone(&state),
            Duration::from_secs(1),
            Instant::now() + Duration::from_millis(50),
            retry_tx,
            None,
        );
    }
    let retried = retry_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("retry reply")
        .expect("quiesced cleanup is already ready");
    assert_eq!(retried, usage);
    assert_eq!(*state.phase.lock().expect("lease phase"), Phase::Quiesced);
    state.mark_closed();
    assert!(state.is_closed());
}

#[test]
fn quiesced_close_retry_still_requests_a_fresh_accept_drain() {
    let state = LeaseState::new(
        1,
        PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        Policy::builder().build().expect("valid policy"),
        DiagnosticReporter::default(),
    );
    state.begin_close();
    let expected = state.quiesce_if_generation(0).expect("mark cleanup ready");
    let (commands, mut receiver) = tokio_mpsc::unbounded_channel();
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime");

    let actual = runtime.block_on(async {
        let close = quiesce_after_identity_quiet(&state, Duration::from_secs(1), Some(&commands));
        let drain = async {
            let Command::DrainAcceptQueue { reply } =
                receiver.recv().await.expect("accept-drain command")
            else {
                panic!("unexpected command while retrying close");
            };
            reply.send(Ok(())).expect("acknowledge accept drain");
        };
        let (usage, ()) = tokio::join!(close, drain);
        usage.expect("quiesced retry")
    });

    assert_eq!(actual, expected);
}

#[test]
fn accept_retry_backoff_is_bounded_and_resets_after_success() {
    let mut backoff = AcceptBackoff::default();
    assert_eq!(backoff.next_delay(), ACCEPT_RETRY_INITIAL);
    assert_eq!(backoff.next_delay(), ACCEPT_RETRY_INITIAL * 2);
    for _ in 0..16 {
        backoff.next_delay();
    }
    assert_eq!(backoff.next_delay(), ACCEPT_RETRY_MAX);

    backoff.recover();
    assert_eq!(backoff.next_delay(), ACCEPT_RETRY_INITIAL);
}

#[test]
fn absolute_header_deadline_ignores_continuous_activity() {
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&sent);
        let writer = tokio::spawn(async move {
            loop {
                if writer.write_all(b"x").await.is_err() {
                    break;
                }
                observed.fetch_add(1, Ordering::Relaxed);
                sleep(Duration::from_millis(1)).await;
            }
        });

        let Err(denial) = read_connect_header(
            &mut reader,
            4096,
            TokioInstant::now() + Duration::from_millis(50),
        )
        .await
        else {
            panic!("continuous reads must not renew the absolute deadline");
        };

        assert_eq!(denial.status, 408);
        assert_eq!(denial.reason, "header-timeout");
        assert!(sent.load(Ordering::Relaxed) > 1);
        drop(reader);
        writer.await.expect("join trickle writer");
    });
}

#[test]
fn header_transport_failure_has_a_distinct_denial() {
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime");
    let mut reader = BoundaryReader {
        step: 0,
        extra_before_error: false,
    };
    let Err(denial) = runtime.block_on(read_connect_header(
        &mut reader,
        4_096,
        TokioInstant::now() + Duration::from_secs(1),
    )) else {
        panic!("transport failure must be denied");
    };
    assert_eq!(denial.status, 400);
    assert_eq!(denial.reason, "header-read-failed");
}

#[test]
fn revoking_arrival_restarts_the_identity_quiet_period() {
    let state = Arc::new(LeaseState::new(
        1,
        PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        Policy::builder().build().expect("valid policy"),
        DiagnosticReporter::default(),
    ));
    state.begin_close();

    let runtime = RuntimeBuilder::new_multi_thread()
        .worker_threads(1)
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime");
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    {
        let _runtime_guard = runtime.enter();
        spawn_close_wait(
            Arc::clone(&state),
            Duration::from_millis(200),
            Instant::now() + Duration::from_secs(1),
            reply_tx,
            None,
        );
    }

    thread::sleep(Duration::from_millis(100));
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind revoking-arrival test");
    let mut client = std::net::TcpStream::connect(listener.local_addr().expect("test address"))
        .expect("connect revoking-arrival test");
    let (server, _) = listener.accept().expect("accept revoking-arrival test");
    server.set_nonblocking(true).expect("nonblocking server");
    let server = {
        let _runtime_guard = runtime.enter();
        TcpStream::from_std(server).expect("Tokio revoking client stream")
    };
    assert!(
        state.admit(server).is_none(),
        "a revoking lease must reject the queued socket"
    );

    assert!(matches!(
        reply_rx.recv_timeout(Duration::from_millis(150)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    let usage = reply_rx
        .recv_timeout(Duration::from_millis(200))
        .expect("quiet-period reply")
        .expect("cleanup after a complete quiet period");
    assert_eq!(usage.usage().denied_connections, 1);
    assert_eq!(*state.phase.lock().expect("lease phase"), Phase::Quiesced);
    let mut byte = [0];
    assert!(matches!(std::io::Read::read(&mut client, &mut byte), Ok(0)));
}

#[test]
fn successful_close_releases_the_registry_reference() {
    let proxy =
        Proxy::start(ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO))
            .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        )
        .expect("attach lease");
    let state = Arc::clone(lease.state.as_ref().expect("lease state"));

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    let deadline = Instant::now() + Duration::from_secs(1);
    while Arc::strong_count(&state) > 1 && Instant::now() < deadline {
        thread::yield_now();
    }

    assert_eq!(Arc::strong_count(&state), 1);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn dropped_lease_eventually_releases_the_registry_reference() {
    let proxy =
        Proxy::start(ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO))
            .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        )
        .expect("attach lease");
    let state = Arc::clone(lease.state.as_ref().expect("lease state"));

    drop(lease);
    let deadline = Instant::now() + Duration::from_secs(1);
    while Arc::strong_count(&state) > 1 && Instant::now() < deadline {
        thread::yield_now();
    }

    assert_eq!(Arc::strong_count(&state), 1);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn lease_drop_during_unwind_cancels_work_and_allows_identity_reuse() {
    let PendingDialFixture {
        proxy,
        lease,
        client,
        active,
    } = pending_dial_fixture(19_450);
    let identity = lease.state.as_ref().expect("lease state").identity.clone();
    let state = Arc::downgrade(lease.state.as_ref().expect("lease state"));

    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _lease_dropped_during_unwind = lease;
        panic!("intentional lease-owner unwind");
    }));
    assert!(unwind.is_err());

    let deadline = Instant::now() + Duration::from_secs(1);
    while active.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
        thread::yield_now();
    }
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert_client_stopped(client);

    let replacement_policy = Policy::builder().build().expect("replacement policy");
    let replacement = loop {
        match proxy.attach(identity.clone(), replacement_policy.clone()) {
            Ok(lease) => break lease,
            Err(AttachError::IdentityInUse) if Instant::now() < deadline => {
                thread::yield_now();
            }
            result => panic!("identity did not recover after unwind: {result:?}"),
        }
    };
    let release_deadline = Instant::now() + Duration::from_secs(1);
    while state.strong_count() != 0 && Instant::now() < release_deadline {
        thread::yield_now();
    }
    assert_eq!(state.strong_count(), 0);
    replacement
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close replacement lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn lease_drop_after_runtime_stop_releases_local_ownership() {
    let mut proxy =
        Proxy::start(ProxyConfig::default()).expect("start proxy for runtime-stop drop");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        )
        .expect("attach lease");
    let state = Arc::downgrade(lease.state.as_ref().expect("lease state"));
    let runtime = proxy.thread.take().expect("runtime handle");
    let (reply, receiver) = mpsc::sync_channel(1);
    drop(receiver);
    proxy
        .commands
        .send(Command::Shutdown {
            deadline: Instant::now() + Duration::from_secs(1),
            reply,
            retryable: false,
        })
        .expect("stop runtime");
    runtime.join().expect("join stopped runtime");

    drop(lease);
    assert!(state.upgrade().is_none());
    drop(proxy);
}

#[test]
fn delayed_release_cannot_remove_a_replacement_lease() {
    let identity = PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let old = Arc::new(LeaseState::new(
        1,
        identity.clone(),
        Policy::builder().build().expect("old policy"),
        DiagnosticReporter::default(),
    ));
    let replacement = Arc::new(LeaseState::new(
        2,
        identity.clone(),
        Policy::builder().build().expect("replacement policy"),
        DiagnosticReporter::default(),
    ));
    let mut leases = HashMap::from([(identity.clone(), Arc::clone(&replacement))]);

    release_if_current(&mut leases, &old);

    let retained = leases.get(&identity).expect("replacement retained");
    assert!(Arc::ptr_eq(retained, &replacement));
}

#[test]
fn queued_old_socket_cannot_inherit_replacement_policy_under_command_pressure() {
    let target = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind replacement-policy target");
    let port = target
        .local_addr()
        .expect("replacement-policy target address")
        .port();
    let proxy = Proxy::start(
        ProxyConfig::default().with_identity_reuse_quiet_period(Duration::from_millis(100)),
    )
    .expect("start proxy");
    let identity = PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let lease = proxy
        .attach(
            identity.clone(),
            Policy::builder().build().expect("deny-all old policy"),
        )
        .expect("attach old lease");
    let endpoint = lease.endpoint().socket_addr();

    let (started_tx, started_rx) = mpsc::sync_channel(1);
    proxy
        .commands
        .send(Command::KeepCommandsReady {
            until: Instant::now() + Duration::from_secs(1),
            started: Some(started_tx),
        })
        .expect("start command pressure");
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("command pressure started");

    let mut old_client =
        std::net::TcpStream::connect(endpoint).expect("queue old-source connection");
    old_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("old-source read timeout");
    std::io::Write::write_all(
        &mut old_client,
        format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes(),
    )
    .expect("write old-source CONNECT");

    let close = thread::spawn(move || {
        lease
            .close(Instant::now() + Duration::from_secs(3))
            .expect("close old lease")
    });

    let old_usage = close.join().expect("close thread").usage();
    let replacement = proxy
        .attach(identity, ip_policy(port, Duration::from_secs(1)))
        .expect("attach replacement lease");
    let mut response = [0_u8; 64];
    let read = std::io::Read::read(&mut old_client, &mut response);
    let terminal = match &read {
        Ok(0) => true,
        Err(error) => matches!(
            error.kind(),
            io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::NotConnected
        ),
        _ => false,
    };
    assert!(
        terminal,
        "queued old-source socket reached the replacement policy: {read:?} {:?}",
        String::from_utf8_lossy(&response)
    );
    assert_eq!(old_usage.denied_connections, 1);

    replacement
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close replacement lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn exhausted_lease_sequence_fails_closed_instead_of_wrapping() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    proxy.next_lease_id.store(u64::MAX, Ordering::Relaxed);

    assert!(matches!(
        proxy.attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        ),
        Err(AttachError::LeaseIdExhausted)
    ));
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn header_terminator_survives_each_read_boundary_split() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    for start in 4_092..=4_096 {
        let mut wire = vec![b'a'; start];
        wire.extend_from_slice(b"\r\n\r\nfollowing");
        let mut input = wire.as_slice();
        let header = runtime
            .block_on(read_bounded_header::<4_096, _>(&mut input, 8_192))
            .expect("boundary-spanning terminator");

        assert_eq!(header.end, start + 4);
        let mut following = header.bytes[header.end..].to_vec();
        following.extend_from_slice(input);
        assert_eq!(following, b"following", "split at byte {start}");
    }
}

#[test]
fn header_byte_limit_accepts_exactly_bounded_terminator() {
    const LIMIT: usize = 1_024;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");

    let mut exact = vec![b'a'; LIMIT - 4];
    exact.extend_from_slice(b"\r\n\r\n");
    let header = runtime
        .block_on(read_bounded_header::<4_096, _>(
            &mut exact.as_slice(),
            LIMIT,
        ))
        .expect("terminator ending at the byte limit");
    assert_eq!(header.end, LIMIT);

    let mut over = vec![b'a'; LIMIT - 3];
    over.extend_from_slice(b"\r\n\r\n");
    let Err(error) = runtime.block_on(read_bounded_header::<4_096, _>(&mut over.as_slice(), LIMIT))
    else {
        panic!("accepted terminator ending beyond the byte limit");
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn unrepresentable_idle_deadline_remains_cancellable_without_panicking() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let (activity, observed) = watch::channel(TokioInstant::now());
        let waiter = tokio::spawn(wait_for_tunnel_idle(observed, Duration::MAX));

        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "idle waiter panicked on overflow");
        activity
            .send(TokioInstant::now())
            .expect("advance activity timestamp");
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "idle waiter panicked after activity");

        waiter.abort();
        assert!(
            waiter
                .await
                .expect_err("aborted idle waiter")
                .is_cancelled()
        );
    });
}
