use std::sync::atomic::{AtomicU64, Ordering};

/// A point-in-time usage snapshot. Counters are monotonic.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    /// Connections admitted to this lease.
    pub accepted_connections: u64,
    /// Connections currently owned by this lease.
    pub active_connections: u64,
    /// Connections refused at capacity or denied after admission.
    pub denied_connections: u64,
    /// Tunnels that finished normally.
    pub completed_connections: u64,
    /// Bytes read from the guest.
    pub uploaded_bytes: u64,
    /// Bytes read from upstream destinations.
    pub downloaded_bytes: u64,
}

/// A usage snapshot certified final by successful lease shutdown.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FinalUsage(Usage);

impl FinalUsage {
    /// Access the final counters.
    pub const fn usage(self) -> Usage {
        self.0
    }
}

#[derive(Debug, Default)]
pub(crate) struct Counters {
    accepted: AtomicU64,
    active: AtomicU64,
    denied: AtomicU64,
    completed: AtomicU64,
    uploaded: AtomicU64,
    downloaded: AtomicU64,
}

impl Counters {
    pub(crate) fn snapshot(&self) -> Usage {
        Usage {
            accepted_connections: self.accepted.load(Ordering::Acquire),
            active_connections: self.active.load(Ordering::Acquire),
            denied_connections: self.denied.load(Ordering::Acquire),
            completed_connections: self.completed.load(Ordering::Acquire),
            uploaded_bytes: self.uploaded.load(Ordering::Acquire),
            downloaded_bytes: self.downloaded.load(Ordering::Acquire),
        }
    }

    pub(crate) fn final_snapshot(&self) -> FinalUsage {
        FinalUsage(self.snapshot())
    }

    pub(crate) fn admit(&self) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
        self.active.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn finish(&self, completed: bool) {
        if completed {
            self.completed.fetch_add(1, Ordering::Relaxed);
        }
        self.active.fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn deny(&self) {
        self.denied.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_upload(&self, bytes: u64) -> u64 {
        self.uploaded.fetch_add(bytes, Ordering::Relaxed) + bytes
    }

    pub(crate) fn record_download(&self, bytes: u64) -> u64 {
        self.downloaded.fetch_add(bytes, Ordering::Relaxed) + bytes
    }
}
