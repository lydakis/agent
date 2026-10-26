//! Errand jobs started from this project, for `/status`: `errand ps
//! --json` lists the active jobs on every runner, and those whose project
//! is this folder's are shown.
use crate::status::Row;
use serde_json::Value;
use std::time::Duration;

/// The active jobs of `project` (Errand names a job's project after its
/// workspace's folder), none when Errand is not installed or does not
/// answer in time.
pub async fn jobs(project: String) -> Vec<Row> {
    // A listing that runs over is killed with the future that waits on it.
    let listing = tokio::process::Command::new("errand")
        .args(["ps", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(3), listing).await {
        Ok(Ok(out)) => rows(&String::from_utf8_lossy(&out.stdout), &project, now()),
        _ => Vec::new(),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn rows(json: &str, project: &str, now: u64) -> Vec<Row> {
    let Ok(Value::Array(jobs)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    jobs.iter()
        .filter(|job| job["project"] == project)
        .map(|job| {
            let state = match job["state"].as_str().unwrap_or("") {
                "running" => "working",
                "staging" | "queued" => "queued",
                "exited" if job["exit_code"] == 0 => "done",
                "exited" | "killed" => "failed",
                other => other,
            };
            let started = job["started_at"].as_str().or(job["admitted_at"].as_str());
            let peer = job["peer"].as_str().unwrap_or("?");
            Row {
                name: job["command"].as_str().unwrap_or("?").to_owned(),
                state: state.to_owned(),
                seconds: started.and_then(seconds).map(|at| now.saturating_sub(at)),
                model: String::new(),
                latest: String::new(),
                place: format!("{peer}/{}", job["id"].as_str().unwrap_or("?")),
            }
        })
        .collect()
}

/// Seconds since the epoch of an RFC 3339 UTC time, as Go writes one:
/// `2026-10-26T00:21:56.123456Z`. Another offset reads as none.
fn seconds(at: &str) -> Option<u64> {
    let at = at.strip_suffix('Z')?;
    let (date, time) = at.split_once('T')?;
    let mut date = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, d) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut time = time.split(':');
    let (h, min) = (
        time.next()?.parse::<i64>().ok()?,
        time.next()?.parse::<i64>().ok()?,
    );
    let s = time.next()?.split('.').next()?.parse::<i64>().ok()?;
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + min * 60 + s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_projects_jobs_are_rows() {
        let json = r#"[
          {"peer":"mini","id":"01JB","state":"running","command":"cargo test",
           "admitted_at":"2026-10-26T00:00:00Z","started_at":"2026-10-26T00:00:30.5Z","project":"calc"},
          {"peer":"gpu","id":"01JC","state":"queued","command":"python train.py",
           "admitted_at":"2026-10-26T00:01:00Z","project":"other"}
        ]"#;
        let now = seconds("2026-10-26T00:02:00Z").unwrap();
        let calc = rows(json, "calc", now);
        assert_eq!(calc.len(), 1);
        assert_eq!(
            (
                calc[0].name.as_str(),
                calc[0].state.as_str(),
                calc[0].seconds
            ),
            ("cargo test", "working", Some(90))
        );
        assert_eq!(calc[0].place, "mini/01JB");
        assert_eq!(seconds("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(rows("not json", "calc", now).len(), 0);
    }
}
