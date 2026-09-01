use std::fmt;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::PeerIdentity;

const DIAGNOSTIC_WINDOW: Duration = Duration::from_secs(1);

/// A stable, bounded machine reason for a denied connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DenialReason(&'static str);

impl DenialReason {
    pub(crate) const fn new(code: &'static str) -> Self {
        Self(code)
    }

    /// Return the lowercase, hyphen-separated reason code.
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for DenialReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// A nonblocking operational event emitted for a denied connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticEvent {
    /// Proxy-assigned sequence that distinguishes reuse of the same identity.
    pub lease_id: u64,
    /// Host-authenticated identity that owned the denied connection.
    pub identity: PeerIdentity,
    /// Stable denial code. This never contains guest-provided text.
    pub reason: DenialReason,
    /// Events suppressed by rate or channel capacity before this event.
    pub suppressed_before: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct DiagnosticConfig {
    pub(crate) sender: mpsc::SyncSender<DiagnosticEvent>,
    pub(crate) max_events_per_second: u32,
}

#[derive(Clone, Default)]
pub(crate) struct DiagnosticReporter(Option<Arc<ReporterInner>>);

impl fmt::Debug for DiagnosticReporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticReporter")
            .field("enabled", &self.0.is_some())
            .finish()
    }
}

struct ReporterInner {
    sender: mpsc::SyncSender<DiagnosticEvent>,
    max_events_per_second: u32,
    state: Mutex<RateState>,
}

struct RateState {
    window_started: Instant,
    emitted: u32,
    suppressed: u64,
}

impl DiagnosticReporter {
    pub(crate) fn new(config: Option<&DiagnosticConfig>) -> Self {
        Self(config.map(|config| {
            Arc::new(ReporterInner {
                sender: config.sender.clone(),
                max_events_per_second: config.max_events_per_second,
                state: Mutex::new(RateState {
                    window_started: Instant::now(),
                    emitted: 0,
                    suppressed: 0,
                }),
            })
        }))
    }

    pub(crate) fn report(&self, lease_id: u64, identity: PeerIdentity, reason: DenialReason) {
        self.report_at(lease_id, identity, reason, Instant::now());
    }

    fn report_at(&self, lease_id: u64, identity: PeerIdentity, reason: DenialReason, now: Instant) {
        let Some(inner) = &self.0 else { return };
        let event = {
            let mut state = inner.state.lock().expect("diagnostic rate state poisoned");
            if now.saturating_duration_since(state.window_started) >= DIAGNOSTIC_WINDOW {
                state.window_started = now;
                state.emitted = 0;
            }
            if state.emitted >= inner.max_events_per_second {
                state.suppressed = state.suppressed.saturating_add(1);
                return;
            }
            state.emitted += 1;
            DiagnosticEvent {
                lease_id,
                identity,
                reason,
                suppressed_before: std::mem::take(&mut state.suppressed),
            }
        };
        match inner.sender.try_send(event) {
            Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => {}
            Err(mpsc::TrySendError::Full(event)) => {
                let mut state = inner.state.lock().expect("diagnostic rate state poisoned");
                state.suppressed = state
                    .suppressed
                    .saturating_add(event.suppressed_before)
                    .saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::thread;

    use super::*;

    const REASON: DenialReason = DenialReason::new("test-denial");
    const IDENTITY: PeerIdentity = PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST));
    const LEASE_ID: u64 = 7;

    #[test]
    fn rate_limit_reports_suppression_in_the_next_window() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let reporter = DiagnosticReporter::new(Some(&DiagnosticConfig {
            sender,
            max_events_per_second: 2,
        }));
        let started = Instant::now();

        reporter.report_at(LEASE_ID, IDENTITY.clone(), REASON, started);
        reporter.report_at(LEASE_ID, IDENTITY.clone(), REASON, started);
        reporter.report_at(LEASE_ID, IDENTITY.clone(), REASON, started);
        reporter.report_at(LEASE_ID, IDENTITY, REASON, started + DIAGNOSTIC_WINDOW);

        let events: Vec<_> = receiver.try_iter().collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].suppressed_before, 0);
        assert_eq!(events[1].suppressed_before, 0);
        assert_eq!(events[2].suppressed_before, 1);
    }

    #[test]
    fn full_channel_is_nonblocking_and_counted_as_suppressed() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let reporter = DiagnosticReporter::new(Some(&DiagnosticConfig {
            sender,
            max_events_per_second: 2,
        }));
        let started = Instant::now();

        reporter.report_at(LEASE_ID, IDENTITY.clone(), REASON, started);
        reporter.report_at(LEASE_ID, IDENTITY, REASON, started);
        assert_eq!(receiver.recv().expect("first event").suppressed_before, 0);
        reporter.report_at(LEASE_ID, IDENTITY, REASON, started + DIAGNOSTIC_WINDOW);

        assert_eq!(
            receiver
                .recv()
                .expect("next-window event")
                .suppressed_before,
            1
        );
    }

    #[test]
    fn concurrent_reporters_share_one_exact_process_limit() {
        const THREADS: usize = 8;
        const EVENTS_PER_THREAD: usize = 100;
        const LIMIT: u32 = 100;

        let (sender, receiver) = mpsc::sync_channel(usize::try_from(LIMIT).unwrap() + 1);
        let reporter = DiagnosticReporter::new(Some(&DiagnosticConfig {
            sender,
            max_events_per_second: LIMIT,
        }));
        let started = Instant::now();
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let reporter = reporter.clone();
                thread::spawn(move || {
                    for _ in 0..EVENTS_PER_THREAD {
                        reporter.report_at(LEASE_ID, IDENTITY.clone(), REASON, started);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("reporter thread");
        }
        reporter.report_at(LEASE_ID, IDENTITY, REASON, started + DIAGNOSTIC_WINDOW);

        let events: Vec<_> = receiver.try_iter().collect();
        assert_eq!(events.len(), usize::try_from(LIMIT).unwrap() + 1);
        assert!(
            events[..events.len() - 1]
                .iter()
                .all(|event| event.suppressed_before == 0)
        );
        assert_eq!(
            events.last().expect("next-window event").suppressed_before,
            u64::try_from(THREADS * EVENTS_PER_THREAD).unwrap() - u64::from(LIMIT)
        );
    }
}
