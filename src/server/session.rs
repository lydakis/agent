//! Client sessions: one stdio owner, or any number of Unix-socket connections,
//! each feeding bounded JSONL requests into the service loop.
use super::Request;
use agent_runtime::{Error, Result, fail, output::Output};
use std::io::{BufRead, Read};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::mpsc,
};

// A request is handed over once, by value, through a channel; the parsed
// command's size is not worth an allocation per message.
#[allow(clippy::large_enum_variant)]
pub enum Inbound {
    Open(u64, Output),
    Request(u64, Result<Request>),
    Closed(u64),
}

pub const LINE_LIMIT: usize = 1024 * 1024;

pub fn stdio_reader(sender: mpsc::Sender<Inbound>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = Vec::new();
            let result = (&mut reader)
                .take(LINE_LIMIT as u64 + 1)
                .read_until(b'\n', &mut line);
            match result {
                Ok(0) => break,
                Ok(_) if line.len() <= LINE_LIMIT && line.ends_with(b"\n") => {
                    let request = serde_json::from_slice(&line).map_err(Error::from);
                    if sender.blocking_send(Inbound::Request(0, request)).is_err() {
                        break;
                    }
                }
                _ => {
                    let _ = sender
                        .blocking_send(Inbound::Request(0, fail("input_line_limit_or_truncation")));
                    break;
                }
            }
        }
        let _ = sender.blocking_send(Inbound::Closed(0));
    });
}

pub async fn socket_reader(
    id: u64,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    sender: mpsc::Sender<Inbound>,
) {
    loop {
        let mut line = Vec::new();
        let result = (&mut reader)
            .take(LINE_LIMIT as u64 + 1)
            .read_until(b'\n', &mut line)
            .await;
        match result {
            Ok(0) => break,
            Ok(_) if line.len() <= LINE_LIMIT && line.ends_with(b"\n") => {
                let request = serde_json::from_slice(&line).map_err(Error::from);
                if sender.send(Inbound::Request(id, request)).await.is_err() {
                    break;
                }
            }
            _ => {
                let _ = sender
                    .send(Inbound::Request(id, fail("input_line_limit_or_truncation")))
                    .await;
                break;
            }
        }
    }
    let _ = sender.send(Inbound::Closed(id)).await;
}
