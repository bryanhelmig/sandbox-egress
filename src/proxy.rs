use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use hickory_resolver::TokioResolver;
#[cfg(test)]
use tokio::io::AsyncReadExt;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::sync::{Semaphore, mpsc as tokio_mpsc, watch};
use tokio::time::{Instant as TokioInstant, sleep, sleep_until, timeout_at};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

use crate::connect::{ConnectRequest, HeaderBlock, parse_connect, read_bounded_header};
use crate::diagnostic::{DenialReason, DiagnosticReporter};
use crate::policy::canonical_hostname;
use crate::rate::TokenBucket;
use crate::resolver::{ResolverBackend, build_system_resolver};
#[cfg(test)]
use crate::resolver::{TestResolver, apply_resolver_cache_options};
use crate::tls::{ClientHelloError, read_client_hello};
use crate::upstream::{ConnectedStream, connect_via};
use crate::usage::Counters;
use crate::{
    AttachError, CloseError, CloseErrorKind, EchPolicy, Endpoint, FinalUsage, PeerIdentity, Policy,
    ProxyConfig, ProxyError, ShutdownError, ShutdownErrorKind, TlsAuthority, Usage,
};

/// Shared proxy listener and synchronous management handle.
#[must_use = "keep the proxy owner and call shutdown to certify cleanup"]
pub struct Proxy {
    endpoint: Endpoint,
    commands: tokio_mpsc::UnboundedSender<Command>,
    thread: Option<thread::JoinHandle<()>>,
    next_lease_id: AtomicU64,
    diagnostics: DiagnosticReporter,
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
        Self::start_inner(config, None, None)
    }

    fn start_inner(
        config: ProxyConfig,
        resolver: Option<ResolverBackend>,
        connector: Option<ConnectorBackend>,
    ) -> Result<Self, ProxyError> {
        config
            .validate()
            .map_err(|reason| ProxyError::Initialization(reason.to_owned()))?;
        let diagnostics = DiagnosticReporter::new(config.diagnostics.as_ref());
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
                        runtime.block_on(run_proxy(
                            config,
                            receiver,
                            runtime_commands,
                            ready_tx,
                            resolver,
                            connector,
                        ));
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("runtime creation failed: {error}")));
                    }
                }
            })
            .map_err(|error| ProxyError::Initialization(error.to_string()))?;

        let endpoint = match ready_rx.recv() {
            Ok(Ok(endpoint)) => endpoint,
            Ok(Err(reason)) => {
                let _ = thread.join();
                return Err(ProxyError::Initialization(reason));
            }
            Err(_) => {
                let _ = thread.join();
                return Err(ProxyError::RuntimeStopped);
            }
        };
        Ok(Self {
            endpoint,
            commands,
            thread: Some(thread),
            next_lease_id: AtomicU64::new(1),
            diagnostics,
        })
    }

    #[cfg(test)]
    pub(crate) fn start_with_test_resolver(
        config: ProxyConfig,
        resolver: Arc<dyn TestResolver>,
    ) -> Result<Self, ProxyError> {
        Self::start_inner(config, Some(ResolverBackend::Test(resolver)), None)
    }

    #[cfg(test)]
    fn start_with_test_connector(
        config: ProxyConfig,
        connector: Arc<dyn TestConnector>,
    ) -> Result<Self, ProxyError> {
        Self::start_inner(config, None, Some(ConnectorBackend::Test(connector)))
    }

    #[cfg(test)]
    pub(crate) fn start_with_test_backends(
        config: ProxyConfig,
        resolver: Arc<dyn TestResolver>,
        connector: Arc<dyn TestConnector>,
    ) -> Result<Self, ProxyError> {
        Self::start_inner(
            config,
            Some(ResolverBackend::Test(resolver)),
            Some(ConnectorBackend::Test(connector)),
        )
    }

    /// Return the listener endpoint shared by all leases.
    ///
    /// A wildcard bind reports its wildcard IP; the host integration chooses
    /// the reachable address it advertises to each guest.
    pub const fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// Attach an immutable policy to one host-authenticated peer identity.
    ///
    /// # Errors
    ///
    /// Returns [`AttachError::InvalidIdentity`] for a non-unicast IPv4,
    /// unspecified or multicast IPv6, or scoped IPv6 unicast source address and
    /// [`AttachError::IdentityInUse`]
    /// until the previous lease has closed successfully or completed best-effort
    /// cleanup. It returns
    /// [`AttachError::LeaseIdExhausted`] rather than reusing a diagnostic
    /// sequence after process-local exhaustion, and
    /// [`AttachError::ProxyStopping`] after proxy-wide shutdown begins.
    pub fn attach(&self, identity: PeerIdentity, policy: Policy) -> Result<Lease, AttachError> {
        let identity = identity.canonical();
        if !identity.is_attachable() {
            return Err(AttachError::InvalidIdentity);
        }
        let id = self
            .next_lease_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| AttachError::LeaseIdExhausted)?;
        let state = Arc::new(LeaseState::new(
            id,
            identity,
            policy,
            self.diagnostics.clone(),
        ));
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Attach {
                state: Arc::clone(&state),
                reply: reply_tx,
            })
            .map_err(|_| AttachError::RuntimeStopped)?;
        reply_rx.recv().map_err(|_| AttachError::RuntimeStopped)??;
        Ok(Lease {
            id,
            endpoint: self.endpoint,
            commands: self.commands.clone(),
            state: Some(state),
        })
    }

    /// Stop the listener and certify that all leases have stopped.
    ///
    /// # Errors
    ///
    /// Returns a [`ShutdownError`] containing the still-stopping proxy if the
    /// runtime is unavailable or tracked work remains at `deadline`. Recover
    /// it with [`ShutdownError::into_proxy`] and retry when the runtime remains
    /// available; new attachments stay refused after the first attempt.
    pub fn shutdown(mut self, deadline: Instant) -> Result<(), ShutdownError> {
        // A zero-capacity reply is the shutdown commit handshake: the runtime
        // exits only when this caller actually observes successful cleanup.
        let (reply_tx, reply_rx) = mpsc::sync_channel(0);
        if self
            .commands
            .send(Command::Shutdown {
                deadline,
                reply: reply_tx,
                retryable: true,
            })
            .is_err()
        {
            return Err(ShutdownError {
                kind: ShutdownErrorKind::RuntimeStopped,
                proxy: self,
            });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = match reply_rx.recv_timeout(remaining) {
            Ok(Ok(())) => self.thread.take().map_or(Ok(()), |thread| {
                thread.join().map_err(|_| ShutdownErrorKind::RuntimeStopped)
            }),
            Ok(Err(())) | Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(ShutdownErrorKind::DeadlineExceeded)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ShutdownErrorKind::RuntimeStopped),
        };
        result.map_err(|kind| ShutdownError { kind, proxy: self })
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let (reply, _) = mpsc::sync_channel(1);
        let _ = self.commands.send(Command::Shutdown {
            deadline: Instant::now() + Duration::from_secs(1),
            reply,
            retryable: false,
        });
    }
}

