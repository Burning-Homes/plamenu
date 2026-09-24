//! In-process native processing slots and conservative temporary-file budgets.
//! These do not replace durable quotas for originals and stored renditions, or
//! filesystem free-space checks: other processes can consume the same disk.

use std::sync::{Arc, Mutex};

use crate::sync::RecoverableMutex as _;
use crate::{media_limits::FileSizeLimit, media_transcode::POSTER_FILE_LIMIT_BYTES};

#[derive(Default)]
struct Usage {
    jobs: usize,
    bytes: u64,
}

struct Inner {
    max_jobs: usize,
    max_bytes: u64,
    usage: Mutex<Usage>,
}

/// Share one pool across all native worker slots on this process. Reservations
/// are nonblocking: a full pool must not claim a database job and let it expire
/// while waiting for scratch space.
#[derive(Clone)]
pub struct ProcessingPool(Arc<Inner>);

impl ProcessingPool {
    #[must_use]
    pub fn new(max_jobs: usize, max_bytes: u64) -> Self {
        Self(Arc::new(Inner {
            max_jobs,
            max_bytes,
            usage: Mutex::new(Usage::default()),
        }))
    }

    /// Cover the source spool, three encoded files (MP4 + up to two HLS
    /// renditions) plus possible destination copies, and poster/hash scratch.
    /// Every encoder output is independently kernel-bounded by `limit`.
    #[must_use]
    pub fn try_reserve(&self, limit: FileSizeLimit, source_ceiling: u64) -> Option<Reservation> {
        if limit.bytes == 0 || source_ceiling == 0 {
            return None;
        }
        let poster_limit = limit.bytes.min(POSTER_FILE_LIMIT_BYTES);
        let bytes = source_ceiling
            .checked_add(limit.bytes.checked_mul(6)?)?
            .checked_add(poster_limit.checked_mul(3)?)?;
        let mut usage = self.0.usage.lock_or_recover();
        if usage.jobs >= self.0.max_jobs || bytes > self.0.max_bytes.saturating_sub(usage.bytes) {
            return None;
        }
        usage.jobs += 1;
        usage.bytes += bytes;
        Some(Reservation {
            pool: Arc::clone(&self.0),
            bytes,
            source_ceiling,
            limit,
        })
    }

    /// Active jobs and conservatively reserved temporary bytes, not stored
    /// media usage. Exposed for scheduling/diagnostics without a database read.
    #[must_use]
    pub fn usage(&self) -> (usize, u64) {
        let usage = self.0.usage.lock_or_recover();
        (usage.jobs, usage.bytes)
    }
}

/// Cannot be cloned or constructed without admission. The worker moves it into
/// final storage so dropping the original caller cannot free a still-used slot.
pub struct Reservation {
    pool: Arc<Inner>,
    bytes: u64,
    pub(crate) source_ceiling: u64,
    pub(crate) limit: FileSizeLimit,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut usage = self.pool.usage.lock_or_recover();
        usage.jobs -= 1;
        usage.bytes -= self.bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(bytes: u64) -> FileSizeLimit {
        FileSizeLimit {
            launcher: "plamenu".into(),
            bytes,
        }
    }

    #[test]
    fn admission_counts_copy_and_poster_peaks_and_releases_on_drop() {
        let pool = ProcessingPool::new(2, 200);
        let a = pool.try_reserve(limit(10), 10).unwrap();
        assert_eq!(pool.usage(), (1, 100));
        assert!(pool.try_reserve(limit(11), 10).is_none());
        let b = pool.try_reserve(limit(10), 10).unwrap();
        assert!(pool.try_reserve(limit(1), 1).is_none());
        assert_eq!(pool.usage(), (2, 200));
        drop(a);
        drop(b);
        assert_eq!(pool.usage(), (0, 0));
    }

    #[test]
    fn zero_and_overflow_budgets_fail_closed() {
        let pool = ProcessingPool::new(1, u64::MAX);
        assert!(pool.try_reserve(limit(0), 1).is_none());
        assert!(pool.try_reserve(limit(1), 0).is_none());
        assert!(pool.try_reserve(limit(u64::MAX), 1).is_none());
        assert!(pool.try_reserve(limit(1), u64::MAX).is_none());
        assert!(
            ProcessingPool::new(0, 100)
                .try_reserve(limit(1), 1)
                .is_none()
        );
        assert_eq!(pool.usage(), (0, 0));
    }

    #[test]
    fn concurrent_claims_share_one_budget() {
        let pool = ProcessingPool::new(8, 100);
        let rendezvous = Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let pool = pool.clone();
                let rendezvous = Arc::clone(&rendezvous);
                std::thread::spawn(move || {
                    let reserved = pool.try_reserve(limit(10), 10);
                    rendezvous.wait();
                    reserved.is_some()
                })
            })
            .collect::<Vec<_>>();
        let admitted = threads
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum::<usize>();
        assert_eq!(admitted, 1);
        assert_eq!(pool.usage(), (0, 0));
    }
}
