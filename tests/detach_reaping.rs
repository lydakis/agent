//! A detached command's place under an init that does not reap orphans.
//! Its own binary: this process becomes the subreaper that never reaps, and
//! that would leave every other test's orphans as zombies too.
#![cfg(target_os = "linux")]
// The libc calls signal or inspect processes these tests started.
#![allow(clippy::undocumented_unsafe_blocks)]
use agent_runtime::tools::{Prepared, Registry};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn state(pid: i32) -> Option<char> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.iter().rposition(|&b| b == b')')? + 1..];
    after
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .map(|&b| b as char)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what}");
}

/// A lasting budget of one, in a directory of the test's own, with orphans
/// coming to this process, which never reaps them, as an init in a container
/// may not.
fn setup(name: &str) -> (Registry, PathBuf) {
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let dir = std::env::temp_dir().join(format!("agent-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (Registry::new("shell").unwrap().with_lasting_budget(1), dir)
}

fn detach(tools: &Registry, command: &str) -> Prepared {
    (tools.prepare(
        "shell",
        &json!({"command":command,"detach":true}).to_string(),
    ))
    .unwrap()
}

/// Detaches a launcher that starts `service` in the background and exits,
/// and returns the service's pid once the launcher is gone.
async fn launch(tools: &Registry, dir: &Path, service: &str) -> i32 {
    let file = dir.join("service.pid");
    let command = format!("{service} & echo $! > {}", file.display());
    let output: Value = serde_json::from_str(
        &(tools.execute(detach(tools, &command), dir, &[], None).await)
            .unwrap()
            .output,
    )
    .unwrap();
    let launcher = output["pid"].as_i64().unwrap() as i32;
    let mut pid = None;
    until("the service started and its launcher exited", || {
        pid = (std::fs::read_to_string(&file).ok()).and_then(|pid| pid.trim().parse().ok());
        pid.is_some() && state(launcher).is_none()
    })
    .await;
    pid.unwrap()
}

async fn refused(tools: &Registry, dir: &Path) -> Option<String> {
    (tools.execute(detach(tools, "true"), dir, &[], None).await)
        .err()
        .map(|error| error.code)
}

/// Within the three seconds of agreeing walks an exited-only group takes.
async fn frees(tools: &Registry, dir: &Path) -> bool {
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if refused(tools, dir).await.is_none() {
            return true;
        }
    }
    false
}

/// What a launcher leaves in its group and that has exited holds no place,
/// though nobody reaps it and it still answers a signal.
#[tokio::test]
async fn an_unreaped_service_frees_its_detached_place() {
    let (tools, dir) = setup("detach-reaping");
    let service = launch(&tools, &dir, "sleep 30").await;
    assert_eq!(
        refused(&tools, &dir).await.as_deref(),
        Some("lasting_limit")
    );
    unsafe { libc::kill(service, libc::SIGKILL) };
    until("the service became a zombie", || {
        state(service) == Some('Z')
    })
    .await;
    // It still answers a signal, and holds no place.
    assert_eq!(unsafe { libc::kill(service, 0) }, 0);
    assert!(
        frees(&tools, &dir).await,
        "an unreaped service kept the place"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A service whose main thread has exited, so `/proc` shows it a zombie,
/// still runs while another of its threads does, and keeps its place.
#[tokio::test]
async fn a_service_whose_main_thread_exited_keeps_its_place() {
    let (tools, dir) = setup("detach-threads");
    let source = dir.join("service.c");
    std::fs::write(
        &source,
        "#include <pthread.h>\n#include <unistd.h>\n\
         static void *work(void *x) { sleep(30); return x; }\n\
         int main(void) { pthread_t t; pthread_create(&t, 0, work, 0); pthread_exit(0); }\n",
    )
    .unwrap();
    let binary = dir.join("service");
    let built = std::process::Command::new("cc")
        .args(["-pthread", "-o"])
        .arg(&binary)
        .arg(&source)
        .status();
    if !built.is_ok_and(|status| status.success()) {
        assert!(std::env::var_os("CI").is_none(), "no C compiler");
        eprintln!("skipped: no C compiler");
        return;
    }
    let service = launch(&tools, &dir, &binary.display().to_string()).await;
    until("the service's main thread exited", || {
        state(service) == Some('Z')
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        refused(&tools, &dir).await.as_deref(),
        Some("lasting_limit")
    );
    unsafe { libc::kill(service, libc::SIGKILL) };
    assert!(
        frees(&tools, &dir).await,
        "the place was not freed once it ended"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A service whose name the kernel cuts mid-character, so `/proc` shows it
/// in bytes that are not UTF-8, still runs and keeps its place.
#[tokio::test]
async fn a_service_whose_name_is_not_utf8_keeps_its_place() {
    let (tools, dir) = setup("detach-name");
    // Fourteen bytes and a two-byte é: the kernel keeps 15 of the 16.
    let name = dir.join("abcdefghijklmn\u{e9}");
    let sleep = ["/bin/sleep", "/usr/bin/sleep"]
        .into_iter()
        .find(|path| Path::new(path).exists())
        .unwrap();
    std::os::unix::fs::symlink(sleep, &name).unwrap();
    let service = launch(&tools, &dir, &format!("'{}' 30", name.display())).await;
    let comm = std::fs::read(format!("/proc/{service}/comm")).unwrap();
    assert!(std::str::from_utf8(&comm).is_err(), "{comm:?}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        refused(&tools, &dir).await.as_deref(),
        Some("lasting_limit")
    );
    unsafe { libc::kill(service, libc::SIGKILL) };
    assert!(
        frees(&tools, &dir).await,
        "the place was not freed once it ended"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
