//! A detached command's place under an init that does not reap orphans.
//! Its own binary: this process becomes the subreaper that never reaps, and
//! that would leave every other test's orphans as zombies too.
#![cfg(target_os = "linux")]
// The libc calls signal or inspect processes this test started.
#![allow(clippy::undocumented_unsafe_blocks)]
use agent_runtime::tools::Registry;
use serde_json::{Value, json};

fn state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

/// What a launcher leaves in its group and that has exited holds no place,
/// though nobody reaps it and it still answers a signal.
#[tokio::test]
async fn an_unreaped_service_frees_its_detached_place() {
    // Orphans now come to this process, which never reaps them, as an init
    // in a container may not.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let tools = Registry::new("shell").unwrap().with_lasting_budget(1);
    let dir = std::env::temp_dir().join(format!("agent-detach-reaping-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let detach = |command: &str| {
        tools
            .prepare(
                "shell",
                &json!({"command":command,"detach":true}).to_string(),
            )
            .unwrap()
    };
    let file = dir.join("service.pid");
    let output: Value = serde_json::from_str(
        &tools
            .execute(
                detach(&format!("sleep 30 & echo $! > {}", file.display())),
                &dir,
                &[],
                None,
            )
            .await
            .unwrap()
            .output,
    )
    .unwrap();
    let launcher = output["pid"].as_i64().unwrap() as i32;
    let mut service = None;
    for _ in 0..500 {
        service = (std::fs::read_to_string(&file).ok()).and_then(|pid| pid.trim().parse().ok());
        if service.is_some() && state(launcher).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let service: i32 = service.expect("the service started");
    let refused = async || {
        (tools.execute(detach("true"), &dir, &[], None).await)
            .err()
            .map(|error| error.code)
    };
    assert_eq!(refused().await.as_deref(), Some("lasting_limit"));
    unsafe { libc::kill(service, libc::SIGKILL) };
    for _ in 0..500 {
        if state(service) == Some('Z') {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // The service is a zombie: it still answers a signal, and holds no place.
    assert_eq!(state(service), Some('Z'));
    assert_eq!(unsafe { libc::kill(-launcher, 0) }, 0);
    let mut free = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if refused().await.is_none() {
            free = true;
            break;
        }
    }
    assert!(free, "an unreaped service kept the place");
    std::fs::remove_dir_all(dir).unwrap();
}
