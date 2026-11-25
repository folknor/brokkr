//! The boot clock, `CLOCK_BOOTTIME`: monotonic and counting through system
//! suspend. It is the clock of a measured child's own timeline - the FIFO
//! marker contract's stamps (pbfhogg derives its zero from procfs process
//! age, which counts on it), the `/proc` sample stamps, the clock check's
//! bound - and of the host trace. `std::time::Instant` stops in suspend, so
//! a quantity on one clock is never valid on the other once a run spans one.
//!
//! Three types keep the quantities apart, so the join points where the
//! clocks meet take only the right one:
//!
//! - [`BootInstant`]: an absolute reading, microseconds since boot. Made by
//!   [`BootInstant::now`], or decoded from a stored absolute stamp.
//! - [`BootElapsed`]: a non-negative span between two readings.
//! - [`ChildUs`]: a stamp on a child's clock, microseconds from its boot
//!   origin.
//!
//! None converts from an `Instant` or a `Duration`. Benchmark walls,
//! deadlines and ceilings stay `Instant` and `Duration`: a wall that silently
//! absorbed a suspend would misstate the measurement.

use std::time::{Duration, Instant};

use crate::error::DevError;

/// A reading of the boot clock, microseconds since boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BootInstant(i64);

impl BootInstant {
    /// Read the boot clock. Fallible rather than zero on failure, so no
    /// caller mistakes a failed read for a plausible stamp.
    pub(crate) fn now() -> Result<Self, DevError> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec for the call's duration.
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut ts) } != 0 {
            return Err(DevError::Io(std::io::Error::last_os_error()));
        }
        Ok(Self(
            ts.tv_sec
                .saturating_mul(1_000_000)
                .saturating_add(ts.tv_nsec / 1_000),
        ))
    }

    /// A stored absolute boot stamp, for the store's and the host trace's
    /// codecs only: the writer established what the integer means.
    pub(crate) fn decoded(us: i64) -> Self {
        Self(us)
    }

    pub(crate) fn as_us(self) -> i64 {
        self.0
    }

    /// The span from `origin` to this reading; zero when this one is not
    /// later (two reads of one monotonic clock never run backwards).
    pub(crate) fn since(self, origin: Self) -> BootElapsed {
        BootElapsed(Duration::from_micros(
            u64::try_from(self.0.saturating_sub(origin.0)).unwrap_or(0),
        ))
    }

    /// This reading as a stamp on the clock whose zero is `origin`.
    pub(crate) fn on_child_clock(self, origin: Self) -> ChildUs {
        ChildUs(self.0.saturating_sub(origin.0))
    }

    /// The reading `at` on the child clock whose zero is this one: where a
    /// child-relative stamp lands on the host trace.
    pub(crate) fn offset_by(self, at: ChildUs) -> Self {
        Self(self.0.saturating_add(at.0))
    }
}

/// A span on the boot clock, suspend included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BootElapsed(Duration);

impl BootElapsed {
    /// A stored boot span, for the store's codec only.
    pub(crate) fn decoded_us(us: i64) -> Self {
        Self(Duration::from_micros(u64::try_from(us).unwrap_or(0)))
    }

    pub(crate) fn as_duration(self) -> Duration {
        self.0
    }

    pub(crate) fn as_us(self) -> i64 {
        i64::try_from(self.0.as_micros()).unwrap_or(i64::MAX)
    }
}

/// A stamp on a child's clock: microseconds from its boot origin. Signed,
/// since an off-contract FIFO stamp may be negative and is kept as it came.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChildUs(i64);

impl ChildUs {
    /// A stamp read off the FIFO or the store, for those codecs only.
    pub(crate) fn decoded(us: i64) -> Self {
        Self(us)
    }

    pub(crate) fn as_us(self) -> i64 {
        self.0
    }
}

/// A child's start, read on both clocks just before its spawn: `at` for its
/// wall and deadline, `boot` as its child clock's zero.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunStart {
    pub(crate) at: Instant,
    pub(crate) boot: BootInstant,
}

impl RunStart {
    /// Both readings, the monotonic first. Refused when the boot clock can't
    /// be read, before anything is spawned.
    pub(crate) fn now() -> Result<Self, DevError> {
        let at = Instant::now();
        let boot = BootInstant::now()?;
        Ok(Self { at, boot })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_boot_clock_reads_and_never_runs_backwards() {
        let first = BootInstant::now().unwrap();
        let second = BootInstant::now().unwrap();
        assert!(first.as_us() > 0);
        assert!(second >= first);
        assert_eq!(first.since(second), BootElapsed::decoded_us(0));
    }

    #[test]
    fn child_stamps_land_on_the_trace_at_their_origin_plus_offset() {
        let origin = BootInstant::decoded(5_000);
        let later = BootInstant::decoded(7_500);
        let stamp = later.on_child_clock(origin);
        assert_eq!(stamp.as_us(), 2_500);
        assert_eq!(origin.offset_by(stamp), later);
        assert_eq!(later.since(origin).as_us(), 2_500);
    }
}
