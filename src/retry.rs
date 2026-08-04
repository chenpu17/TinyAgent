//! Shared transient-retry backoff for batch (`--retries`) and single-run
//! headless mode. Both retry only transient failures (provider/network errors)
//! with exponential backoff; deterministic failures (bad_task, max_turns, …)
//! are never retried because they won't change on a re-run.

use std::time::{Duration, Instant};

/// Exponential backoff for retry `attempt` (0-indexed) with ±25% jitter: base
/// 1, 2, 4, 8, 16, 32s — the shift is clamped so a large `--retries` can't
/// sleep for minutes (or overflow). Jitter decorrelates concurrent workers:
/// without it, a fleet of batch tasks that all failed on the same overloaded
/// provider would retry in lockstep and stampede it again at each step.
pub(crate) fn backoff(attempt: usize) -> Duration {
    backoff_with(attempt, jitter_seed())
}

/// Sleep for `dur`, but return early (returning `true`) as soon as `cancel` is
/// set — checking roughly every 100ms. Used between retry attempts so a Ctrl-C
/// during the (up to ~40s with jitter) backoff is honored within ~100ms instead
/// of after the whole sleep, which would let the next attempt run a full task
/// the user just asked to abort. The final sub-100ms tail is slept whole after
/// one last cancel check, and `cancel` is re-checked on return so a signal
/// landing in that tail is still caught before the caller proceeds.
pub(crate) fn sleep_cancelable(cancel: &std::sync::atomic::AtomicBool, dur: Duration) -> bool {
    use std::sync::atomic::Ordering;
    const SLICE: Duration = Duration::from_millis(100);
    let mut remaining = dur;
    while remaining > SLICE {
        if cancel.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(SLICE);
        remaining = remaining.saturating_sub(SLICE);
    }
    if cancel.load(Ordering::Relaxed) {
        return true;
    }
    if remaining > Duration::ZERO {
        std::thread::sleep(remaining);
    }
    cancel.load(Ordering::Relaxed)
}

/// Base backoff in milliseconds (before jitter).
fn base_millis(attempt: usize) -> u64 {
    1000u64 << attempt.min(5)
}

/// Pure core, testable: `jitter` in 0.0..=1.0 scales the base by 0.75..=1.25.
fn backoff_with(attempt: usize, jitter: f64) -> Duration {
    let mult = 0.75 + 0.5 * jitter.clamp(0.0, 1.0);
    Duration::from_millis((base_millis(attempt) as f64 * mult) as u64)
}

/// Dependency-free jitter source in 0.0..1.0. The low nanos of the time since
/// the first backoff call are unpredictable enough to decorrelate retries —
/// this is not a security context, just anti-lockstep spread.
fn jitter_seed() -> f64 {
    use std::sync::OnceLock;
    static T0: OnceLock<Instant> = OnceLock::new();
    let t0 = *T0.get_or_init(Instant::now);
    let nanos = t0.elapsed().subsec_nanos();
    (nanos % 1_000_000) as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        // Base 1, 2, 4, 8, 16, 32s — doubling each step…
        assert_eq!(base_millis(0), 1_000);
        assert_eq!(base_millis(1), 2_000);
        assert_eq!(base_millis(2), 4_000);
        assert_eq!(base_millis(3), 8_000);
        assert_eq!(base_millis(4), 16_000);
        assert_eq!(base_millis(5), 32_000);
        // …then clamped at 32s so a large --retries never sleeps for minutes
        // (and so the shift can't overflow on absurd inputs).
        assert_eq!(base_millis(6), 32_000);
        assert_eq!(base_millis(100), 32_000);
    }

    #[test]
    fn jitter_stays_within_plus_minus_25_percent() {
        for attempt in [0, 3, 5, 100] {
            let base = base_millis(attempt);
            assert_eq!(backoff_with(attempt, 0.0).as_millis() as u64, base * 3 / 4);
            assert_eq!(backoff_with(attempt, 1.0).as_millis() as u64, base * 5 / 4);
            // The seeded entry point must land inside the same envelope.
            let d = backoff(attempt).as_millis() as u64;
            assert!(d >= base * 3 / 4 && d <= base * 5 / 4, "attempt {attempt}: {d}ms");
        }
    }

    #[test]
    fn sleep_cancelable_returns_promptly_when_already_cancelled() {
        use std::sync::atomic::AtomicBool;
        let cancel = AtomicBool::new(true);
        let start = Instant::now();
        let cancelled = sleep_cancelable(&cancel, Duration::from_secs(10));
        assert!(cancelled);
        // Already cancelled: the very first check returns without sleeping 10s.
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn sleep_cancelable_sleeps_the_whole_duration_when_not_cancelled() {
        use std::sync::atomic::AtomicBool;
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        let cancelled = sleep_cancelable(&cancel, Duration::from_millis(150));
        assert!(!cancelled);
        // 150ms = one 100ms slice + a 50ms tail; at least the slice slept.
        assert!(start.elapsed() >= Duration::from_millis(100));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
