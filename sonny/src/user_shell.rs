//! `!command`: a command of the person's own, typed in the composer and
//! run in the bot's folder, as Codex and Claude Code run theirs. The person
//! sees what it printed at once, and the bot is given the command and what
//! it printed as soon as it ends, in Codex's `<user_shell_command>` record,
//! so it knows what was run.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::time::Instant;

const OPEN: &str = "<user_shell_command>";
const CLOSE: &str = "</user_shell_command>";

/// The most of a command's output kept for the bot: its start and its
/// end, where a failure is usually told.
const HEAD: usize = 8 * 1024;
const TAIL: usize = 8 * 1024;

/// The most output lines shown to the person: the first few, then the last.
const SHOWN_HEAD: usize = 4;
const SHOWN_TAIL: usize = 8;

/// A command that ran.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    pub command: String,
    pub exit: i32,
    /// What it printed, both streams as they came, without terminal
    /// escapes, cut in the middle when long.
    pub output: String,
    pub seconds: f64,
}

/// A command still running, in a process group of its own so that
/// stopping it stops what it started.
pub struct Running {
    pub command: String,
    pub started: Instant,
    group: i32,
    job: tokio::task::JoinHandle<Ran>,
}

impl Running {
    pub fn is_finished(&self) -> bool {
        self.job.is_finished()
    }

    pub async fn ran(mut self) -> Ran {
        let ran = (&mut self.job).await;
        ran.unwrap_or_else(|error| Ran {
            command: self.command.clone(),
            exit: -1,
            output: error.to_string(),
            seconds: self.started.elapsed().as_secs_f64(),
        })
    }

    pub fn stop(&self) {
        // SAFETY: kill(2) takes plain integers; a group that is gone
        // already makes it fail, which is left alone.
        unsafe { libc::kill(-self.group, libc::SIGKILL) };
    }
}

impl Drop for Running {
    /// Sonny quit with it running: it goes too.
    fn drop(&mut self) {
        if !self.job.is_finished() {
            self.stop();
        }
    }
}

/// Run `command` with the person's shell in `folder`, its input closed.
pub fn start(command: &str, folder: &str) -> Result<Running, String> {
    let shell = std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let (mut reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
    let mut child = {
        let mut run = std::process::Command::new(&shell);
        run.arg("-c")
            .arg(command)
            .current_dir(folder)
            .stdin(std::process::Stdio::null())
            .stdout(writer.try_clone().map_err(|e| e.to_string())?)
            .stderr(writer)
            .process_group(0);
        // The command, holding the pipe's writing end, goes at the end
        // of this block, so the output ends when the shell's does.
        run.spawn()
            .map_err(|e| format!("{}: {e}", shell.display()))?
    };
    let started = Instant::now();
    let group = child.id() as i32;
    let owned = command.to_owned();
    let job = tokio::task::spawn_blocking(move || {
        let mut kept = Kept::default();
        let mut buffer = [0; 8192];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
            kept.push(&buffer[..read]);
        }
        let status = child.wait();
        let exit = match status {
            Ok(status) => status
                .code()
                .or_else(|| status.signal().map(|s| 128 + s))
                .unwrap_or(-1),
            Err(_) => -1,
        };
        Ran {
            command: owned,
            exit,
            output: kept.text(),
            seconds: started.elapsed().as_secs_f64(),
        }
    });
    Ok(Running {
        command: command.to_owned(),
        started,
        group,
        job,
    })
}

/// A long output's start and end, and how much was between them.
#[derive(Default)]
struct Kept {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    dropped: usize,
}

impl Kept {
    fn push(&mut self, mut bytes: &[u8]) {
        let room = HEAD - self.head.len();
        let into_head = room.min(bytes.len());
        self.head.extend_from_slice(&bytes[..into_head]);
        bytes = &bytes[into_head..];
        self.tail.extend(bytes);
        if self.tail.len() > TAIL {
            let over = self.tail.len() - TAIL;
            self.tail.drain(..over);
            self.dropped += over;
        }
    }