/// Exclusive management handle for one run's proxy identity and work.
#[must_use = "keep the lease owner and call close before reusing its identity"]
pub struct Lease {
    id: u64,
    endpoint: Endpoint,
    commands: tokio_mpsc::UnboundedSender<Command>,
    state: Option<Arc<LeaseState>>,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Lease")
            .field("endpoint", &self.endpoint)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Lease {
    /// Return the unique process-local sequence for correlating host diagnostics.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Return the shared proxy listener endpoint for this run.
    ///
    /// A wildcard bind reports its wildcard IP; the host integration chooses
    /// the reachable address it advertises to each guest.
    pub const fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// Read monotonic live counters without crossing the runtime boundary.
    pub fn usage(&self) -> Usage {
        self.state
            .as_ref()
            .map_or_else(Usage::default, |state| state.counters.snapshot())
    }

    fn take_closed_usage(&mut self, state: &LeaseState) -> Option<FinalUsage> {
        let usage = state.closed_snapshot()?;
        self.state.take();
        Some(usage)
    }

    fn runtime_stopped_or_closed(mut self, state: &LeaseState) -> Result<FinalUsage, CloseError> {
        if let Some(usage) = self.take_closed_usage(state) {
            Ok(usage)
        } else {
            Err(CloseError {
                kind: CloseErrorKind::RuntimeStopped,
                lease: self,
            })
        }
    }

    /// Revoke this lease and wait for all of its tracked work to be destroyed.
    ///
    /// # Errors
    ///
    /// On timeout or runtime failure, the returned [`CloseError`] retains the
    /// lease. Recover it with [`CloseError::into_lease`] and retry; the identity
    /// remains unavailable in the meantime. If proxy-wide shutdown already
    /// certified this lease, `close` consumes that committed snapshot locally.
    pub fn close(mut self, deadline: Instant) -> Result<FinalUsage, CloseError> {
        let Some(state) = self.state.as_ref().map(Arc::clone) else {
            return Err(CloseError {
                kind: CloseErrorKind::RuntimeStopped,
                lease: self,
            });
        };
        if let Some(usage) = self.take_closed_usage(&state) {
            return Ok(usage);
        }
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
            return self.runtime_stopped_or_closed(&state);
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
            Err(mpsc::RecvTimeoutError::Disconnected) => self.runtime_stopped_or_closed(&state),
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
        retryable: bool,
    },
    DrainAcceptQueue {
        reply: tokio::sync::oneshot::Sender<Result<(), CloseErrorKind>>,
    },
    #[cfg(test)]
    KeepCommandsReady {
        until: Instant,
        started: Option<mpsc::SyncSender<()>>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Open,
    Revoking(u64),
    Quiesced,
    Closed,
}

#[derive(Debug)]
struct LeaseState {
    id: u64,
    identity: PeerIdentity,
    policy: Policy,
    phase: Mutex<Phase>,
    cancel: CancellationToken,
    tracker: TaskTracker,
    permits: Arc<Semaphore>,
    connection_attempts: Option<Mutex<TokenBucket>>,
    counters: Arc<Counters>,
    diagnostics: DiagnosticReporter,
}

impl LeaseState {
    fn new(
        id: u64,
        identity: PeerIdentity,
        policy: Policy,
        diagnostics: DiagnosticReporter,
    ) -> Self {
        let max_connections = policy.max_connections;
        let connection_attempt_rate = policy.connection_attempt_rate;
        Self {
            id,
            identity,
            policy,
            phase: Mutex::new(Phase::Open),
            cancel: CancellationToken::new(),
            tracker: TaskTracker::new(),
            permits: Arc::new(Semaphore::new(max_connections)),
            connection_attempts: connection_attempt_rate
                .map(|limit| Mutex::new(TokenBucket::full(limit, Instant::now()))),
            counters: Arc::new(Counters::default()),
            diagnostics,
        }
    }

    fn is_closed(&self) -> bool {
        *self.phase.lock().expect("lease phase poisoned") == Phase::Closed
    }

    fn closed_snapshot(&self) -> Option<FinalUsage> {
        let phase = self.phase.lock().expect("lease phase poisoned");
        (*phase == Phase::Closed).then(|| self.counters.final_snapshot())
    }

    fn admit(self: &Arc<Self>, stream: TcpStream) -> Option<(Admission, TcpStream)> {
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            self.reject_unadmitted(stream, "lease-capacity");
            return None;
        };
        let mut phase = self.phase.lock().expect("lease phase poisoned");
        if *phase != Phase::Open {
            drop(permit);
            drop(stream);
            self.record_unadmitted_locked(&mut phase, "lease-revoking");
            return None;
        }
        let tracking = self.tracker.token();
        self.counters.admit();
        drop(phase);
        Some((
            Admission {
                state: Arc::clone(self),
                _tracking: tracking,
                _permit: permit,
                completed: false,
            },
            stream,
        ))
    }

    fn allows_connection_attempt(
        &self,
        global: Option<&mut TokenBucket>,
    ) -> Result<(), &'static str> {
        let phase = self.phase.lock().expect("lease phase poisoned");
        if *phase != Phase::Open {
            return Err("lease-revoking");
        }
        let now = Instant::now();
        if self.connection_attempts.as_ref().is_some_and(|bucket| {
            !bucket
                .lock()
                .expect("lease connection-attempt bucket poisoned")
                .try_take(now)
        }) {
            return Err("lease-rate");
        }
        if global.is_some_and(|bucket| !bucket.try_take(now)) {
            return Err("global-rate");
        }
        Ok(())
    }

