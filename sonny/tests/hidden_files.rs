//! A template an agent writes reads no file kept from agents: `sonny tool
//! live`, as the tool host runs it for one call, outside any sandbox.
use std::path::Path;

/// What `sonny tool live` answers for `markdown`, run in `folder` with `home`.
fn live(home: &Path, folder: &Path, markdown: &str) -> serde_json::Value {
    use std::io::Write;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sonny"))
        .args(["tool", "live"])
        .current_dir(folder)
        .env("HOME", home)
        .env("AGENT_TEST_HOME", home)
        .env("AGENT_BOT", "Reader")
        .env_remove("AGENT_STORE")
        .env_remove("AGENT_SOCKET")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let input = serde_json::json!({"markdown": markdown}).to_string();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let ran = child.wait_with_output().unwrap();
    assert!(ran.status.success(), "{ran:?}");
    serde_json::from_slice(&ran.stdout).unwrap()
}

#[test]
fn a_live_block_reads_no_file_in_a_folder_kept_from_agents() {
    let home = std::env::temp_dir().join(format!("sonny-hidden-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let work = home.join("work");
    let keys = home.join(".agent/keys");
    let cookies = home.join("cookies");
    for folder in [&work, &keys, &cookies] {
        std::fs::create_dir_all(folder).unwrap();
    }
    std::fs::write(keys.join("env"), "OPENAI_API_KEY=sk-kept\n").unwrap();
    std::fs::write(cookies.join("jar"), "SESSION=listed\n").unwrap();
    std::fs::write(work.join("notes.md"), "plain notes\n").unwrap();
    std::os::unix::fs::symlink(&keys, work.join("to-keys")).unwrap();
    let output = |markdown: &str| live(&home, &work, markdown)["output"].to_string();
    // A file of the agent's own fills, and so do the person's cookies,
    // which the config does not list.
    assert!(output("{{ file(\"notes.md\") }}").contains("plain notes"));
    assert!(output("{{ file(\"../cookies/jar\") }}").contains("SESSION=listed"));
    // The keys are refused, however they are named: by their own path,
    // up from the folder, or through a link in it.
    let absolute = keys.join("env").display().to_string();
    for path in [absolute.as_str(), "../.agent/keys/env", "to-keys/env"] {
        let said = output(&format!("{{{{ file(\"{path}\") }}}}"));
        assert!(said.contains("read_denied"), "{path}: {said}");
        assert!(!said.contains("sk-kept"), "{path}: {said}");
    }
    // A folder the config lists is refused the same way.
    std::fs::write(
        home.join(".agent/config.toml"),
        "[sandbox]\nunreadable = [\"~/cookies\"]\n",
    )
    .unwrap();
    let said = output("{{ file(\"../cookies/jar\") }}");
    assert!(
        said.contains("read_denied") && !said.contains("SESSION"),
        "{said}"
    );
    assert!(output("{{ file(\"notes.md\") }}").contains("plain notes"));
    std::fs::remove_dir_all(&home).unwrap();
}
