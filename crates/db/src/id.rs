//! Time-ordered snowflake IDs, Mastodon style: 48 bits of unix milliseconds
//! shifted left over 15 sequence bits and one writer-lane bit. Sortable by creation
//! time within one process lifetime, with a positive signed range until ~6430.
//! Sequence exhaustion and clock rollback advance logical time; the timestamp
//! can therefore lead the wall clock. Restarts do not share this state.
//!
//! The server uses even IDs and CLI commands use odd IDs, so online administration
//! cannot collide with the serving process. Separate session advisory locks
//! enforce one server and one CLI command per database (see [`crate::single_writer`]).
//! Each lane supports 32,768 IDs per logical millisecond. Lifting this to multi-server
//! means moving generation into Postgres the way Mastodon's `timestamp_id()`
//! function does.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static LAST_ID: AtomicI64 = AtomicI64::new(0);

/// Selects the CLI's odd-ID lane before any allocation in this process.
/// The caller must hold the CLI writer lock for the whole command lifetime.
pub fn initialize_cli_lane() -> Result<(), &'static str> {
    select_cli_lane(&LAST_ID)
}

fn select_cli_lane(last: &AtomicI64) -> Result<(), &'static str> {
    last.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
        .map(|_| ())
        .map_err(|_| "the CLI ID lane must be selected before allocating IDs")
}

/// Generates the next snowflake ID.
#[must_use]
pub fn next() -> i64 {
    let ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_millis(),
    )
    .expect("system clock implausibly far in the future");
    allocate(&LAST_ID, ms)
}

// One atomic state coordinates the timestamp and sequence. Overflow carries
// into the next logical millisecond, rather than wrapping the sequence to zero.
fn allocate(last: &AtomicI64, unix_ms: i64) -> i64 {
    let floor = unix_ms
        .checked_mul(1 << 16)
        .filter(|value| *value >= 0)
        .expect("system clock outside the snowflake timestamp range");
    let previous = last
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
            previous
                .checked_add(2)
                .map(|next| next.max(floor | (previous & 1)))
        })
        .expect("snowflake ID range exhausted");
    (previous + 2).max(floor | (previous & 1))
}

/// The smallest snowflake ID that could have been minted at `unix_ms` (sequence
/// bits zeroed). Mirrors Mastodon's `Snowflake.id_at(time, with_random: false)`,
/// used to translate a time window into an `id BETWEEN lo AND hi` range over the
/// time-ordered `statuses.id` for the admin metrics queries. The matching upper
/// bound is `id_at(end_ms) | 0xFFFF` (all sequence bits set).
#[must_use]
pub fn id_at(unix_ms: i64) -> i64 {
    unix_ms << 16
}

/// The logical ingest time encoded in a snowflake ID, millisecond precision
/// (potentially ahead of wall time after sequence exhaustion or clock rollback) (the inverse of [`id_at`], Mastodon's `Snowflake.to_time`).
#[must_use]
pub fn time_of(id: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(id >> 16) * 1_000_000)
        .expect("snowflake ids stay within the representable time range")
}

#[cfg(test)]
mod tests {
    use super::next;

    #[test]
    fn ids_are_positive_and_strictly_increasing_within_a_millisecond() {
        let ids: Vec<i64> = (0..1000).map(|_| next()).collect();
        assert!(ids.iter().all(|&id| id > 0));
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids must be unique");
        assert_eq!(sorted, ids, "ids must be monotonically increasing");
    }

    #[test]
    fn id_embeds_current_time() {
        let before_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let id_ms = next() >> 16;
        assert!((id_ms - before_ms).abs() < 1000);
    }

    #[test]
    fn sequence_boundary_and_clock_rollback_preserve_order() {
        use super::{AtomicI64, allocate};
        for lane in [0, 1] {
            let last = AtomicI64::new((100 << 16) | 0xfffc | lane);
            assert_eq!(allocate(&last, 100), (100 << 16) | 0xfffe | lane);
            assert_eq!(allocate(&last, 100), (101 << 16) | lane);
            assert_eq!(allocate(&last, 99), (101 << 16) | 2 | lane);
            assert_eq!(allocate(&last, 102), (102 << 16) | lane);
        }
    }

    #[test]
    fn concurrent_allocations_cross_multiple_sequence_boundaries() {
        use super::{AtomicI64, allocate};
        let last = AtomicI64::new(0);
        let mut ids = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        (0..70_000)
                            .map(|_| allocate(&last, 100))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| {
                    let ids = handle.join().unwrap();
                    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
                    ids
                })
                .collect::<Vec<_>>()
        });
        ids.sort_unstable();
        assert_eq!(ids.len(), 280_000);
        assert!(ids.windows(2).all(|pair| pair[1] == pair[0] + 2));
    }

    #[test]
    fn server_and_cli_ids_remain_disjoint_across_clock_changes_and_overflow() {
        use super::{AtomicI64, allocate, select_cli_lane};
        let server = AtomicI64::new(0);
        let cli = AtomicI64::new(0);
        select_cli_lane(&cli).unwrap();
        for millis in [100, 101, 99, 200] {
            for _ in 0..70_000 {
                let serving = allocate(&server, millis);
                let command = allocate(&cli, millis);
                assert_eq!(serving & 1, 0);
                assert_eq!(command & 1, 1);
                assert_ne!(serving, command);
            }
        }
        assert!(select_cli_lane(&server).is_err());
        assert!(select_cli_lane(&cli).is_err());
    }
}