    fn begin_close(&self) {
        let mut phase = self.phase.lock().expect("lease phase poisoned");
        if *phase == Phase::Open {
            *phase = Phase::Revoking(0);
            self.tracker.close();
            self.cancel.cancel();
        }
    }

    fn mark_closed(&self) {
        let mut phase = self.phase.lock().expect("lease phase poisoned");
        *phase = Phase::Closed;
    }

    fn quiesce_if_generation(&self, expected: u64) -> Option<FinalUsage> {
        let mut phase = self.phase.lock().expect("lease phase poisoned");
        if !matches!(*phase, Phase::Revoking(generation) if generation == expected)
            || expected == u64::MAX
        {
            return None;
        }
        *phase = Phase::Quiesced;
        Some(self.counters.final_snapshot())
    }

    fn quiesced_snapshot(&self) -> Option<FinalUsage> {
        let phase = self.phase.lock().expect("lease phase poisoned");
        (*phase == Phase::Quiesced).then(|| self.counters.final_snapshot())
    }

    fn reject_unadmitted(&self, stream: TcpStream, reason: &'static str) {
        let mut phase = self.phase.lock().expect("lease phase poisoned");
        drop(stream);
        self.record_unadmitted_locked(&mut phase, reason);
    }

    fn record_unadmitted_locked(&self, phase: &mut Phase, reason: &'static str) {
        // The caller's phase lock orders both accounting and generation
        // changes before the final quiescence check.
        if matches!(phase, Phase::Open | Phase::Revoking(_)) {
            self.counters.deny();
            self.report_denial(reason);
            if let Phase::Revoking(generation) = phase
                && let Some(next) = generation.checked_add(1)
            {
                *generation = next;
            }
        }
    }

    fn revocation_generation(&self) -> u64 {
        match *self.phase.lock().expect("lease phase poisoned") {
            Phase::Revoking(generation) => generation,
            _ => u64::MAX,
        }
    }

    fn record_denial(&self, reason: &'static str) {
        self.counters.deny();
        self.report_denial(reason);
    }

    fn report_denial(&self, reason: &'static str) {
        self.diagnostics
            .report(self.id, self.identity.clone(), DenialReason::new(reason));
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

enum ConnectorBackend {
    Direct,
    Upstream(SocketAddr),
    #[cfg(test)]
    Test(Arc<dyn TestConnector>),
}

impl ConnectorBackend {
    async fn connect(&self, address: SocketAddr) -> io::Result<ConnectedStream> {
        match self {
            Self::Direct => TcpStream::connect(address)
                .await
                .map(ConnectedStream::direct),
            Self::Upstream(proxy) => connect_via(*proxy, address).await,
            #[cfg(test)]
            Self::Test(connector) => connector
                .connect(address)
                .await
                .map(ConnectedStream::direct),
        }
    }

    const fn failure_reason(&self) -> &'static str {
        match self {
            Self::Direct => "dial-failed",
            Self::Upstream(_) => "upstream-proxy-failed",
            #[cfg(test)]
            Self::Test(_) => "dial-failed",
        }
    }
}

#[cfg(test)]
pub(crate) trait TestConnector: Send + Sync {
    fn connect(
        &self,
        address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send + '_>>;
}

// Yield between bounded drain batches. A full batch is never evidence that the
// queue is empty; the command is requeued and attachment or cleanup stays held.
const ACCEPT_DRAIN_BATCH: usize = 256;
const ACCEPT_RETRY_INITIAL: Duration = Duration::from_millis(5);
const ACCEPT_RETRY_MAX: Duration = Duration::from_secs(1);

#[derive(Default)]
struct AcceptBackoff {
    delay: Duration,
    retry_at: Option<TokioInstant>,
}

impl AcceptBackoff {
    fn next_delay(&mut self) -> Duration {
        self.delay = if self.delay.is_zero() {
            ACCEPT_RETRY_INITIAL
        } else {
            self.delay.saturating_mul(2).min(ACCEPT_RETRY_MAX)
        };
        self.delay
    }

    fn fail(&mut self) {
        let delay = self.next_delay();
        self.retry_at = TokioInstant::now().checked_add(delay);
    }

    const fn retry_at(&self) -> Option<TokioInstant> {
        self.retry_at
    }

    fn resume(&mut self) {
        self.retry_at = None;
    }

