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

mod routing;

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
