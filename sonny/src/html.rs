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
    render_with(chrome(), path, css_width)
}

/// [`render`] with this browser, or Quick Look without one.
fn render_with(chrome: Option<PathBuf>, path: &Path, css_width: u32) -> Result<Vec<u8>, String> {
    let out = std::env::temp_dir().join(format!("sonny-shot-{}", std::process::id()));
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let shot = match chrome {
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
    use resvg::tiny_skia;

    /// A page drawn by a stand-in browser: it is asked for a screenshot of
    /// the page at the width wanted, and its blank end is cut. A real
    /// browser's start-up time is not this code's to test.
    #[test]
    fn a_page_draws_and_its_blank_end_is_cut() {
        let dir = std::env::temp_dir().join(format!("sonny-html-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("page.html");
        std::fs::write(&path, "<h1>Hello</h1>").unwrap();
        // What the browser would draw: 1600 CSS pixels at twice that, a
        // heading's worth of ink at the top and blank below.
        let mut page = tiny_skia::Pixmap::new(960, 3200).unwrap();
        page.fill(tiny_skia::Color::WHITE);
        let ink = tiny_skia::Rect::from_xywh(32.0, 32.0, 400.0, 64.0).unwrap();
        let mut paint = tiny_skia::Paint::default();
        paint.set_color(tiny_skia::Color::BLACK);
        page.fill_rect(ink, &paint, tiny_skia::Transform::identity(), None);
        page.save_png(dir.join("drawn.png")).unwrap();
        let browser = dir.join("chrome");
        crate::testing::script(
            &browser,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{args}'\n\
                 for a; do case $a in --screenshot=*) cp '{drawn}' \"${{a#--screenshot=}}\";; esac; done\n",
                args = dir.join("args").display(),
                drawn = dir.join("drawn.png").display(),
            ),
        );
        let png = super::render_with(Some(browser), &path, 480).unwrap();
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        for wanted in [
            "--headless=new",
            "--window-size=480,1600",
            "--force-device-scale-factor=2",
        ] {
            assert!(args.lines().any(|a| a == wanted), "{args}");
        }
        assert_eq!(
            args.lines().last(),
            Some(format!("file://{}", path.display()).as_str())
        );
        let drawn = crate::image::page(&png, 80).unwrap();
        assert!(!drawn.cut);
        // The heading kept, the blank 3000 pixels below it dropped.
        assert!((1..6).contains(&drawn.rows), "{}", drawn.rows);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