    fn recover(&mut self) {
        self.delay = Duration::ZERO;
        self.resume();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrainResult {
    Drained,
    BatchFull,
    AcceptFailed,
}

struct ConnectionRuntime {
    shared: Arc<ConnectionShared>,
    global_permits: Arc<Semaphore>,
    global_connection_attempts: Option<TokenBucket>,
}

struct ConnectionShared {
    resolver: ResolverBackend,
    connector: ConnectorBackend,
    phase_permits: PhasePermits,
    config: ProxyConfig,
}

struct PhasePermits {
    dns: Semaphore,
    dial: Semaphore,
}

impl ConnectionRuntime {
    fn dispatch(
        &mut self,
        stream: TcpStream,
        peer: SocketAddr,
        leases: &HashMap<PeerIdentity, Arc<LeaseState>>,
    ) {
        let accepted_at = TokioInstant::now();
        let identity = PeerIdentity::SourceIp(peer.ip()).canonical();
        let Some(state) = leases.get(&identity).cloned() else {
            drop(stream);
            return;
        };
        if (state.connection_attempts.is_some() || self.global_connection_attempts.is_some())
            && let Err(reason) =
                state.allows_connection_attempt(self.global_connection_attempts.as_mut())
        {
            state.reject_unadmitted(stream, reason);
            return;
        }
        let Ok(global_permit) = self.global_permits.clone().try_acquire_owned() else {
            state.reject_unadmitted(stream, "global-capacity");
            return;
        };
        let Some((admission, stream)) = state.admit(stream) else {
            drop(global_permit);
            return;
        };
        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            let mut admission = admission;
            let cancel = admission.state.cancel.clone();
            let state = Arc::clone(&admission.state);
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                result = serve_connect(
                    stream,
                    &state,
                    &shared.resolver,
                    &shared.phase_permits,
                    &shared.connector,
                    &shared.config,
                    accepted_at,
                ) => Some(result),
            };
            if matches!(result, Some(Ok(ConnectionDisposition::Completed))) {
                admission.mark_completed();
            }
            drop(global_permit);
        });
    }

