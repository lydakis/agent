//! What the daemon's own process uses, for `stats`: its memory now and at
//! its peak, its threads, and the processor time it has spent since it
//! started, so a leak or a busy loop shows without another tool. Read on
//! demand, off the run loop: one `getrusage` and one small read of the
//! process's own record; nothing is sampled in the background.

/// When the daemon started serving, set as it starts.
static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Mark the daemon's start, once.
pub fn start() {
    STARTED.get_or_init(std::time::Instant::now);
}

/// The process's footprint now. Fields it cannot read are null.
pub fn read() -> serde_json::Value {
    // SAFETY: a zeroed rusage is valid; getrusage fills it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: a valid pointer to an rusage this function owns.
    let used = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0;
    let ms = |time: libc::timeval| time.tv_sec as u64 * 1_000 + time.tv_usec as u64 / 1_000;
    // Linux counts the peak in KiB, macOS in bytes.
    #[cfg(target_os = "macos")]
    let peak = usage.ru_maxrss as u64;
    #[cfg(not(target_os = "macos"))]
    let peak = usage.ru_maxrss as u64 * 1024;
    let (resident, threads) = now();
    serde_json::json!({
        "uptime_ms": STARTED.get().map(|at| at.elapsed().as_millis() as u64),
        "cpu_user_ms": used.then(|| ms(usage.ru_utime)),
        "cpu_system_ms": used.then(|| ms(usage.ru_stime)),
        "rss_bytes": resident,
        "peak_rss_bytes": used.then_some(peak),
        "threads": threads,
    })
}

/// Resident bytes and threads now, from the kernel's record of this task.
#[cfg(target_os = "macos")]
fn now() -> (Option<u64>, Option<u64>) {
    // SAFETY: a zeroed proc_taskinfo is valid; proc_pidinfo fills it.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: the buffer is a proc_taskinfo of `size` bytes this function owns.
    let read = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    match read == size {
        true => (
            Some(info.pti_resident_size),
            Some(info.pti_threadnum.max(0) as u64),
        ),
        false => (None, None),
    }
}

/// Resident bytes and threads now, from `/proc/self/status`.
#[cfg(not(target_os = "macos"))]
fn now() -> (Option<u64>, Option<u64>) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (None, None);
    };
    let field = |name: &str| {
        (status.lines())
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|number| number.parse::<u64>().ok())
    };
    (field("VmRSS:").map(|kib| kib * 1024), field("Threads:"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_footprint_reads_this_process() {
        super::start();
        let footprint = super::read();
        assert!(footprint["uptime_ms"].is_u64(), "{footprint}");
        assert!(footprint["cpu_user_ms"].is_u64(), "{footprint}");
        assert!(
            footprint["peak_rss_bytes"].as_u64() > Some(0),
            "{footprint}"
        );
        let resident = footprint["rss_bytes"].as_u64().unwrap();
        assert!(resident > 0 && resident <= footprint["peak_rss_bytes"].as_u64().unwrap() * 2);
        assert!(footprint["threads"].as_u64() >= Some(1), "{footprint}");
    }
}
