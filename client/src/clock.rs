//! Times as the person's clock reads them, one way for the daemon and its
//! clients: the daemon's trailer and Sonny's lines name a moment alike.

pub const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `epoch` (seconds) in local time.
pub fn local(epoch: i64) -> libc::tm {
    // SAFETY: localtime_r fills the zeroed struct it is given and nothing else.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&(epoch as libc::time_t), &mut tm);
        tm
    }
}

/// `epoch` (seconds) in local time, its weekday and its offset from UTC:
/// `Sat 31 Oct 2026 16:12, UTC-04:00`.
pub fn when(epoch: i64) -> String {
    let tm = local(epoch);
    let offset = tm.tm_gmtoff / 60;
    format!(
        "{} {} {} {} {:02}:{:02}, UTC{}{:02}:{:02}",
        DAYS[tm.tm_wday.rem_euclid(7) as usize],
        tm.tm_mday,
        MONTHS[tm.tm_mon.rem_euclid(12) as usize],
        tm.tm_year + 1900,
        tm.tm_hour,
        tm.tm_min,
        if offset < 0 { '-' } else { '+' },
        offset.abs() / 60,
        offset.abs() % 60,
    )
}
