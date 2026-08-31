use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use hickory_resolver::TokioResolver;
use http::uri::Authority;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::sync::{Semaphore, mpsc as tokio_mpsc};
use tokio::time::{Instant as TokioInstant, sleep, timeout_at};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

use crate::policy::canonical_hostname;
use crate::usage::Counters;
use crate::{
    AttachError, CloseError, CloseErrorKind, Endpoint, FinalUsage, PeerIdentity, Policy,
    ProxyConfig, ProxyError, Usage,
};

/// Shared proxy listener and synchronous management handle.
pub struct Proxy {
    endpoint: Endpoint,
    commands: tokio_mpsc::UnboundedSender<Command>,
    thread: Option<thread::JoinHandle<()>>,
    next_lease_id: AtomicU64,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Proxy")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl Proxy {
    /// Start the listener and its owned asynchronous runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime thread, resolver, or listener cannot
    /// be initialized.
    pub fn start(config: ProxyConfig) -> Result<Self, ProxyError> {
        let (commands, receiver) = tokio_mpsc::unbounded_channel();
        let runtime_commands = commands.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("sandbox-egress-runtime".to_owned())
            .spawn(move || {
                let runtime = RuntimeBuilder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("sandbox-egress-worker")
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        runtime.block_on(run_proxy(config, receiver, runtime_commands, ready_tx));
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("runtime creation failed: {error}")));
                    }
                }
            })
            .map_err(|error| ProxyError::Initialization(error.to_string()))?;

        let endpoint = ready_rx
            .recv()
            .map_err(|_| ProxyError::RuntimeStopped)?
            .map_err(ProxyError::Initialization)?;
        Ok(Self {
            endpoint,
            commands,
            thread: Some(thread),
            next_lease_id: AtomicU64::new(1),
        })
    }

    /// Return the listener endpoint shared by all leases.
    pub const fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// Attach an immutable policy to one host-authenticated peer identity.
    ///
    /// # Errors
    ///
    /// Returns [`AttachError::IdentityInUse`] until the previous lease has
    /// closed successfully or completed best-effort cleanup.
    pub fn attach(&self, identity: PeerIdentity, policy: Policy) -> Result<Lease, AttachError> {
        let id = self.next_lease_id.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(LeaseState::new(id, identity, policy));
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Attach {
                state: Arc::clone(&state),
                reply: reply_tx,
            })
            .map_err(|_| AttachError::RuntimeStopped)?;
        reply_rx.recv().map_err(|_| AttachError::RuntimeStopped)??;
        Ok(Lease {
            endpoint: self.endpoint,
            commands: self.commands.clone(),
            state: Some(state),
        })
    }

    /// Stop the listener and certify that all leases have stopped.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime is unavailable or tracked work remains
    /// at `deadline`.
    pub fn shutdown(mut self, deadline: Instant) -> Result<(), ProxyError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Shutdown {
                deadline,
                reply: reply_tx,
            })
            .map_err(|_| ProxyError::RuntimeStopped)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        match reply_rx.recv_timeout(remaining) {
            Ok(Ok(())) => {
                if let Some(thread) = self.thread.take() {
                    thread.join().map_err(|_| ProxyError::RuntimeStopped)?;
                }
                Ok(())
            }
            Ok(Err(())) | Err(mpsc::RecvTimeoutError::Timeout) => Err(ProxyError::ShutdownTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProxyError::RuntimeStopped),
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let (reply, _) = mpsc::sync_channel(1);
        let _ = self.commands.send(Command::Shutdown {
            deadline: Instant::now() + Duration::from_secs(1),
            reply,
        });
    }
}

/// Exclusive management handle for one run's proxy identity and work.
pub struct Lease {
    endpoint: Endpoint,
    commands: tokio_mpsc::UnboundedSender<Command>,
    state: Option<Arc<LeaseState>>,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Lease")
            .field("endpoint", &self.endpoint)
            .field("id", &self.state.as_ref().map(|state| state.id))
            .finish_non_exhaustive()
    }
}

impl Lease {
    /// Return the proxy URL for this run.
    pub const fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// Read monotonic live counters without crossing the runtime boundary.
    pub fn usage(&self) -> Usage {
        self.state
            .as_ref()
            .map_or_else(Usage::default, |state| state.counters.snapshot())
    }

