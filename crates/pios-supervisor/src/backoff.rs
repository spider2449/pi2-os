//! Exponential restart backoff that resets after a stable run.

use std::time::Duration;

pub const BASE: Duration = Duration::from_millis(500);
pub const MAX: Duration = Duration::from_secs(30);
/// A run at least this long is considered healthy and resets the backoff.
pub const STABLE: Duration = Duration::from_secs(60);

/// Delay before the next restart, given consecutive quick failures so far
/// (0 for the first restart) and how long the last run lasted.
pub fn next_delay(consecutive_failures: u32, last_run: Duration) -> (Duration, u32) {
    if last_run >= STABLE {
        return (BASE, 1);
    }
    let shift = consecutive_failures.min(16);
    let delay = BASE.saturating_mul(1u32 << shift).min(MAX);
    (delay, consecutive_failures.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_then_caps() {
        let quick = Duration::from_secs(1);
        let mut n = 0;
        let mut delays = Vec::new();
        for _ in 0..10 {
            let (d, next) = next_delay(n, quick);
            delays.push(d);
            n = next;
        }
        assert_eq!(delays[0], Duration::from_millis(500));
        assert_eq!(delays[1], Duration::from_secs(1));
        assert_eq!(delays[2], Duration::from_secs(2));
        assert_eq!(*delays.last().unwrap(), MAX);
    }

    #[test]
    fn resets_after_stable_run() {
        assert_eq!(next_delay(8, Duration::from_secs(120)), (BASE, 1));
    }
}
