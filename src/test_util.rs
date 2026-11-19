//! Helpers shared by unit tests and benches.

/// Process CPU time (unix: `CLOCK_PROCESS_CPUTIME_ID`).
/// Non-unix fallback: wall time since first call, as there is no libc.
pub(crate) fn cpu_now() -> std::time::Duration {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime only writes the timespec we hand it.
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
        std::time::Duration::new(
            u64::try_from(ts.tv_sec).unwrap_or(0),
            u32::try_from(ts.tv_nsec).unwrap_or(0),
        )
    }
    #[cfg(not(unix))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed()
    }
}

/// `cpu_now` in microseconds, for the f64 benches.
pub(crate) fn cpu_now_us() -> f64 {
    cpu_now().as_secs_f64() * 1e6
}