    /// Revoke this lease and wait for all of its tracked work to be destroyed.
    ///
    /// # Errors
    ///
    /// On timeout or runtime failure, the returned [`CloseError`] retains the
    /// lease. Recover it with [`CloseError::into_lease`] and retry; the identity
    /// remains unavailable in the meantime.
    pub fn close(mut self, deadline: Instant) -> Result<FinalUsage, CloseError> {
        let Some(state) = self.state.as_ref().map(Arc::clone) else {
            return Err(CloseError {
                kind: CloseErrorKind::RuntimeStopped,
                lease: self,
            });
        };
        state.begin_close();
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .commands
            .send(Command::Close {
                state: Arc::clone(&state),
                deadline,
                reply: reply_tx,
            })
            .is_err()
        {
            return Err(CloseError {
                kind: CloseErrorKind::RuntimeStopped,
                lease: self,
            });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match reply_rx.recv_timeout(remaining) {
            Ok(Ok(usage)) => {
                // Releasing the identity is the caller's commit point. The
                // runtime may finish cleanup at the deadline while reply
                // delivery loses the race; a failed close must not release
                // ownership in that case.
                state.mark_closed();
                let _ = self.commands.send(Command::Release {
                    state: Arc::clone(&state),
                });
                self.state.take();
                Ok(usage)
            }
            Ok(Err(kind)) => Err(CloseError { kind, lease: self }),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(CloseError {
                kind: CloseErrorKind::DeadlineExceeded,
                lease: self,
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CloseError {
                kind: CloseErrorKind::RuntimeStopped,
                lease: self,
            }),
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.begin_close();
            let _ = self.commands.send(Command::Reap { state });
        }
    }
}

enum Command {
    Attach {
        state: Arc<LeaseState>,
        reply: mpsc::SyncSender<Result<(), AttachError>>,
    },
    Close {
        state: Arc<LeaseState>,
        deadline: Instant,
        reply: mpsc::SyncSender<Result<FinalUsage, CloseErrorKind>>,
    },
    Reap {
        state: Arc<LeaseState>,
    },
    Release {
        state: Arc<LeaseState>,
    },
    Shutdown {
        deadline: Instant,
        reply: mpsc::SyncSender<Result<(), ()>>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Open,
    Revoking,
    Closed,
}

#[derive(Debug)]
struct LeaseState {
    id: u64,
    identity: PeerIdentity,
    policy: Arc<Policy>,
    phase: Mutex<Phase>,
    cancel: CancellationToken,
    tracker: TaskTracker,
    permits: Arc<Semaphore>,
    counters: Arc<Counters>,
}

impl LeaseState {
    fn new(id: u64, identity: PeerIdentity, policy: Policy) -> Self {
        let max_connections = policy.max_connections;
        Self {
            id,
            identity,
            policy: Arc::new(policy),
            phase: Mutex::new(Phase::Open),
            cancel: CancellationToken::new(),
            tracker: TaskTracker::new(),
            permits: Arc::new(Semaphore::new(max_connections)),
            counters: Arc::new(Counters::default()),
        }
    }

    fn is_closed(&self) -> bool {
        *self.phase.lock().expect("lease phase poisoned") == Phase::Closed
    }

    fn admit(self: &Arc<Self>) -> Option<Admission> {
        let permit = self.permits.clone().try_acquire_owned().ok()?;
        let phase = self.phase.lock().expect("lease phase poisoned");
        if *phase != Phase::Open {
            self.counters.deny();
            return None;
        }
        let tracking = self.tracker.token();
        self.counters.admit();
        drop(phase);
        Some(Admission {
            state: Arc::clone(self),
            _tracking: tracking,
            _permit: permit,
            completed: false,
        })
    }

    fn begin_close(&self) {
        let mut phase = self.phase.lock().expect("lease phase poisoned");
        if *phase == Phase::Open {
            *phase = Phase::Revoking;
            self.tracker.close();
            self.cancel.cancel();
        }
    }

    fn mark_closed(&self) {
        let mut phase = self.phase.lock().expect("lease phase poisoned");
        *phase = Phase::Closed;
    }
}

struct Admission {
    state: Arc<LeaseState>,
    _tracking: TaskTrackerToken,
    _permit: tokio::sync::OwnedSemaphorePermit,
    completed: bool,
}

impl Admission {
    fn mark_completed(&mut self) {
        self.completed = true;
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.state.counters.finish(self.completed);
    }
}

#[allow(clippy::too_many_lines)]
async fn run_proxy(
    config: ProxyConfig,
    mut commands: tokio_mpsc::UnboundedReceiver<Command>,
    command_sender: tokio_mpsc::UnboundedSender<Command>,
    ready: mpsc::SyncSender<Result<Endpoint, String>>,
) {
    let resolver =
        match TokioResolver::builder_tokio().and_then(hickory_resolver::ResolverBuilder::build) {
            Ok(resolver) => Arc::new(resolver),
            Err(error) => {
                let _ = ready.send(Err(format!("resolver initialization failed: {error}")));
                return;
            }
        };
    let listener = match TcpListener::bind(config.bind_address).await {
        Ok(listener) => listener,
        Err(error) => {
            let _ = ready.send(Err(format!("listener bind failed: {error}")));
            return;
        }
    };
    let endpoint = match listener.local_addr() {
        Ok(address) => Endpoint::new(address),
        Err(error) => {
            let _ = ready.send(Err(format!("listener address failed: {error}")));
            return;
        }
    };
    if ready.send(Ok(endpoint)).is_err() {
        return;
    }

    let mut leases: HashMap<PeerIdentity, Arc<LeaseState>> = HashMap::new();
    let global_permits = Arc::new(Semaphore::new(config.max_connections));

    loop {
        tokio::select! {
            biased;
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::Attach { state, reply } => {
                        let replaceable = leases.get(&state.identity).is_none_or(|old| old.is_closed());
                        if replaceable {
                            leases.insert(state.identity.clone(), state);
                            let _ = reply.send(Ok(()));
                        } else {
                            let _ = reply.send(Err(AttachError::IdentityInUse));
                        }
                    }
                    Command::Close { state, deadline, reply } => {
                        state.begin_close();
                        spawn_close_wait(state, config.identity_reuse_quiet_period, deadline, reply);
                    }
                    Command::Reap { state } => {
                        state.begin_close();
                        let command_sender = command_sender.clone();
                        tokio::spawn(async move {
                            state.tracker.wait().await;
                            sleep(config.identity_reuse_quiet_period).await;
                            state.mark_closed();
                            let _ = command_sender.send(Command::Release { state });
                        });
                    }
                    Command::Release { state } => {
                        release_if_current(&mut leases, &state);
                    }
                    Command::Shutdown { deadline, reply } => {
                        for state in leases.values() {
                            state.begin_close();
                        }
                        let mut success = true;
                        for state in leases.values() {
                            if timeout_at(TokioInstant::from_std(deadline), state.tracker.wait()).await.is_err() {
                                success = false;
                                break;
                            }
                            state.mark_closed();
                        }
                        let _ = reply.send(success.then_some(()).ok_or(()));
                        break;
                    }
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                let identity = PeerIdentity::SourceIp(peer.ip());
                let Some(state) = leases.get(&identity).cloned() else {
                    drop(stream);
                    continue;
                };
                let Ok(global_permit) = global_permits.clone().try_acquire_owned() else {
                    state.counters.deny();
                    drop(stream);
                    continue;
                };
                let Some(admission) = state.admit() else {
                    drop(global_permit);
                    drop(stream);
                    continue;
                };
                let resolver = Arc::clone(&resolver);
                let config = config.clone();
                tokio::spawn(async move {
                    let mut admission = admission;
                    let cancel = admission.state.cancel.clone();
                    let state = Arc::clone(&admission.state);
                    let result = tokio::select! {
                        biased;
                        () = cancel.cancelled() => None,
                        result = serve_connect(stream, &state, &resolver, &config) => Some(result),
                    };
                    if matches!(result, Some(Ok(ConnectionDisposition::Completed))) {
                        admission.mark_completed();
                    }
                    drop(global_permit);
                });
            }
        }
    }
}

fn release_if_current(
    leases: &mut HashMap<PeerIdentity, Arc<LeaseState>>,
    state: &Arc<LeaseState>,
) {
    let is_current = leases
        .get(&state.identity)
        .is_some_and(|current| Arc::ptr_eq(current, state));
    if is_current {
        leases.remove(&state.identity);
    }
}

fn spawn_close_wait(
    state: Arc<LeaseState>,
    quiet_period: Duration,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<FinalUsage, CloseErrorKind>>,
) {
    tokio::spawn(async move {
        let close = async {
            state.tracker.wait().await;
            sleep(quiet_period).await;
            state.counters.final_snapshot()
        };
        let result = timeout_at(TokioInstant::from_std(deadline), close)
            .await
            .map_err(|_| CloseErrorKind::DeadlineExceeded);
        let _ = reply.send(result);
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionDisposition {
    Completed,
    Denied,
}

async fn serve_connect(
    mut client: TcpStream,
    state: &Arc<LeaseState>,
    resolver: &TokioResolver,
    config: &ProxyConfig,
) -> io::Result<ConnectionDisposition> {
    let started = TokioInstant::now();
    let handshake_deadline = started + state.policy.handshake_timeout;
    let header_deadline = (started + config.header_timeout).min(handshake_deadline);
    let Ok(Ok(header)) = timeout_at(
        header_deadline,
        read_header(&mut client, config.max_header_bytes),
    )
    .await
    else {
        return deny(&mut client, state, 408, "header-timeout").await;
    };

    let request = match parse_connect(&header.bytes[..header.end]) {
        Ok(request) => request,
        Err(reason) => return deny(&mut client, state, 400, reason).await,
    };
    if !state.policy.allows_port(request.port) {
        return deny(&mut client, state, 403, "port-denied").await;
    }

    let addresses = if let Ok(ip) = request.host.parse::<IpAddr>() {
        if !state.policy.allows_ip_literal(ip) {
            return deny(&mut client, state, 403, "ip-literal-denied").await;
        }
        vec![SocketAddr::new(ip, request.port)]
    } else {
        let Some(hostname) = canonical_hostname(&request.host) else {
            return deny(&mut client, state, 400, "invalid-hostname").await;
        };
        if !state.policy.allows_hostname(&hostname) {
            return deny(&mut client, state, 403, "host-denied").await;
        }
        let dns_deadline = (TokioInstant::now() + state.policy.dns_timeout).min(handshake_deadline);
        let Ok(Ok(lookup)) =
            timeout_at(dns_deadline, resolver.lookup_ip(format!("{hostname}."))).await
        else {
            return deny(&mut client, state, 502, "dns-failed").await;
        };
        let mut addresses = Vec::new();
        for ip in lookup.iter() {
            if !state.policy.allows_ip(ip) {
                return deny(&mut client, state, 403, "resolved-address-denied").await;
            }
            addresses.push(SocketAddr::new(ip, request.port));
        }
        if addresses.is_empty() {
            return deny(&mut client, state, 502, "dns-empty").await;
        }
        addresses
    };

    let mut upstream = None;
    for address in addresses {
        match timeout_at(handshake_deadline, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => {
                upstream = Some(stream);
                break;
            }
            Ok(Err(_)) => {}
            Err(_) => break,
        }
    }
    let Some(mut upstream) = upstream else {
        return deny(&mut client, state, 502, "dial-failed").await;
    };

    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    if header.end < header.bytes.len() {
        upstream.write_all(&header.bytes[header.end..]).await?;
        state
            .counters
            .record_upload((header.bytes.len() - header.end) as u64);
    }

    let mut client = Metered::new(
        client,
        Arc::clone(&state.counters),
        Direction::Upload,
        state.policy.max_upload_bytes,
    );
    let mut upstream = Metered::new(
        upstream,
        Arc::clone(&state.counters),
        Direction::Download,
        state.policy.max_download_bytes,
    );
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(ConnectionDisposition::Completed)
}

struct HeaderBlock {
    bytes: Vec<u8>,
    end: usize,
}

async fn read_header(stream: &mut TcpStream, max: usize) -> io::Result<HeaderBlock> {
    let mut bytes = Vec::with_capacity(max.min(4_096));
    let mut chunk = [0_u8; 4_096];
    loop {
        if let Some(end) = find_header_end(&bytes) {
            return Ok(HeaderBlock { bytes, end });
        }
        if bytes.len() >= max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header too large",
            ));
        }
        let allowed = (max - bytes.len()).min(chunk.len());
        let read = stream.read(&mut chunk[..allowed]).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "header ended early",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

#[derive(Debug)]
struct ConnectRequest {
    host: String,
    port: u16,
}

fn parse_connect(bytes: &[u8]) -> Result<ConnectRequest, &'static str> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(bytes) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err("incomplete-header"),
        Err(_) => return Err("malformed-header"),
    }
    if request.method != Some("CONNECT") {
        return Err("connect-required");
    }
    if !matches!(request.version, Some(0 | 1)) {
        return Err("unsupported-http-version");
    }
    let target = request.path.ok_or("missing-authority")?;
    if target.contains('@') {
        return Err("userinfo-not-allowed");
    }
    let authority: Authority = target.parse().map_err(|_| "invalid-authority")?;
    let port = authority.port_u16().ok_or("missing-port")?;
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    Ok(ConnectRequest {
        host: host.to_owned(),
        port,
    })
}

async fn deny(
    client: &mut TcpStream,
    state: &LeaseState,
    status: u16,
    reason: &'static str,
) -> io::Result<ConnectionDisposition> {
    state.counters.deny();
    let body = format!("sandbox-egress denied: {reason}\n");
    let response = format!(
        "HTTP/1.1 {status} Denied\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(response.as_bytes()).await;
    let _ = client.shutdown().await;
    Ok(ConnectionDisposition::Denied)
}

#[derive(Clone, Copy)]
enum Direction {
    Upload,
    Download,
}

struct Metered<T> {
    inner: T,
    counters: Arc<Counters>,
    direction: Direction,
    limit: Option<u64>,
}

impl<T> Metered<T> {
    fn new(inner: T, counters: Arc<Counters>, direction: Direction, limit: Option<u64>) -> Self {
        Self {
            inner,
            counters,
            direction,
            limit,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Metered<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if let Poll::Ready(Ok(())) = result {
            let bytes = (buffer.filled().len() - before) as u64;
            let total = match self.direction {
                Direction::Upload => self.counters.record_upload(bytes),
                Direction::Download => self.counters.record_download(bytes),
            };
            if self.limit.is_some_and(|limit| total > limit) {
                return Poll::Ready(Err(io::Error::other("transfer byte limit exceeded")));
            }
        }
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Metered<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_connect_authority() {
        let request =
            parse_connect(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n")
                .expect("valid CONNECT");
        assert_eq!(request.host, "example.com");
        assert_eq!(request.port, 443);
    }

    #[test]
    fn parser_rejects_plain_http() {
        assert_eq!(
            parse_connect(b"GET http://example.com/ HTTP/1.1\r\n\r\n").unwrap_err(),
            "connect-required"
        );
    }

    #[test]
    fn parser_rejects_userinfo_in_connect_authority() {
        assert_eq!(
            parse_connect(b"CONNECT user@example.com:443 HTTP/1.1\r\n\r\n").unwrap_err(),
            "userinfo-not-allowed"
        );
    }

    #[test]
    fn parser_normalizes_bracketed_ipv6() {
        let request = parse_connect(b"CONNECT [2001:db8::1]:443 HTTP/1.1\r\n\r\n")
            .expect("valid IPv6 CONNECT");
        assert_eq!(request.host, "2001:db8::1");
        assert_eq!(request.port, 443);
    }

    #[test]
    fn cleanup_readiness_does_not_release_identity_before_success_is_observed() {
        let state = Arc::new(LeaseState::new(
            1,
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        ));
        state.begin_close();

        let runtime = RuntimeBuilder::new_multi_thread()
            .worker_threads(1)
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
            );
        }
        let usage = reply_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup reply")
            .expect("cleanup ready");

        assert_eq!(usage.usage().active_connections, 0);
        assert_eq!(*state.phase.lock().expect("lease phase"), Phase::Revoking);
        state.mark_closed();
        assert!(state.is_closed());
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
    fn delayed_release_cannot_remove_a_replacement_lease() {
        let identity = PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let old = Arc::new(LeaseState::new(
            1,
            identity.clone(),
            Policy::builder().build().expect("old policy"),
        ));
        let replacement = Arc::new(LeaseState::new(
            2,
            identity.clone(),
            Policy::builder().build().expect("replacement policy"),
        ));
        let mut leases = HashMap::from([(identity.clone(), Arc::clone(&replacement))]);

        release_if_current(&mut leases, &old);

        let retained = leases.get(&identity).expect("replacement retained");
        assert!(Arc::ptr_eq(retained, &replacement));
    }
}
