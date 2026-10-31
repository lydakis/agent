//! Scripts run by the machine's `node` or `bun`: the vendored renderers
//! (Pele for Mermaid, MathJax for LaTeX) are JavaScript.
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
}

#[cfg(test)]
pub fn available() -> bool {
    ["node", "bun"].into_iter().any(on_path)
}

/// How long a script may take: five seconds, so a picture that will not
/// come never holds a reply up. Tests share busy runners, where node's
/// cold start alone can take that (#418), and check what is drawn, not
/// how fast.
const LIMIT: Duration = Duration::from_secs(if cfg!(test) { 60 } else { 5 });

/// What `script` writes to stdout, or the first line it wrote to stderr.
/// At most [`LIMIT`].
pub fn run(script: String, what: &str) -> Result<String, String> {
    let runtime = ["node", "bun"]
        .into_iter()
        .find(|bin| on_path(bin))
        .ok_or_else(|| format!("drawing {what} needs node or bun on PATH"))?;
    let mut child = Command::new(runtime)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{runtime}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    // Each pipe on its own thread, so a full one cannot stall the child or
    // Sonny; stdin closes when its thread ends.
    std::thread::spawn(move || stdin.write_all(script.as_bytes()));
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{what} took longer than {}s", LIMIT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    if !status.success() {
        let error = String::from_utf8_lossy(&err);
        return Err(error
            .lines()
            .next()
            .unwrap_or(&format!("{what} failed"))
            .to_owned());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}
