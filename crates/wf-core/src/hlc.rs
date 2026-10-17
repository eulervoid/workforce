//! Hybrid logical clock. Orders concurrent writes; never grants authority.

use serde::{Deserialize, Serialize};

/// How far ahead of a device's own clock a received event's `wall_ms` may be.
pub const HLC_TOLERANCE_MS: u64 = 60 * 60 * 1000;

/// Wall clock milliseconds plus a counter. Derived `Ord` compares wall time first.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Hlc {
    wall_ms: u64,
    counter: u32,
}

impl Hlc {
    pub const ZERO: Hlc = Hlc {
        wall_ms: 0,
        counter: 0,
    };

    pub fn wall_ms(&self) -> u64 {
        self.wall_ms
    }

    /// Whether admission accepts this clock: neither part is at its maximum, so a successor
    /// always exists.
    pub fn is_admissible(&self) -> bool {
        self.wall_ms < u64::MAX && self.counter < u32::MAX
    }

    /// The smallest admissible clock strictly greater than `floor`, and not behind `now_ms`.
    /// `None` when the clock is used up: `floor` is so far ahead that its successor would
    /// overflow.
    ///
    /// `floor` is the greatest clock among the author's previous event and all `deps`, so
    /// every event is ordered after everything it causally follows. A full counter carries
    /// into `wall_ms`.
    pub fn next_after(floor: Hlc, now_ms: u64) -> Option<Hlc> {
        let next = if now_ms > floor.wall_ms {
            Hlc {
                wall_ms: now_ms,
                counter: 0,
            }
        } else {
            match floor
                .counter
                .checked_add(1)
                .filter(|counter| *counter < u32::MAX)
            {
                Some(counter) => Hlc {
                    wall_ms: floor.wall_ms,
                    counter,
                },
                None => Hlc {
                    wall_ms: floor.wall_ms.checked_add(1)?,
                    counter: 0,
                },
            }
        };
        next.is_admissible().then_some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_with_wall_clock() {
        let first = Hlc::next_after(Hlc::ZERO, 100).unwrap();
        let second = Hlc::next_after(first, 200).unwrap();
        assert!(second > first);
        assert_eq!(second.wall_ms(), 200);
    }

    #[test]
    fn counter_breaks_ties_when_the_clock_stalls_or_runs_backwards() {
        let first = Hlc::next_after(Hlc::ZERO, 100).unwrap();
        let same_time = Hlc::next_after(first, 100).unwrap();
        let earlier = Hlc::next_after(same_time, 50).unwrap();
        assert!(first < same_time && same_time < earlier);
        assert_eq!(earlier.wall_ms(), 100);
    }

    fn clock(wall_ms: u64, counter: u32) -> Hlc {
        Hlc { wall_ms, counter }
    }

    #[test]
    fn a_full_counter_carries_into_the_wall_time() {
        let next = Hlc::next_after(clock(100, u32::MAX - 1), 50).unwrap();
        assert_eq!(next, clock(101, 0));
    }

    #[test]
    fn the_clock_is_used_up_instead_of_overflowing() {
        assert_eq!(Hlc::next_after(clock(u64::MAX - 1, u32::MAX - 1), 0), None);
        assert_eq!(Hlc::next_after(clock(u64::MAX, 0), 0), None);
        assert_eq!(Hlc::next_after(Hlc::ZERO, u64::MAX), None);
    }

    #[test]
    fn the_maxima_are_not_admissible() {
        assert!(!clock(u64::MAX, 0).is_admissible());
        assert!(!clock(0, u32::MAX).is_admissible());
        assert!(clock(u64::MAX - 1, u32::MAX - 1).is_admissible());
    }
}
