//! Hybrid logical clock. Orders concurrent writes; never grants authority.

use serde::{Deserialize, Serialize};

/// Wall clock milliseconds plus a counter. Derived `Ord` compares wall time first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

    /// The smallest clock strictly greater than `floor`, and not behind `now_ms`.
    ///
    /// `floor` is the greatest clock among the author's previous event and all `deps`, so
    /// every event is ordered after everything it causally follows.
    pub fn next_after(floor: Hlc, now_ms: u64) -> Hlc {
        if now_ms > floor.wall_ms {
            Hlc {
                wall_ms: now_ms,
                counter: 0,
            }
        } else {
            Hlc {
                wall_ms: floor.wall_ms,
                counter: floor.counter + 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_with_wall_clock() {
        let first = Hlc::next_after(Hlc::ZERO, 100);
        let second = Hlc::next_after(first, 200);
        assert!(second > first);
        assert_eq!(second.wall_ms(), 200);
    }

    #[test]
    fn counter_breaks_ties_when_the_clock_stalls_or_runs_backwards() {
        let first = Hlc::next_after(Hlc::ZERO, 100);
        let same_time = Hlc::next_after(first, 100);
        let earlier = Hlc::next_after(same_time, 50);
        assert!(first < same_time && same_time < earlier);
        assert_eq!(earlier.wall_ms(), 100);
    }
}
