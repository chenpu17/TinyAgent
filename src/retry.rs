//! Shared transient-retry backoff for batch (`--retries`) and single-run
//! headless mode. Both retry only transient failures (provider/network errors)
//! with exponential backoff; deterministic failures (bad_task, max_turns, …)
//! are never retried because they won't change on a re-run.

/// Exponential backoff (seconds) for retry `attempt` (0-indexed): 1, 2, 4, 8,
/// 16, 32 — the shift is clamped so a large `--retries` can't sleep for minutes
/// (or overflow).
pub(crate) fn backoff_secs(attempt: usize) -> u64 {
    1u64 << attempt.min(5)
}

#[cfg(test)]
mod tests {
    use super::backoff_secs;

    #[test]
    fn backoff_grows_then_caps() {
        // 1, 2, 4, 8, 16, 32 — doubling each step…
        assert_eq!(backoff_secs(0), 1);
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(4), 16);
        assert_eq!(backoff_secs(5), 32);
        // …then clamped at 32s so a large --retries never sleeps for minutes
        // (and so the shift can't overflow on absurd inputs).
        assert_eq!(backoff_secs(6), 32);
        assert_eq!(backoff_secs(100), 32);
    }
}