    async fn drain_ready(
        &mut self,
        listener: &TcpListener,
        leases: &HashMap<PeerIdentity, Arc<LeaseState>>,
    ) -> DrainResult {
        for _ in 0..ACCEPT_DRAIN_BATCH {
            let accepted = std::future::poll_fn(|context| match listener.poll_accept(context) {
                Poll::Ready(result) => Poll::Ready(Some(result)),
                Poll::Pending => Poll::Ready(None),
            })
            .await;
            match accepted {
                Some(Ok((stream, peer))) => self.dispatch(stream, peer, leases),
                None => return DrainResult::Drained,
                Some(Err(_)) => return DrainResult::AcceptFailed,
            }
        }
        DrainResult::BatchFull
    }
}

async fn wait_for_accept_retry(deadline: Option<TokioInstant>) {
    if let Some(deadline) = deadline {
        sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[allow(clippy::too_many_lines)]
async fn run_proxy(
    mut config: ProxyConfig,
    mut commands: tokio_mpsc::UnboundedReceiver<Command>,
    command_sender: tokio_mpsc::UnboundedSender<Command>,
    ready: mpsc::SyncSender<Result<Endpoint, String>>,
    resolver: Option<ResolverBackend>,
    connector: Option<ConnectorBackend>,
) {
    let resolver = match resolver {
        Some(resolver) => resolver,
        None => match build_system_resolver(&config) {
            Ok(resolver) => ResolverBackend::System(Box::new(resolver)),
            Err(error) => {
                let _ = ready.send(Err(format!("resolver initialization failed: {error}")));
                return;
            }
        },
    };
    let system_connector = config
        .upstream_proxy
        .map_or(ConnectorBackend::Direct, ConnectorBackend::Upstream);
    let connector = connector.unwrap_or(system_connector);
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
    config.bind_address = endpoint.socket_addr();
    if config
        .upstream_proxy
        .is_some_and(|proxy| is_proxy_endpoint(proxy, config.bind_address))
    {
        let _ = ready.send(Err(
            "upstream proxy must not be the Sandbox Egress listener".to_owned(),
        ));
        return;
    }
    if config
        .dns_servers
        .iter()
        .any(|server| is_proxy_endpoint(*server, config.bind_address))
    {
        let _ = ready.send(Err(
            "explicit DNS server must not be the Sandbox Egress listener".to_owned(),
        ));
        return;
    }
    if ready.send(Ok(endpoint)).is_err() {
        return;
    }

    let mut leases: HashMap<PeerIdentity, Arc<LeaseState>> = HashMap::new();
    let global_permits = Arc::new(Semaphore::new(config.max_connections));
    let global_connection_attempts = config
        .connection_attempt_rate
        .map(|limit| TokenBucket::full(limit, Instant::now()));
    let mut connections = ConnectionRuntime {
        shared: Arc::new(ConnectionShared {
            resolver,
            connector,
            phase_permits: PhasePermits {
                dns: Semaphore::new(config.max_concurrent_dns),
                dial: Semaphore::new(config.max_concurrent_dials),
            },
            config,
        }),
        global_permits,
        global_connection_attempts,
    };

    let mut stopping = false;
    let mut accept_backoff = AcceptBackoff::default();
    loop {
        let retry_at = accept_backoff.retry_at();
        tokio::select! {
            biased;
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::Attach { state, reply } => {
                        let replaceable = leases.get(&state.identity).is_none_or(|old| old.is_closed());
                        // Policy installation and this empty-queue observation
                        // are ordered on the one listener-owner task.
                        if stopping {
                            let _ = reply.send(Err(AttachError::ProxyStopping));
                        } else if !replaceable {
                            let _ = reply.send(Err(AttachError::IdentityInUse));
                        } else {
                            match connections.drain_ready(&listener, &leases).await {
                                DrainResult::Drained => {
                                    accept_backoff.recover();
                                    leases.insert(state.identity.clone(), state);
                                    let _ = reply.send(Ok(()));
                                }
                                DrainResult::BatchFull => {
                                    accept_backoff.recover();
                                    let _ = command_sender.send(Command::Attach { state, reply });
                                }
                                DrainResult::AcceptFailed => {
                                    accept_backoff.fail();
                                    let _ = reply.send(Err(AttachError::ListenerUnavailable));
                                }
                            }
                        }
                    }
                    Command::Close { state, deadline, reply } => {
                        state.begin_close();
                        spawn_close_wait(
                            state,
                            connections.shared.config.identity_reuse_quiet_period,
                            deadline,
                            reply,
                            Some(command_sender.clone()),
                        );
                    }
                    Command::Reap { state } => {
                        state.begin_close();
                        let command_sender = command_sender.clone();
                        let drain_sender = command_sender.clone();
                        let quiet_period = connections.shared.config.identity_reuse_quiet_period;
                        tokio::spawn(async move {
                            state.tracker.wait().await;
                            if quiesce_after_identity_quiet(
                                &state,
                                quiet_period,
                                Some(&drain_sender),
                            )
                            .await
                            .is_ok()
                            {
                                state.mark_closed();
                                let _ = command_sender.send(Command::Release { state });
                            }
                        });
                    }
                    Command::Release { state } => {
                        release_if_current(&mut leases, &state);
                    }
                    Command::Shutdown { deadline, reply, retryable } => {
                        stopping = true;
                        for state in leases.values() {
                            state.begin_close();
                        }
                        let mut success = true;
                        for state in leases.values() {
                            if complete_before_deadline(
                                TokioInstant::from_std(deadline),
                                state.tracker.wait(),
                            )
                            .await
                            .is_none()
                            {
                                success = false;
                                break;
                            }
                            state.mark_closed();
                        }
                        let delivered = reply.send(success.then_some(()).ok_or(())).is_ok();
                        if !retryable || success && delivered {
                            break;
                        }
                    }
                    Command::DrainAcceptQueue { reply } => {
                        if reply.is_closed() {
                            continue;
                        }
                        let drained = connections.drain_ready(&listener, &leases).await;
                        if reply.is_closed() {
                            continue;
                        }
                        match drained {
                            DrainResult::Drained => {
                                accept_backoff.recover();
                                let _ = reply.send(Ok(()));
                            }
                            DrainResult::BatchFull => {
                                accept_backoff.recover();
                                let _ = command_sender.send(Command::DrainAcceptQueue { reply });
                            }
                            DrainResult::AcceptFailed => {
                                accept_backoff.fail();
                                let _ = reply.send(Err(CloseErrorKind::ListenerUnavailable));
                            }
                        }
                    }
                    #[cfg(test)]
                    Command::KeepCommandsReady { until, started } => {
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        if Instant::now() < until {
                            let _ = command_sender.send(Command::KeepCommandsReady {
                                until,
                                started: None,
                            });
                        }
                    }
                }
            }
            () = wait_for_accept_retry(retry_at), if retry_at.is_some() => {
                accept_backoff.resume();
            }
            accepted = listener.accept(), if !stopping && retry_at.is_none() => {
                match accepted {
                    Ok((stream, peer)) => {
                        accept_backoff.recover();
                        connections.dispatch(stream, peer, &leases);
                    }
                    Err(_) => accept_backoff.fail(),
                }
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
    drain_sender: Option<tokio_mpsc::UnboundedSender<Command>>,
) {
    tokio::spawn(async move {
        let close = async {
            state.tracker.wait().await;
            quiesce_after_identity_quiet(&state, quiet_period, drain_sender.as_ref()).await
        };
        let result = match complete_before_deadline(TokioInstant::from_std(deadline), close).await {
            Some(result) => result,
            None => Err(CloseErrorKind::DeadlineExceeded),
        };
        let _ = reply.send(result);
    });
}

async fn quiesce_after_identity_quiet(
    state: &LeaseState,
    quiet_period: Duration,
    drain_sender: Option<&tokio_mpsc::UnboundedSender<Command>>,
) -> Result<FinalUsage, CloseErrorKind> {
    loop {
        if let Some(usage) = state.quiesced_snapshot() {
            if let Some(drain_sender) = drain_sender {
                drain_accept_queue(drain_sender).await?;
            }
            return Ok(usage);
        }
        let before = state.revocation_generation();
        sleep(quiet_period).await;
        if let Some(drain_sender) = drain_sender {
            // The listener owner drains first; the generation check below then
            // rejects this candidate interval if an old socket was observed.
            drain_accept_queue(drain_sender).await?;
        }
        if let Some(usage) = state.quiesce_if_generation(before) {
            return Ok(usage);
        }
    }
}

async fn drain_accept_queue(
    command_sender: &tokio_mpsc::UnboundedSender<Command>,
) -> Result<(), CloseErrorKind> {
    let (reply, drained) = tokio::sync::oneshot::channel();
    command_sender
        .send(Command::DrainAcceptQueue { reply })
        .map_err(|_| CloseErrorKind::RuntimeStopped)?;
    drained.await.map_err(|_| CloseErrorKind::RuntimeStopped)?
}

async fn complete_before_deadline<T>(
    deadline: TokioInstant,
    work: impl Future<Output = T>,
) -> Option<T> {
    if TokioInstant::now() >= deadline {
        return None;
    }
    timeout_at(deadline, work).await.ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionDisposition {
    Completed,
    Denied,
}

const CONNECT_SUCCESS_RESPONSE: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

async fn serve_connect(
    mut client: TcpStream,
    state: &Arc<LeaseState>,
    resolver: &ResolverBackend,
    phase_permits: &PhasePermits,
    connector: &ConnectorBackend,
    config: &ProxyConfig,
    accepted_at: TokioInstant,
) -> io::Result<ConnectionDisposition> {
    let Some((deadline, header_deadline)) = connection_deadlines(
        accepted_at,
        state.policy.handshake_timeout,
        config.header_timeout,
    ) else {
        return reject_without_response(&mut client, state, "invalid-handshake-deadline").await;
    };
    let header =
        match read_connect_header(&mut client, config.max_header_bytes, header_deadline).await {
            Ok(header) => header,
            Err(denial) => return deny(&mut client, state, denial, deadline).await,
        };

    let request = match parse_connect(&header.bytes[..header.end]) {
        Ok(request) => request,
        Err(reason) => {
            return deny(&mut client, state, Denial::new(400, reason), deadline).await;
        }
    };
    if !state.policy.allows_port(request.port) {
        return deny(&mut client, state, Denial::PORT_DENIED, deadline).await;
    }
    let buffered_upload = header.bytes.len() - header.end;
    if state
        .policy
        .max_upload_bytes
        .is_some_and(|limit| buffered_upload as u64 > limit)
    {
        state.counters.record_upload(buffered_upload as u64);
        return deny(&mut client, state, Denial::UPLOAD_LIMIT, deadline).await;
    }

    let addresses = match resolve_addresses(
        &request,
        state,
        resolver,
        &phase_permits.dns,
        config,
        deadline,
    )
    .await
    {
        Ok(addresses) => addresses,
        Err(denial) => return deny(&mut client, state, denial, deadline).await,
    };

    let upstream = match dial_with_budget(
        addresses,
        connector,
        &phase_permits.dial,
        &state.cancel,
        deadline,
    )
    .await
    {
        Ok(upstream) => upstream,
        Err(denial) => return deny(&mut client, state, denial, deadline).await,
    };
    let Some(upstream) = upstream else {
        let denial = Denial::new(502, connector.failure_reason());
        return deny(&mut client, state, denial, deadline).await;
    };

    establish_tunnel(client, upstream, state, &request, header, config, deadline).await
}

async fn establish_tunnel<U>(
    mut client: TcpStream,
    mut upstream: U,
    state: &LeaseState,
    request: &ConnectRequest,
    header: HeaderBlock,
    config: &ProxyConfig,
    handshake_deadline: TokioInstant,
) -> io::Result<ConnectionDisposition>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    if !write_connect_success(&mut client, handshake_deadline).await? {
        return reject_without_response(&mut client, state, "connect-response-timeout").await;
    }
    let buffered_upload = match state.policy.tls_authority {
        TlsAuthority::Disabled => match forward_uninspected_upload(
            &mut upstream,
            state,
            &header.bytes[header.end..],
            handshake_deadline,
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(reason) => return reject_without_response(&mut client, state, reason).await,
        },
        TlsAuthority::RequireVisibleSni { ech } => {
            let inspection_and_forward = async {
                let hello = inspect_tls_tunnel(
                    &mut client,
                    state,
                    &request.host,
                    ech,
                    header.bytes[header.end..].to_vec(),
                    config.max_client_hello_bytes,
                )
                .await?;
                let bytes = hello.len();
                upstream
                    .write_all(&hello)
                    .await
                    .map_err(|_| "upstream-write-failed")?;
                Ok::<usize, &'static str>(bytes)
            };
            match complete_before_deadline(handshake_deadline, inspection_and_forward).await {
                Some(Ok(bytes)) => bytes,
                Some(Err(reason)) => {
                    return reject_without_response(&mut client, state, reason).await;
                }
                None => {
                    return reject_without_response(&mut client, state, "client-hello-timeout")
                        .await;
                }
            }
        }
    };

    tunnel_bidirectionally(client, upstream, state, buffered_upload).await
}

async fn tunnel_bidirectionally<U>(
    client: TcpStream,
    upstream: U,
    state: &LeaseState,
    buffered_upload: usize,
) -> io::Result<ConnectionDisposition>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    let activity = state.policy.idle_timeout.map(|timeout| {
        let (sender, receiver) = watch::channel(TokioInstant::now());
        (timeout, sender, receiver)
    });
    let activity_sender = activity.as_ref().map(|(_, sender, _)| sender.clone());
    let mut client = Metered::new(
        client,
        Arc::clone(&state.counters),
        Direction::Upload,
        state.policy.max_upload_bytes,
        buffered_upload as u64,
        activity_sender.clone(),
    );
    let mut upstream = Metered::new(
        upstream,
        Arc::clone(&state.counters),
        Direction::Download,
        state.policy.max_download_bytes,
        0,
        activity_sender,
    );
    let copy = tokio::io::copy_bidirectional(&mut client, &mut upstream);
    let result = match activity {
        Some((timeout, _sender, receiver)) => {
            tokio::select! {
                biased;
                result = copy => result,
                () = wait_for_tunnel_idle(receiver, timeout) => {
                    state.record_denial("tunnel-idle-timeout");
                    return Ok(ConnectionDisposition::Denied);
                }
            }
        }
        None => copy.await,
    };
    match result {
        Ok(_) => Ok(ConnectionDisposition::Completed),
        Err(error) if is_transfer_limit_error(&error) => {
            state.record_denial("transfer-limit");
            Ok(ConnectionDisposition::Denied)
        }
        Err(error) => Err(error),
    }
}

async fn wait_for_tunnel_idle(mut activity: watch::Receiver<TokioInstant>, timeout: Duration) {
    loop {
        let observed = *activity.borrow_and_update();
        let Some(deadline) = observed.checked_add(timeout) else {
            // A policy can outlive the Instant used to validate its duration.
            // Treat a now-unrepresentable deadline as farther than the clock
            // can express while still waking on traffic and lease cancellation.
            if activity.changed().await.is_err() {
                return;
            }
            continue;
        };
        sleep_until(deadline).await;
        if !activity.has_changed().unwrap_or(false) {
            return;
        }
    }
}

fn connection_deadlines(
    started: TokioInstant,
    handshake_timeout: Duration,
    header_timeout: Duration,
) -> Option<(TokioInstant, TokioInstant)> {
    let handshake_deadline = started.checked_add(handshake_timeout)?;
    let header_deadline = started
        .checked_add(header_timeout)
        .unwrap_or(handshake_deadline)
        .min(handshake_deadline);
    Some((handshake_deadline, header_deadline))
}

async fn dial_approved_addresses(
    addresses: Vec<SocketAddr>,
    connector: &ConnectorBackend,
    cancel: &CancellationToken,
    handshake_deadline: TokioInstant,
) -> Option<ConnectedStream> {
    let mut addresses = addresses.into_iter();
    while let Some(address) = addresses.next() {
        if cancel.is_cancelled() {
            break;
        }
        let now = TokioInstant::now();
        let remaining = handshake_deadline.saturating_duration_since(now);
        let attempts_left = u32::try_from(addresses.len().saturating_add(1)).unwrap_or(u32::MAX);
        let attempt_budget = remaining / attempts_left;
        if attempt_budget.is_zero() {
            break;
        }
        let attempt_deadline = now
            .checked_add(attempt_budget)
            .unwrap_or(handshake_deadline)
            .min(handshake_deadline);
        if let Some(Ok(stream)) =
            complete_before_deadline(attempt_deadline, connector.connect(address)).await
        {
            return Some(stream);
        }
    }
    None
}

async fn dial_with_budget(
    addresses: Vec<SocketAddr>,
    connector: &ConnectorBackend,
    permits: &Semaphore,
    cancel: &CancellationToken,
    handshake_deadline: TokioInstant,
) -> Result<Option<ConnectedStream>, Denial> {
    let permit = complete_before_deadline(handshake_deadline, permits.acquire())
        .await
        .ok_or(Denial::DIAL_CAPACITY)?
        .map_err(|_| Denial::DIAL_CAPACITY)?;
    let upstream = dial_approved_addresses(addresses, connector, cancel, handshake_deadline).await;
    drop(permit);
    Ok(upstream)
}

async fn forward_uninspected_upload<W>(
    upstream: &mut W,
    state: &LeaseState,
    bytes: &[u8],
    handshake_deadline: TokioInstant,
) -> Result<usize, &'static str>
where
    W: AsyncWrite + Unpin,
{
    state.counters.record_upload(bytes.len() as u64);
    match complete_before_deadline(handshake_deadline, upstream.write_all(bytes)).await {
        Some(Ok(())) => Ok(bytes.len()),
        Some(Err(_)) => Err("upstream-write-failed"),
        None => Err("initial-upload-timeout"),
    }
}

async fn write_connect_success<W>(
    client: &mut W,
    handshake_deadline: TokioInstant,
) -> io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    match complete_before_deadline(
        handshake_deadline,
        client.write_all(CONNECT_SUCCESS_RESPONSE),
    )
    .await
    {
        Some(result) => result.map(|()| true),
        None => Ok(false),
    }
}

async fn inspect_tls_tunnel(
    client: &mut TcpStream,
    state: &LeaseState,
    connect_host: &str,
    ech_policy: EchPolicy,
    initial: Vec<u8>,
    configured_max: usize,
) -> Result<Vec<u8>, &'static str> {
    let initial_len = initial.len();
    state.counters.record_upload(initial_len as u64);
    let max_bytes = state
        .policy
        .max_upload_bytes
        .map_or(configured_max, |limit| {
            configured_max.min(usize::try_from(limit).unwrap_or(usize::MAX))
        });
    let mut metered = Metered::new(
        client,
        Arc::clone(&state.counters),
        Direction::Upload,
        state.policy.max_upload_bytes,
        initial_len as u64,
        None,
    );
    let hello = read_client_hello(&mut metered, initial, max_bytes)
        .await
        .map_err(|error| match error {
            ClientHelloError::TooLarge if max_bytes < configured_max => "upload-limit",
            ClientHelloError::TooLarge => "client-hello-too-large",
            ClientHelloError::Invalid => "client-hello-invalid",
            ClientHelloError::UnexpectedEof => "client-hello-eof",
        })?;
    let server_name = hello
        .server_name
        .as_deref()
        .and_then(canonical_hostname)
        .ok_or("tls-sni-missing")?;
    let connect_host = canonical_hostname(connect_host).ok_or("tls-authority-not-hostname")?;
    if server_name != connect_host {
        return Err("tls-sni-mismatch");
    }
    if hello.ech_present && ech_policy == EchPolicy::Reject {
        return Err("ech-denied");
    }
    Ok(hello.wire_bytes)
}

async fn reject_without_response(
    client: &mut TcpStream,
    state: &LeaseState,
    reason: &'static str,
) -> io::Result<ConnectionDisposition> {
    state.record_denial(reason);
    let _ = client.shutdown().await;
    Ok(ConnectionDisposition::Denied)
}

#[derive(Clone, Copy)]
struct Denial {
    status: u16,
    reason: &'static str,
}

impl Denial {
    const DIAL_CAPACITY: Self = Self::new(503, "dial-capacity");
    const DNS_CAPACITY: Self = Self::new(503, "dns-capacity");
    const DNS_ANSWER_TOO_LARGE: Self = Self::new(502, "dns-answer-too-large");
    const DNS_EMPTY: Self = Self::new(502, "dns-empty");
    const DNS_FAILED: Self = Self::new(502, "dns-failed");
    const DNS_TIMEOUT: Self = Self::new(504, "dns-timeout");
    const HOST_DENIED: Self = Self::new(403, "host-denied");
    const INVALID_HOSTNAME: Self = Self::new(400, "invalid-hostname");
    const IP_LITERAL_DENIED: Self = Self::new(403, "ip-literal-denied");
    const PORT_DENIED: Self = Self::new(403, "port-denied");
    const PROXY_ENDPOINT_DENIED: Self = Self::new(403, "proxy-endpoint-denied");
    const RESOLVED_ADDRESS_DENIED: Self = Self::new(403, "resolved-address-denied");
    const HEADER_EOF: Self = Self::new(400, "header-eof");
    const HEADER_READ_FAILED: Self = Self::new(400, "header-read-failed");
    const HEADER_TIMEOUT: Self = Self::new(408, "header-timeout");
    const HEADER_TOO_LARGE: Self = Self::new(431, "header-too-large");
    const UPLOAD_LIMIT: Self = Self::new(413, "upload-limit");

