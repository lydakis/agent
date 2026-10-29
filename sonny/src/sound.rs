//! A sound, and for what needs the person a notification, when work ends
//! or stops on them. One Sonny rings for a project: the lead's, which sees
//! every agent in it; the windows it opens for tasks stay quiet.
//!
//! On a Mac the sound is one of the system's (`SONNY_SOUND_DONE=Pop`, a
//! file's path, or `off` changes one); elsewhere it is the terminal bell.
//! The notification is OSC 777, which Rex shows as a desktop notification.
use std::{
    io::Write,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Alert {
    /// A task finished.
    TaskDone,
    /// The lead finished: the answer to what the person asked.
    LeadDone,
    Failed,
    /// An agent waits on the person's answer.
    NeedsYou,
}

impl Alert {
    fn sound(self) -> (&'static str, &'static str) {
        match self {
            Alert::TaskDone => ("SONNY_SOUND_TASK", "Pop"),
            Alert::LeadDone => ("SONNY_SOUND_DONE", "Glass"),
            Alert::Failed => ("SONNY_SOUND_FAILED", "Basso"),
            Alert::NeedsYou => ("SONNY_SOUND_NEEDS_YOU", "Hero"),
        }
    }
}

pub struct Sounds {
    on: bool,
    /// `/sound`: off keeps the notifications and drops the sound.
    pub chime: bool,
    last: Option<(Instant, Alert)>,
}

/// Sounds that land this close together ring once, as the most urgent.
const TOGETHER: Duration = Duration::from_millis(1200);

impl Sounds {
    pub fn new(on: bool) -> Self {
        Self {
            on,
            chime: true,
            last: None,
        }
    }

    /// Ring for the alert, and notify when it needs the person or answers
    /// them; `what` says which agent and what about.
    pub fn alert(&mut self, alert: Alert, what: &str) {
        if !self.on {
            return;
        }
        if matches!(alert, Alert::NeedsYou | Alert::LeadDone | Alert::Failed) {
            let text: String = what
                .chars()
                .filter(|c| !c.is_control())
                .map(|c| if c == ';' { ',' } else { c })
                .take(200)
                .collect();
            let mut stdout = std::io::stdout();
            let _ = write!(stdout, "\x1b]777;notify;Sonny;{text}\x07");
            let _ = stdout.flush();
        }
        if !self.chime {
            return;
        }
        if let Some((at, last)) = self.last
            && at.elapsed() < TOGETHER
            && last >= alert
        {
            return;
        }
        self.last = Some((Instant::now(), alert));
        play(alert);
    }
}

fn play(alert: Alert) {
    let (key, default) = alert.sound();
    let chosen = std::env::var(key).unwrap_or_else(|_| default.to_owned());
    if chosen == "off" {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let path = if chosen.contains('/') {
            std::path::PathBuf::from(&chosen)
        } else {
            std::path::PathBuf::from(format!("/System/Library/Sounds/{chosen}.aiff"))
        };
        if path.is_file() && crate::mac::play(&path) {
            return;
        }
    }
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x07");
    let _ = stdout.flush();
}
