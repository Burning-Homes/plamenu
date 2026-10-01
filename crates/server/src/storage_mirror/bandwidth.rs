//! One finite byte schedule shared by copying and live ingress, not one timer
//! per publisher. Reserve a conservative wall-clock share for live re-encoding.
use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

const REENCODE_BYTES_PER_SECOND: u64 = 2 * 1024 * 1024;
// The bounded 256-KiB playlist / two-second tick plus offset/directory metadata.
const COPY_METADATA_BYTES_PER_SECOND: u64 = 256 * 1024;
const MAX_QUEUE: Duration = Duration::from_secs(2);
struct Schedule {
    due: Instant,
    reserved: u64,
}
pub(crate) struct Bandwidth {
    bytes_per_second: u64,
    schedule: Mutex<Schedule>,
}
pub(crate) struct Reservation(Arc<Bandwidth>, u64);
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut schedule = self.0.schedule();
        schedule.reserved = schedule.reserved.saturating_sub(self.1);
    }
}
impl Bandwidth {
    fn schedule(&self) -> std::sync::MutexGuard<'_, Schedule> {
        // Preserve outstanding admission on poison; resetting to a default
        // schedule would incorrectly refund still-owned encoder reservations.
        self.schedule
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(super) fn new(bytes_per_second: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes_per_second,
            schedule: Mutex::new(Schedule {
                due: Instant::now(),
                reserved: 0,
            }),
        })
    }
    pub(super) fn reserve_live(self: &Arc<Self>, reencode: bool) -> io::Result<Reservation> {
        let mut schedule = self.schedule();
        let rate = if reencode {
            REENCODE_BYTES_PER_SECOND
        } else {
            COPY_METADATA_BYTES_PER_SECOND
        };
        let next = schedule.reserved.saturating_add(rate);
        if next > self.bytes_per_second || (!reencode && next == self.bytes_per_second) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        schedule.reserved = next;
        Ok(Reservation(self.clone(), rate))
    }
    pub(crate) async fn transfer(&self, bytes: u64) -> io::Result<()> {
        let due = {
            let mut schedule = self.schedule();
            let rate = self.bytes_per_second.saturating_sub(schedule.reserved);
            if rate == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let cost = Duration::from_secs(bytes / rate)
                + Duration::from_nanos((bytes % rate).saturating_mul(1_000_000_000) / rate);
            let now = Instant::now();
            let next = schedule
                .due
                .max(now)
                .checked_add(cost)
                .ok_or(io::ErrorKind::InvalidInput)?;
            if next.saturating_duration_since(now) > MAX_QUEUE {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            schedule.due = next;
            next
        };
        tokio::time::sleep_until(due).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn simultaneous_copies_and_live_tags_share_one_schedule() {
        let budget = Bandwidth::new(1024 * 1024);
        let began = Instant::now();
        let (first, second, third) = tokio::join!(
            budget.transfer(64 * 1024),
            budget.transfer(64 * 1024),
            budget.transfer(64 * 1024)
        );
        first.unwrap();
        second.unwrap();
        third.unwrap();
        assert!(began.elapsed() >= Duration::from_millis(187));
        assert!(budget.transfer(3 * 1024 * 1024).await.is_err());
        // A refused reservation must not poison the next request's schedule.
        budget.transfer(1024).await.unwrap();
    }
    #[tokio::test]
    async fn reencode_reservations_are_owned_and_leave_only_the_unreserved_rate() {
        let budget = Bandwidth::new(3 * 1024 * 1024);
        let lease = budget.reserve_live(true).unwrap();
        assert!(budget.reserve_live(true).is_err());
        let began = Instant::now();
        budget.transfer(128 * 1024).await.unwrap();
        assert!(began.elapsed() >= Duration::from_millis(125));
        drop(lease);
        let began = Instant::now();
        budget.transfer(128 * 1024).await.unwrap();
        assert!(began.elapsed() >= Duration::from_millis(41));
        assert_eq!(budget.schedule().reserved, 0);
        let exact = Bandwidth::new(REENCODE_BYTES_PER_SECOND);
        let lease = exact.reserve_live(true).unwrap();
        assert!(exact.transfer(1).await.is_err());
        drop(lease);
        exact.transfer(1).await.unwrap();
    }
    #[test]
    fn copy_publishers_reserve_metadata_but_cannot_take_the_last_payload_share() {
        let budget = Bandwidth::new(1024 * 1024);
        let a = budget.reserve_live(false).unwrap();
        let b = budget.reserve_live(false).unwrap();
        let c = budget.reserve_live(false).unwrap();
        assert!(budget.reserve_live(false).is_err());
        assert_eq!(
            budget.schedule().reserved,
            3 * COPY_METADATA_BYTES_PER_SECOND
        );
        drop((a, b, c));
        assert_eq!(budget.schedule().reserved, 0);
    }
}
