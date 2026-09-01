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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::sync::{Semaphore, mpsc as tokio_mpsc, watch};
use tokio::time::{Instant as TokioInstant, sleep, sleep_until, timeout_at};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

use crate::connect::{ConnectRequest, find_header_end, parse_connect};
use crate::diagnostic::{DenialReason, DiagnosticReporter};
use crate::policy::canonical_hostname;
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

        let endpoint = ready_rx
            .recv()
            .map_err(|_| ProxyError::RuntimeStopped)?
            .map_err(ProxyError::Initialization)?;
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
    pub const fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// Attach an immutable policy to one host-authenticated peer identity.
    ///
    /// # Errors
    ///
    /// Returns [`AttachError::IdentityInUse`] until the previous lease has
    /// closed successfully or completed best-effort cleanup. It returns
    /// [`AttachError::LeaseIdExhausted`] rather than reusing a diagnostic
    /// sequence after process-local exhaustion, and
    /// [`AttachError::ProxyStopping`] after proxy-wide shutdown begins.
    pub fn attach(&self, identity: PeerIdentity, policy: Policy) -> Result<Lease, AttachError> {
        let identity = identity.canonical();
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
        reply: tokio::sync::oneshot::Sender<()>,
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
    Revoking,
    Quiesced,
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
    revocation_generation: Mutex<u64>,
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
        Self {
            id,
            identity,
            policy: Arc::new(policy),
            phase: Mutex::new(Phase::Open),
            cancel: CancellationToken::new(),
            tracker: TaskTracker::new(),
            permits: Arc::new(Semaphore::new(max_connections)),
            counters: Arc::new(Counters::default()),
            revocation_generation: Mutex::new(0),
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
        let phase = self.phase.lock().expect("lease phase poisoned");
        if *phase != Phase::Open {
            drop(permit);
            drop(stream);
            self.record_unadmitted_locked(*phase, "lease-revoking");
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

    fn quiesce_if_generation(&self, expected: u64) -> Option<FinalUsage> {
        let mut phase = self.phase.lock().expect("lease phase poisoned");
        if *phase == Phase::Quiesced {
            return Some(self.counters.final_snapshot());
        }
        if *phase != Phase::Revoking {
            return None;
        }
        let generation = self
            .revocation_generation
            .lock()
            .expect("revocation generation poisoned");
        if *generation != expected || expected == u64::MAX {
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
        let phase = self.phase.lock().expect("lease phase poisoned");
        drop(stream);
        self.record_unadmitted_locked(*phase, reason);
    }

    fn record_unadmitted_locked(&self, phase: Phase, reason: &'static str) {
        // The caller's phase lock orders both accounting and generation
        // changes before the final quiescence check.
        if matches!(phase, Phase::Open | Phase::Revoking) {
            self.counters.deny();
            self.report_denial(reason);
            if phase == Phase::Revoking {
                self.note_revoking_arrival_locked();
            }
        }
    }

    fn note_revoking_arrival_locked(&self) {
        let mut generation = self
            .revocation_generation
            .lock()
            .expect("revocation generation poisoned");
        if let Some(next) = generation.checked_add(1) {
            *generation = next;
        }
    }

    fn revocation_generation(&self) -> u64 {
        *self
            .revocation_generation
            .lock()
            .expect("revocation generation poisoned")
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
                .map(ConnectedStream::Direct),
            Self::Upstream(proxy) => connect_via(*proxy, address)
                .await
                .map(ConnectedStream::Proxied),
            #[cfg(test)]
            Self::Test(connector) => connector
                .connect(address)
                .await
                .map(ConnectedStream::Direct),
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

struct ConnectionRuntime {
    resolver: Arc<ResolverBackend>,
    connector: Arc<ConnectorBackend>,
    global_permits: Arc<Semaphore>,
    phase_permits: Arc<PhasePermits>,
    config: Arc<ProxyConfig>,
}

struct PhasePermits {
    dns: Semaphore,
    dial: Semaphore,
}

impl ConnectionRuntime {
    fn dispatch(
        &self,
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
        let Ok(global_permit) = self.global_permits.clone().try_acquire_owned() else {
            state.reject_unadmitted(stream, "global-capacity");
            return;
        };
        let Some((admission, stream)) = state.admit(stream) else {
            drop(global_permit);
            return;
        };
        let resolver = Arc::clone(&self.resolver);
        let phase_permits = Arc::clone(&self.phase_permits);
        let connector = Arc::clone(&self.connector);
        let config = Arc::clone(&self.config);
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
                    &resolver,
                    &phase_permits,
                    &connector,
                    &config,
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
        &self,
        listener: &TcpListener,
        leases: &HashMap<PeerIdentity, Arc<LeaseState>>,
    ) -> bool {
        for _ in 0..ACCEPT_DRAIN_BATCH {
            let accepted = std::future::poll_fn(|context| match listener.poll_accept(context) {
                Poll::Ready(result) => Poll::Ready(Some(result)),
                Poll::Pending => Poll::Ready(None),
            })
            .await;
            match accepted {
                Some(Ok((stream, peer))) => self.dispatch(stream, peer, leases),
                None => return true,
                Some(Err(_)) => return false,
            }
        }
        false
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
        Some(resolver) => Arc::new(resolver),
        None => match build_system_resolver(&config) {
            Ok(resolver) => Arc::new(ResolverBackend::System(Box::new(resolver))),
            Err(error) => {
                let _ = ready.send(Err(format!("resolver initialization failed: {error}")));
                return;
            }
        },
    };
    let system_connector = config
        .upstream_proxy
        .map_or(ConnectorBackend::Direct, ConnectorBackend::Upstream);
    let connector = Arc::new(connector.unwrap_or(system_connector));
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
    if ready.send(Ok(endpoint)).is_err() {
        return;
    }

    let mut leases: HashMap<PeerIdentity, Arc<LeaseState>> = HashMap::new();
    let connections = ConnectionRuntime {
        global_permits: Arc::new(Semaphore::new(config.max_connections)),
        phase_permits: Arc::new(PhasePermits {
            dns: Semaphore::new(config.max_concurrent_dns),
            dial: Semaphore::new(config.max_concurrent_dials),
        }),
        resolver,
        connector,
        config: Arc::new(config),
    };

    let mut stopping = false;
    loop {
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
                        } else if connections.drain_ready(&listener, &leases).await {
                            leases.insert(state.identity.clone(), state);
                            let _ = reply.send(Ok(()));
                        } else {
                            let _ = command_sender.send(Command::Attach { state, reply });
                        }
                    }
                    Command::Close { state, deadline, reply } => {
                        state.begin_close();
                        spawn_close_wait(
                            state,
                            connections.config.identity_reuse_quiet_period,
                            deadline,
                            reply,
                            Some(command_sender.clone()),
                        );
                    }
                    Command::Reap { state } => {
                        state.begin_close();
                        let command_sender = command_sender.clone();
                        let drain_sender = command_sender.clone();
                        let quiet_period = connections.config.identity_reuse_quiet_period;
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
                        if drained {
                            let _ = reply.send(());
                        } else {
                            let _ = command_sender.send(Command::DrainAcceptQueue { reply });
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
            accepted = listener.accept(), if !stopping => {
                if let Ok((stream, peer)) = accepted {
                    connections.dispatch(stream, peer, &leases);
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
    drained.await.map_err(|_| CloseErrorKind::RuntimeStopped)
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
    let Some((handshake_deadline, header_deadline)) = connection_deadlines(
        accepted_at,
        state.policy.handshake_timeout,
        config.header_timeout,
    ) else {
        return deny(&mut client, state, 500, "invalid-handshake-deadline").await;
    };
    let header =
        match read_connect_header(&mut client, config.max_header_bytes, header_deadline).await {
            Ok(header) => header,
            Err(denial) => return deny(&mut client, state, denial.status, denial.reason).await,
        };

    let request = match parse_connect(&header.bytes[..header.end]) {
        Ok(request) => request,
        Err(reason) => return deny(&mut client, state, 400, reason).await,
    };
    if !state.policy.allows_port(request.port) {
        return deny(&mut client, state, 403, "port-denied").await;
    }
    let buffered_upload = header.bytes.len() - header.end;
    if state
        .policy
        .max_upload_bytes
        .is_some_and(|limit| buffered_upload as u64 > limit)
    {
        state.counters.record_upload(buffered_upload as u64);
        return deny(&mut client, state, 413, "upload-limit").await;
    }

    let addresses = match resolve_addresses(
        &request,
        state,
        resolver,
        &phase_permits.dns,
        config,
        handshake_deadline,
    )
    .await
    {
        Ok(addresses) => addresses,
        Err(denial) => return deny(&mut client, state, denial.status, denial.reason).await,
    };

    let upstream = match dial_with_budget(
        addresses,
        connector,
        &phase_permits.dial,
        handshake_deadline,
    )
    .await
    {
        Ok(upstream) => upstream,
        Err(denial) => return deny(&mut client, state, denial.status, denial.reason).await,
    };
    let Some(upstream) = upstream else {
        return deny(&mut client, state, 502, connector.failure_reason()).await;
    };

    match upstream {
        ConnectedStream::Direct(upstream) => {
            establish_tunnel(
                client,
                upstream,
                state,
                &request,
                header,
                config,
                handshake_deadline,
            )
            .await
        }
        ConnectedStream::Proxied(upstream) => {
            establish_tunnel(
                client,
                upstream,
                state,
                &request,
                header,
                config,
                handshake_deadline,
            )
            .await
        }
    }
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
        return reject_tunnel(&mut client, state, "connect-response-timeout").await;
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
            Err(reason) => return reject_tunnel(&mut client, state, reason).await,
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
                Some(Err(reason)) => return reject_tunnel(&mut client, state, reason).await,
                None => return reject_tunnel(&mut client, state, "client-hello-timeout").await,
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
        let deadline = observed
            .checked_add(timeout)
            .expect("validated idle timeout deadline");
        sleep_until(deadline).await;
        if !activity
            .has_changed()
            .expect("tunnel activity sender is retained")
        {
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
    handshake_deadline: TokioInstant,
) -> Option<ConnectedStream> {
    let mut addresses = addresses.into_iter();
    while let Some(address) = addresses.next() {
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
    handshake_deadline: TokioInstant,
) -> Result<Option<ConnectedStream>, Denial> {
    let permit = complete_before_deadline(handshake_deadline, permits.acquire())
        .await
        .ok_or(Denial::DIAL_CAPACITY)?
        .map_err(|_| Denial::DIAL_CAPACITY)?;
    let upstream = dial_approved_addresses(addresses, connector, handshake_deadline).await;
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

async fn reject_tunnel(
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
    const PROXY_ENDPOINT_DENIED: Self = Self::new(403, "proxy-endpoint-denied");
    const RESOLVED_ADDRESS_DENIED: Self = Self::new(403, "resolved-address-denied");
    const HEADER_EOF: Self = Self::new(400, "header-eof");
    const HEADER_READ_FAILED: Self = Self::new(400, "header-read-failed");
    const HEADER_TIMEOUT: Self = Self::new(408, "header-timeout");
    const HEADER_TOO_LARGE: Self = Self::new(431, "header-too-large");

    const fn new(status: u16, reason: &'static str) -> Self {
        Self { status, reason }
    }
}

async fn read_connect_header(
    client: &mut TcpStream,
    max_bytes: usize,
    deadline: TokioInstant,
) -> Result<HeaderBlock, Denial> {
    match complete_before_deadline(deadline, read_header(client, max_bytes)).await {
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
        let address = SocketAddr::new(ip, request.port);
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
        let address = SocketAddr::new(ip, request.port);
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
    address_ip == endpoint_ip
        || endpoint_ip.is_unspecified()
            && address_ip.is_loopback()
            && (endpoint.is_ipv6() || address.is_ipv4())
}

struct HeaderBlock {
    bytes: Vec<u8>,
    end: usize,
}

// Keep the hostile-input scan behind a stable code-generation boundary.
// Whole-program LTO otherwise coupled its loop layout to unrelated policy
// constructor changes; the committed 1 MiB benchmark reproduced the effect.
#[inline(never)]
async fn read_header<R>(stream: &mut R, max: usize) -> io::Result<HeaderBlock>
where
    R: AsyncRead + Unpin,
{
    // Keep ordinary CONNECT requests in one allocation without reserving a
    // full read chunk for every concurrent handshake.
    let mut bytes = Vec::with_capacity(max.min(1_024));
    let mut chunk = [0_u8; 4_096];
    loop {
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
        let scan_from = bytes.len().saturating_sub(3);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_header_end(&bytes, scan_from) {
            return Ok(HeaderBlock { bytes, end });
        }
    }
}

async fn deny(
    client: &mut TcpStream,
    state: &LeaseState,
    status: u16,
    reason: &'static str,
) -> io::Result<ConnectionDisposition> {
    state.record_denial(reason);
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
mod tests {
    use super::*;

    mod deadlines;
    mod dial_budget;
    mod dns_wire;

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

    struct FailingResolver;

    struct CapturingResolver(mpsc::Sender<String>);

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
            0xc0, 0x0c, 0, 6, 0, 1, 0, 0, 0, 60, 0, 24, 0xc0, 0x0c, 0xc0, 0x0c, 0, 0, 0, 1, 0, 0,
            0, 60, 0, 0, 0, 60, 0, 0, 0, 60, 0, 0, 0, 60, 0, 0, 0, 60,
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
        let proxy = Proxy::start_with_test_connector(ProxyConfig::default(), connector)
            .expect("start proxy");
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
            TokioInstant::now() + Duration::from_millis(400),
        ));
        assert!(connected.is_some(), "reachable fallback was not attempted");
        assert_eq!(
            *attempts.lock().expect("attempt list poisoned"),
            vec![first, second]
        );
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
            let mut client = std::net::TcpStream::connect(lease.endpoint().socket_addr())
                .expect("connect proxy");
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
    fn dns_resolver_failure_remains_distinct_and_never_dials() {
        let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
        let proxy = Proxy::start_with_test_backends(
            ProxyConfig::default(),
            Arc::new(FailingResolver),
            connector,
        )
        .expect("start DNS failure proxy");
        let lease = proxy
            .attach(
                PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
                hostname_policy("failed.test", 443),
            )
            .expect("attach DNS failure lease");
        let mut client =
            std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        std::io::Write::write_all(
            &mut client,
            b"CONNECT failed.test:443 HTTP/1.1\r\nHost: failed.test\r\n\r\n",
        )
        .expect("write CONNECT");

        let mut response = String::new();
        std::io::Read::read_to_string(&mut client, &mut response).expect("read DNS failure denial");
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(response.contains("dns-failed"), "{response}");
        assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
        assert_eq!(lease.usage().denied_connections, 1);
        lease
            .close(Instant::now() + Duration::from_secs(1))
            .expect("close DNS failure lease");
        proxy
            .shutdown(Instant::now() + Duration::from_secs(1))
            .expect("proxy shutdown");
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
        let socket =
            std::net::UdpSocket::bind(dns_address).expect("bind late-response UDP DNS server");
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
        let mut second_client =
            std::net::TcpStream::connect(endpoint).expect("connect second lease");
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
            let mut client = std::net::TcpStream::connect(lease.endpoint().socket_addr())
                .expect("connect proxy");
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
        std::io::Read::read_to_string(&mut client, &mut response)
            .expect("read proxy endpoint denial");

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
    fn proxy_endpoint_matching_covers_transport_spellings_and_wildcard_loopback() {
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
        assert!(!is_proxy_endpoint(
            "93.184.216.34:4750".parse().expect("remote endpoint"),
            "[::]:4750".parse().expect("dual-stack wildcard"),
        ));
        assert!(!is_proxy_endpoint(
            "127.0.0.1:4751".parse().expect("different port"),
            endpoint,
        ));
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
            let mut client = std::net::TcpStream::connect(lease.endpoint().socket_addr())
                .expect("connect proxy");
            let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
            std::io::Write::write_all(&mut client, request.as_bytes()).expect("write CONNECT");
            let mut response = String::new();
            std::io::Read::read_to_string(&mut client, &mut response)
                .expect("read hostname denial");

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
    fn legacy_numeric_host_spellings_cannot_bypass_the_resolved_address_floor() {
        let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for hostname in ["127.1", "0177.0.0.1", "0x7f000001", "2130706433"] {
            assert!(hostname.parse::<IpAddr>().is_err(), "{hostname}");
            let resolver = Arc::new(FixedAnswerResolver(vec![IpAddr::V4(
                std::net::Ipv4Addr::LOCALHOST,
            )]));
            let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
            let proxy =
                Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
                    .expect("start proxy");
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
            let mut client = std::net::TcpStream::connect(lease.endpoint().socket_addr())
                .expect("connect proxy");
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
    fn close_cancels_an_in_progress_dial() {
        let port = 19_443;
        let (entered_tx, entered_rx) = mpsc::channel();
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connector = Arc::new(PendingConnector {
            entered: entered_tx,
            active: Arc::clone(&active),
        });
        let proxy = Proxy::start_with_test_connector(ProxyConfig::default(), connector)
            .expect("start proxy");
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
        let proxy = Proxy::start_with_test_connector(ProxyConfig::default(), connector)
            .expect("start proxy");
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
        let proxy = Proxy::start_with_test_connector(ProxyConfig::default(), connector)
            .expect("start proxy");
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
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(response.contains("dial-failed"), "{response}");
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
            assert!(response.starts_with("HTTP/1.1 408"), "{response}");
            assert!(response.contains("header-timeout"), "{response}");
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
            let close =
                quiesce_after_identity_quiet(&state, Duration::from_secs(1), Some(&commands));
            let drain = async {
                let Command::DrainAcceptQueue { reply } =
                    receiver.recv().await.expect("accept-drain command")
                else {
                    panic!("unexpected command while retrying close");
                };
                reply.send(()).expect("acknowledge accept drain");
            };
            let (usage, ()) = tokio::join!(close, drain);
            usage.expect("quiesced retry")
        });

        assert_eq!(actual, expected);
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
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                .as_bytes(),
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
                .block_on(read_header(&mut input, 8_192))
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
            .block_on(read_header(&mut exact.as_slice(), LIMIT))
            .expect("terminator ending at the byte limit");
        assert_eq!(header.end, LIMIT);

        let mut over = vec![b'a'; LIMIT - 3];
        over.extend_from_slice(b"\r\n\r\n");
        let Err(error) = runtime.block_on(read_header(&mut over.as_slice(), LIMIT)) else {
            panic!("accepted terminator ending beyond the byte limit");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