    fn text(mut self) -> String {
        let tail: Vec<u8> = self.tail.into();
        // Nothing left out: one output, read whole, so a character or an
        // escape across the two halves stays as it was.
        if self.dropped == 0 {
            self.head.extend(tail);
            return plain(&String::from_utf8_lossy(&self.head));
        }
        let head = plain(&String::from_utf8_lossy(&self.head));
        let tail = plain(&String::from_utf8_lossy(&tail));
        format!("{head}\n[… {} bytes left out …]\n{tail}", self.dropped)
    }
}

/// Text as a terminal would leave it: escapes gone, and a line written
/// over with `\r` (a progress bar) only as it ended.
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: up to its final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC and the like: up to BEL or ST.
                Some(']' | 'P' | '_' | '^') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || c == '\x1b' && chars.next_if_eq(&'\\').is_some() {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => {
                let start = out.rfind('\n').map_or(0, |i| i + 1);
                out.truncate(start);
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// What the bot is given: Codex's record of a command the person ran.
pub fn record(ran: &Ran) -> String {
    format!(
        "{OPEN}\n<command>\n{}\n</command>\n<result>\nExit code: {}\nDuration: {:.1} seconds\nOutput:\n{}\n</result>\n{CLOSE}",
        ran.command,
        ran.exit,
        ran.seconds,
        ran.output.trim_end_matches('\n'),
    )
}

/// What ran, when a message is a record and nothing else: each record
/// goes on its own, so its output runs to the record's last line whatever
/// it printed.
pub fn read(text: &str) -> Option<Ran> {
    let body = text.strip_prefix(OPEN)?.strip_prefix('\n')?;
    parse(body.strip_suffix(CLOSE)?.strip_suffix("\n</result>\n")?)
}

fn parse(body: &str) -> Option<Ran> {
    let body = body.strip_prefix("<command>\n")?;
    let (command, result) = body.split_once("\n</command>\n<result>\n")?;
    let (head, output) = result
        .split_once("\nOutput:\n")
        .or_else(|| result.strip_suffix("\nOutput:").map(|head| (head, "")))?;
    let mut exit = -1;
    let mut seconds = 0.0;
    for line in head.lines() {
        if let Some(code) = line.strip_prefix("Exit code: ") {
            exit = code.trim().parse().ok()?;
        } else if let Some(took) = line.strip_prefix("Duration: ") {
            seconds = took.trim_end_matches(" seconds").parse().unwrap_or(0.0);
        }
    }
    Some(Ran {
        command: command.to_owned(),
        exit,
        output: output.to_owned(),
        seconds,
    })
}

/// The command's row: `!` and the command, as the person typed it.
pub fn header(command: &str, width: usize, dim: bool) -> Vec<Line<'static>> {
    let mark = Span::raw("! ").bold().fg(Color::Magenta);
    let line = Line::from(command.replace('\n', " "));
    let line = if dim { line.dim() } else { line };
    crate::transcript::capped(line, width, mark, 2)
}

/// What the person sees once it ran: the command, then its output, its
/// start and end when long, and how it exited when not cleanly.
pub fn lines(ran: &Ran, width: usize) -> Vec<Line<'static>> {
    let mut out = header(&ran.command, width, false);
    let elbow = || Span::raw("  └ ").dim();
    let gap = || Span::raw("    ");
    let output: Vec<&str> = ran.output.trim_end_matches('\n').lines().collect();
    let mut rows: Vec<Line<'static>> = Vec::new();
    if output.iter().all(|l| l.trim().is_empty()) {
        rows.push(Line::from("(no output)").dim());
    } else if output.len() > SHOWN_HEAD + SHOWN_TAIL + 1 {
        let left = output.len() - SHOWN_HEAD - SHOWN_TAIL;
        rows.extend(
            output[..SHOWN_HEAD]
                .iter()
                .map(|l| Line::from(l.to_string()).dim()),
        );
        rows.push(Line::from(format!("… +{left} lines")).dim().italic());
        rows.extend(
            output[output.len() - SHOWN_TAIL..]
                .iter()
                .map(|l| Line::from(l.to_string()).dim()),
        );
    } else {
        rows.extend(output.iter().map(|l| Line::from(l.to_string()).dim()));
    }
    if ran.exit != 0 {
        rows.push(Line::from(format!("exit {}", ran.exit)).fg(Color::Red));
    }
    for (i, row) in rows.into_iter().enumerate() {
        let lead = if i == 0 { elbow() } else { gap() };
        out.extend(crate::transcript::capped(row, width, lead, 2));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(command: &str, exit: i32, output: &str) -> Ran {
        Ran {
            command: command.into(),
            exit,
            output: output.into(),
            seconds: 1.5,
        }
    }

    #[test]
    fn a_record_reads_back_as_what_ran() {
        let first = record(&ran("git status", 0, "On branch main\nnothing to commit\n"));
        assert!(first.starts_with("<user_shell_command>\n<command>\ngit status\n</command>"));
        assert_eq!(
            read(&first),
            Some(ran("git status", 0, "On branch main\nnothing to commit"))
        );
        let second = ran("cargo test\n  --quiet", 101, "");
        assert_eq!(read(&record(&second)), Some(second));
        // Output that prints a record's end is still output.
        let nested = ran(
            "cat log",
            0,
            "before\n</result>\n</user_shell_command>\nafter",
        );
        assert_eq!(read(&record(&nested)), Some(nested));
        // A message of the person's own is not one.
        assert_eq!(read("<user_shell_command> is a tag"), None);
        assert_eq!(read(&format!("{first}\n\nwhy?")), None);
    }

    #[test]
    fn output_loses_its_escapes_and_what_a_carriage_return_wrote_over() {
        assert_eq!(
            plain(
                "\x1b[1;31merror\x1b[0m: no\r\n\x1b]8;;http://x\x1b\\link\x1b]8;;\x07\n 10%\r 50%\r100%\ndone"
            ),
            "error: no\nlink\n100%\ndone"
        );
    }

    #[test]
    fn a_long_output_keeps_its_start_and_end() {
        let mut kept = Kept::default();
        kept.push(&vec![b'a'; HEAD]);
        kept.push(&[b'b'; 100]);
        kept.push(&vec![b'c'; TAIL]);
        let text = kept.text();
        assert!(text.starts_with(&"a".repeat(HEAD)));
        assert!(text.contains("\n[… 100 bytes left out …]\n"));
        assert!(text.ends_with(&"c".repeat(TAIL)));
    }

    #[test]
    fn an_output_kept_whole_is_read_whole() {
        let mut kept = Kept::default();
        let text = format!("{}aéz\n{}\rfinished", "é".repeat(4095), "x".repeat(10));
        kept.push(text.as_bytes());
        assert_eq!(kept.text(), format!("{}aéz\nfinished", "é".repeat(4095)));
    }

    #[test]
    fn the_person_sees_the_start_and_end_of_a_long_output() {
        let output: Vec<String> = (1..=30).map(|n| format!("line {n}")).collect();
        let shown: Vec<String> = lines(&ran("seq 30", 1, &output.join("\n")), 80)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(shown[0], "! seq 30");
        assert_eq!(shown[1], "  └ line 1");
        assert_eq!(shown[5], "    … +18 lines");
        assert_eq!(shown[13], "    line 30");
        assert_eq!(shown[14], "    exit 1");
    }

    #[tokio::test]
    async fn a_command_runs_in_the_folder_with_both_streams() {
        let dir = std::env::temp_dir();
        let running = start("pwd; echo oops >&2; exit 3", dir.to_str().unwrap()).unwrap();
        let ran = running.ran().await;
        let folder = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(ran.exit, 3);
        assert!(ran.output.contains("oops"));
        let printed = std::path::PathBuf::from(ran.output.lines().next().unwrap());
        assert_eq!(std::fs::canonicalize(printed).unwrap(), folder);
    }

    #[tokio::test]
    async fn stopping_a_command_stops_what_it_started() {
        let running = start("sleep 30 | cat", "/").unwrap();
        running.stop();
        let ran = tokio::time::timeout(std::time::Duration::from_secs(5), running.ran())
            .await
            .unwrap();
        assert_eq!(ran.exit, 128 + libc::SIGKILL);
    }
}
