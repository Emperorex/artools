//! The `-j/--jobs` worker count shared by all tools.

/// Upper bound for `-j/--jobs`. Far more threads than any directory walk
/// can use; it only stops a typo such as `-j 10000` from spawning thousands
/// of threads.
pub const MAX_JOBS: u16 = 128;

/// CPU-aware default worker count, used as the `-j/--jobs` default.
///
/// Half of `available_parallelism()`, clamped to `[1, 16]`: using every core
/// by default competes with the rest of the system (and with the other
/// tools in this repo if run concurrently), while an unclamped value could
/// default to an unreasonably high thread count on large build/CI machines.
/// `available_parallelism()` failing (sandboxed or restricted environments)
/// falls back to 1, which the clamp still turns into a valid default.
pub fn default_jobs() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (cpus / 2).clamp(1, 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_jobs_is_half_the_cpus_clamped() {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        assert_eq!(default_jobs(), (cpus / 2).clamp(1, 16));
        assert!((1..=usize::from(MAX_JOBS)).contains(&default_jobs()));
    }
}
