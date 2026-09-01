use std::sync::atomic::{AtomicU64, Ordering};

/// A point-in-time usage snapshot.
///
/// Cumulative counters are monotonic and saturate at [`u64::MAX`]. The active
/// connection gauge rises and falls with admitted work.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
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
        saturating_add(&self.accepted, 1);
        self.active.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn finish(&self, completed: bool) {
        if completed {
            saturating_add(&self.completed, 1);
        }
        self.active.fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn deny(&self) {
        saturating_add(&self.denied, 1);
    }

    pub(crate) fn record_upload(&self, bytes: u64) -> u64 {
        saturating_add(&self.uploaded, bytes)
    }

    pub(crate) fn record_download(&self, bytes: u64) -> u64 {
        saturating_add(&self.downloaded, bytes)
    }
}

fn saturating_add(counter: &AtomicU64, amount: u64) -> u64 {
    let previous = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(amount))
        })
        .expect("saturating update never rejects a value");
    previous.saturating_add(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_counters_saturate_instead_of_wrapping() {
        let counters = Counters::default();
        counters.accepted.store(u64::MAX, Ordering::Relaxed);
        counters.denied.store(u64::MAX, Ordering::Relaxed);
        counters.completed.store(u64::MAX, Ordering::Relaxed);

        counters.admit();
        counters.deny();
        counters.finish(true);

        let usage = counters.snapshot();
        assert_eq!(usage.accepted_connections, u64::MAX);
        assert_eq!(usage.denied_connections, u64::MAX);
        assert_eq!(usage.completed_connections, u64::MAX);
        assert_eq!(usage.active_connections, 0);
    }

    #[test]
    fn byte_counters_saturate_and_return_the_stored_total() {
        let counters = Counters::default();
        counters.uploaded.store(u64::MAX - 2, Ordering::Relaxed);
        counters.downloaded.store(u64::MAX - 1, Ordering::Relaxed);

        assert_eq!(counters.record_upload(4), u64::MAX);
        assert_eq!(counters.record_download(2), u64::MAX);
        assert_eq!(counters.snapshot().uploaded_bytes, u64::MAX);
        assert_eq!(counters.snapshot().downloaded_bytes, u64::MAX);
    }
}
