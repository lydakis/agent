//! A ```html fence drawn as the page it is: laid out by a browser on this
//! machine, headless, into a PNG shown like the other pictures. Chrome
//! (or Chromium, Edge, Brave) when there is one, as it runs the page's
//! scripts; else macOS's Quick Look, which is WebKit without them.
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// How tall a page is laid out, in CSS pixels, before its blank end is cut.
const WINDOW_HEIGHT: u32 = 1600;

/// The page at `path` as a PNG, `css_width` CSS pixels wide at twice
/// that in device pixels; or why not.
pub fn render(path: &Path, css_width: u32) -> Result<Vec<u8>, String> {
    let out = std::env::temp_dir().join(format!("sonny-shot-{}", std::process::id()));
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let shot = match chrome() {
        Some(chrome) => {
            let png = out.join("page.png");
            let _ = std::fs::remove_file(&png);
            let mut command = Command::new(chrome);
            command
                .arg("--headless=new")
                .arg("--disable-gpu")
                .arg("--hide-scrollbars")
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                // Headless, it keeps a fresh profile of its own, apart
                // from the person's open Chrome. On macOS that profile is
                // given its own folder: without one, Chrome hangs on exit
                // when HOME holds no profile yet (on Linux, the flag itself
                // hangs Chrome for Testing).
                .args(
                    cfg!(target_os = "macos")
                        .then(|| format!("--user-data-dir={}", out.join("profile").display())),
                )
                .arg("--force-device-scale-factor=2")
                .arg(format!("--window-size={css_width},{WINDOW_HEIGHT}"))
                .arg(format!("--screenshot={}", png.display()))
                // Chrome will not run as root sandboxed (a container).
                .args(root().then_some("--no-sandbox"))
                .arg(format!("file://{}", path.display()));
            finish(command, "Chrome", Duration::from_secs(15))?;
            png
        }
        None if cfg!(target_os = "macos") => {
            let mut command = Command::new("qlmanage");
            command
                .arg("-t")
                .arg("-s")
                .arg((css_width * 2).to_string())
                .arg("-o")
                .arg(&out)
                .arg(path);
            finish(command, "Quick Look", Duration::from_secs(10))?;
            let name = path.file_name().ok_or("no file name")?.to_string_lossy();
            out.join(format!("{name}.png"))
        }
        None => return Err("drawing HTML needs Chrome or Chromium".into()),
    };
    let png = std::fs::read(&shot).map_err(|e| format!("{}: {e}", shot.display()))?;
    let _ = std::fs::remove_file(&shot);
    Ok(png)
}

fn root() -> bool {
    // SAFETY: geteuid takes nothing and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// A Chromium-based browser: `SONNY_CHROME`, the usual apps, then PATH.
fn chrome() -> Option<PathBuf> {
    if let Some(bin) = std::env::var_os("SONNY_CHROME") {
        return Some(PathBuf::from(bin));
    }
    let apps = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ];
    let names = ["google-chrome", "chromium", "chromium-browser", "chrome"];
    apps.iter()
        .map(PathBuf::from)
        .find(|app| app.is_file())
        .or_else(|| {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
                .find(|bin| bin.is_file())
        })
}

/// Run to the end, quietly, killed past `limit`.
fn finish(mut command: Command, what: &str, limit: Duration) -> Result<(), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{what}: {e}"))?;
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return match status.success() {
                true => Ok(()),
                false => Err(format!("{what} could not draw the page")),
            };
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{what} took longer than {}s", limit.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_page_draws_and_its_blank_end_is_cut() {
        if super::chrome().is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("sonny-html-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("page.html");
        std::fs::write(
            &path,
            "<h1>Hello</h1><table border=1><tr><td>a</td><td>b</td></tr></table>",
        )
        .unwrap();
        let png = super::render(&path, 480).unwrap();
        let drawn = crate::image::page(&png, 80).unwrap();
        assert!(!drawn.cut);
        // 1600 CSS pixels laid out, a heading and a table kept.
        assert!((2..12).contains(&drawn.rows), "{}", drawn.rows);
    }
}