    const fn new(status: u16, reason: &'static str) -> Self {
        Self { status, reason }
    }
}

async fn read_connect_header<R>(
    client: &mut R,
    max_bytes: usize,
    deadline: TokioInstant,
) -> Result<HeaderBlock, Denial>
where
    R: AsyncRead + Unpin,
{
    match complete_before_deadline(deadline, read_bounded_header::<4_096, _>(client, max_bytes))
        .await
    {
        Some(Ok(header)) => Ok(header),
        Some(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
            Err(Denial::HEADER_TOO_LARGE)
        }
        Some(Err(error)) if error.kind() == io::ErrorKind::UnexpectedEof => Err(Denial::HEADER_EOF),
        Some(Err(_)) => Err(Denial::HEADER_READ_FAILED),
        None => Err(Denial::HEADER_TIMEOUT),
    }
}

async fn resolve_addresses(
    request: &ConnectRequest,
    state: &LeaseState,
    resolver: &ResolverBackend,
    dns_permits: &Semaphore,
    config: &ProxyConfig,
    handshake_deadline: TokioInstant,
) -> Result<Vec<SocketAddr>, Denial> {
    if let Ok(ip) = request.host.parse::<IpAddr>() {
        let address = SocketAddr::new(ip.to_canonical(), request.port);
        if is_proxy_endpoint(address, config.bind_address) {
            return Err(Denial::PROXY_ENDPOINT_DENIED);
        }
        return state
            .policy
            .allows_ip_literal(ip, &config.nat64_prefixes)
            .then(|| vec![address])
            .ok_or(Denial::IP_LITERAL_DENIED);
    }

    let hostname = canonical_hostname(&request.host).ok_or(Denial::INVALID_HOSTNAME)?;
    if !state.policy.allows_hostname(&hostname) {
        return Err(Denial::HOST_DENIED);
    }
    let dns_deadline = TokioInstant::now()
        .checked_add(state.policy.dns_timeout)
        .unwrap_or(handshake_deadline)
        .min(handshake_deadline);
    let dns_permit = complete_before_deadline(dns_deadline, dns_permits.acquire())
        .await
        .ok_or(Denial::DNS_CAPACITY)?
        .map_err(|_| Denial::DNS_CAPACITY)?;
    let lookup = complete_before_deadline(
        dns_deadline,
        resolver.lookup(&hostname, config.max_resolved_addresses),
    )
    .await;
    drop(dns_permit);
    let addresses = match lookup {
        Some(Ok(addresses)) => addresses,
        Some(Err(_)) => return Err(Denial::DNS_FAILED),
        None => return Err(Denial::DNS_TIMEOUT),
    };
    if addresses.is_empty() {
        return Err(Denial::DNS_EMPTY);
    }
    if addresses.len() > config.max_resolved_addresses {
        return Err(Denial::DNS_ANSWER_TOO_LARGE);
    }

    let mut seen = HashSet::with_capacity(addresses.len());
    let mut approved = Vec::with_capacity(addresses.len());
    for ip in addresses {
        let address = SocketAddr::new(ip.to_canonical(), request.port);
        if is_proxy_endpoint(address, config.bind_address) {
            return Err(Denial::PROXY_ENDPOINT_DENIED);
        }
        if !state.policy.allows_ip(ip, &config.nat64_prefixes) {
            return Err(Denial::RESOLVED_ADDRESS_DENIED);
        }
        if seen.insert(address) {
            approved.push(address);
        }
    }
    Ok(approved)
}

fn is_proxy_endpoint(address: SocketAddr, endpoint: SocketAddr) -> bool {
    if address.port() != endpoint.port() {
        return false;
    }
    let address_ip = address.ip().to_canonical();
    let endpoint_ip = endpoint.ip().to_canonical();
    address_ip == endpoint_ip || endpoint_ip.is_unspecified()
}

async fn deny(
    client: &mut TcpStream,
    state: &LeaseState,
    denial: Denial,
    handshake_deadline: TokioInstant,
) -> io::Result<ConnectionDisposition> {
    let Denial { status, reason } = denial;
    state.record_denial(reason);
    if TokioInstant::now() < handshake_deadline {
        let body = format!("sandbox-egress denied: {reason}\n");
        let response = format!(
            "HTTP/1.1 {status} Denied\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = client.try_write(response.as_bytes());
    }
    let _ = client.shutdown().await;
    Ok(ConnectionDisposition::Denied)
}

#[derive(Clone, Copy)]
enum Direction {
    Upload,
    Download,
}

#[derive(Debug)]
struct TransferLimitExceeded;

impl std::fmt::Display for TransferLimitExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("transfer byte limit exceeded")
    }
}

