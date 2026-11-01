//! `sonny scheduler` started for a HOME already gone.

#[test]
fn a_scheduler_started_for_a_home_already_gone_makes_it_no_more() {
    let home = std::env::temp_dir().join(format!("sonny-gone-home-{}", std::process::id()));
    assert!(!home.exists());
    let ran = std::process::Command::new(env!("CARGO_BIN_EXE_sonny"))
        .arg("scheduler")
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap();
    assert!(ran.status.success(), "{ran:?}");
    assert!(
        String::from_utf8_lossy(&ran.stderr).contains("home_gone"),
        "{ran:?}"
    );
    assert!(!home.exists(), "its HOME is not made again");
}