impl std::error::Error for TransferLimitExceeded {}

fn is_transfer_limit_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<TransferLimitExceeded>())
        .is_some()
}

struct Metered<T> {
    inner: T,
    counters: Arc<Counters>,
    direction: Direction,
    limit: Option<u64>,
    transferred: u64,
    activity: Option<watch::Sender<TokioInstant>>,
}

impl<T> Metered<T> {
    fn new(
        inner: T,
        counters: Arc<Counters>,
        direction: Direction,
        limit: Option<u64>,
        transferred: u64,
        activity: Option<watch::Sender<TokioInstant>>,
    ) -> Self {
        Self {
            inner,
            counters,
            direction,
            limit,
            transferred,
            activity,
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
        let remaining = self
            .limit
            .map(|limit| limit.saturating_sub(self.transferred));
        let result = match remaining.and_then(|bytes| usize::try_from(bytes).ok()) {
            Some(bytes) if bytes > 0 && bytes < buffer.remaining() => {
                let (result, filled) = {
                    let mut limited = ReadBuf::new(buffer.initialize_unfilled_to(bytes));
                    let result = Pin::new(&mut self.inner).poll_read(context, &mut limited);
                    (result, limited.filled().len())
                };
                if let Poll::Ready(Ok(())) = &result {
                    buffer.advance(filled);
                }
                result
            }
            _ => Pin::new(&mut self.inner).poll_read(context, buffer),
        };
        if let Poll::Ready(Ok(())) = result {
            let bytes = (buffer.filled().len() - before) as u64;
            if bytes > 0
                && let Some(activity) = &self.activity
            {
                activity.send_replace(TokioInstant::now());
            }
            match self.direction {
                Direction::Upload => self.counters.record_upload(bytes),
                Direction::Download => self.counters.record_download(bytes),
            };
            self.transferred = self.transferred.saturating_add(bytes);
            if self.limit.is_some_and(|limit| self.transferred > limit) {
                return Poll::Ready(Err(io::Error::other(TransferLimitExceeded)));
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
mod tests;
