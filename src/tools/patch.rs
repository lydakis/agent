//! `apply_patch`: the patch format OpenAI's models are trained to edit
//! with. A patch is parsed whole, then every change is computed against
//! the files as they are, and only then is anything written, so a patch
//! that does not apply changes nothing.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path
//! +line
//! *** Update File: path
//! *** Move to: new/path
//! @@ fn context
//!  kept
//! -removed
//! +added
//! *** End of File
//! *** Delete File: path
//! *** End Patch
//! ```
use super::{FILE_BYTES, files};
use crate::{Error, Result, sandbox::Policy};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The Lark grammar OpenAI models are given for this tool, as Codex gives
/// it. A model sends a patch that fits it as the call's raw input.
pub const GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

/// One file's change.
#[derive(Debug, PartialEq)]
pub enum Change {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

/// A run of lines to find and replace, after an optional `@@` line that
/// places it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chunk {
    context: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    end_of_file: bool,
}

/// A parsed patch: its changes, in order.
#[derive(Debug, PartialEq)]
pub struct Patch {
    pub changes: Vec<Change>,
}

fn invalid(line: usize, detail: impl std::fmt::Display) -> Error {
    Error::with("invalid_patch", format!("line {line}: {detail}"))
}

/// The most a patch's changed files may hold together while it is staged.
const STAGED_BYTES: usize = 16 * FILE_BYTES;

/// A path as the file it names: `./f` and `d/../f` are `f`.
fn file_key(path: &str) -> PathBuf {
    let mut key = PathBuf::new();
    for part in Path::new(path).components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if key.file_name().is_some() => {
                key.pop();
            }
            part => key.push(part),
        }
    }
    key
}

/// Note that the patch changes `path`. Each file is changed once: a
/// second change would be computed from the file as it was.
fn claim(seen: &mut HashSet<PathBuf>, path: &str, line: usize) -> Result<()> {
    let key = file_key(path);
    if !seen.insert(key) {
        return Err(invalid(
            line,
            format!("{path} is changed twice; a patch changes each file once"),
        ));
    }
    Ok(())
}

impl Patch {
    /// Parse a patch. A heredoc around it, as a model writes when it
    /// thinks it is running a command, is taken off first.
    pub fn parse(text: &str) -> Result<Patch> {
        let mut lines: Vec<&str> = text.trim().lines().collect();
        if lines.len() >= 2
            && lines[0].trim_start().starts_with("<<")
            && lines[lines.len() - 1].trim() == "EOF"
        {
            lines = lines[1..lines.len() - 1].to_vec();
        }
        let first = lines.first().map(|line| line.trim());
        if first != Some("*** Begin Patch") {
            return Err(invalid(1, "a patch starts with *** Begin Patch"));
        }
        if lines.len() < 2 || lines[lines.len() - 1].trim() != "*** End Patch" {
            return Err(invalid(lines.len(), "a patch ends with *** End Patch"));
        }
        let body = &lines[1..lines.len() - 1];
        let mut changes = Vec::new();
        let mut seen = HashSet::new();
        let mut at = 0;
        while at < body.len() {
            let number = at + 2;
            let line = body[at].trim_end();
            at += 1;
            if let Some(path) = line.strip_prefix("*** Add File: ") {
                let mut contents = String::new();
                while at < body.len() && !body[at].starts_with("*** ") {
                    let Some(text) = body[at].strip_prefix('+') else {
                        return Err(invalid(at + 2, "every line of an added file starts with +"));
                    };
                    contents.push_str(text);
                    contents.push('\n');
                    at += 1;
                }
                let path = path_of(path, number)?;
                claim(&mut seen, &path, number)?;
                changes.push(Change::Add { path, contents });
            } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
                let path = path_of(path, number)?;
                claim(&mut seen, &path, number)?;
                changes.push(Change::Delete { path });
            } else if let Some(path) = line.strip_prefix("*** Update File: ") {
                let path = path_of(path, number)?;
                claim(&mut seen, &path, number)?;
                let mut move_to = None;
                if let Some(to) = body.get(at).and_then(|l| l.strip_prefix("*** Move to: ")) {
                    let to = path_of(to, at + 2)?;
                    // A move to the file itself is no move.
                    if file_key(&to) != file_key(&path) {
                        claim(&mut seen, &to, at + 2)?;
                        move_to = Some(to);
                    }
                    at += 1;
                }
                let chunks = chunks(body, &mut at)?;
                if chunks.is_empty() && move_to.is_none() {
                    return Err(invalid(
                        number,
                        format!("{path}: an update changes nothing"),
                    ));
                }
                changes.push(Change::Update {
                    path,
                    move_to,
                    chunks,
                });
            } else if line.is_empty() {
                continue;
            } else {
                return Err(invalid(
                    number,
                    format!(
                        "expected *** Add File, *** Update File or *** Delete File, found {line:?}"
                    ),
                ));
            }
        }
        if changes.is_empty() {
            return Err(invalid(1, "a patch changes at least one file"));
        }
        // A file and a file below it can't both be written: one of them
        // would need a folder where the other puts a file.
        if let Some(below) = seen
            .iter()
            .find(|key| key.ancestors().skip(1).any(|above| seen.contains(above)))
        {
            return Err(Error::with(
                "invalid_patch",
                format!(
                    "{} is below another file this patch changes",
                    below.display()
                ),
            ));
        }
        Ok(Patch { changes })
    }

    /// Every path the patch writes or removes, as written in it: each
    /// added, updated and deleted file, and both ends of a move. Nothing
    /// is touched before a caller has seen them all.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.changes
            .iter()
            .flat_map(|change| match change {
                Change::Add { path, .. } | Change::Delete { path } => [Some(path.as_str()), None],
                Change::Update { path, move_to, .. } => [Some(path.as_str()), move_to.as_deref()],
            })
            .flatten()
    }

    /// Apply the patch under `workspace`: [`stage`](Self::stage), then
    /// [`commit`](Staged::commit).
    pub async fn apply(&self, workspace: &Path, sandbox: Option<Arc<Policy>>) -> Result<String> {
        self.stage(workspace).await?.commit(sandbox).await
    }

    /// Read and check every file the patch changes, and compute what each
    /// becomes, writing nothing.
    pub async fn stage(&self, workspace: &Path) -> Result<Staged> {
        let mut writes = Vec::new();
        let mut staged = 0;
        let mut summary = String::from("Success. Updated the following files:\n");
        for change in &self.changes {
            match change {
                Change::Add { path, contents } => {
                    writable(workspace, path).await?;
                    staged += contents.len();
                    writes.push(Write::Put(workspace.join(path), contents.clone(), None));
                    summary.push_str(&format!("A {path}\n"));
                }
                Change::Delete { path } => {
                    let target = workspace.join(path);
                    // Only a regular file is removed.
                    match tokio::fs::symlink_metadata(&target).await {
                        Ok(metadata) if metadata.is_file() || metadata.is_symlink() => {}
                        Ok(_) => return Err(Error::with("file_not_regular", path.clone())),
                        Err(_) => return Err(Error::with("file_unreadable", path.clone())),
                    }
                    writes.push(Write::Remove(target));
                    summary.push_str(&format!("D {path}\n"));
                }
                Change::Update {
                    path,
                    move_to,
                    chunks,
                } => {
                    let source = workspace.join(path);
                    let bytes = files::read_bounded(&source).await?;
                    let text = String::from_utf8(bytes)
                        .map_err(|_| Error::with("file_not_utf8", path.clone()))?;
                    // Matching can take a while on a long file: off the
                    // runtime's thread.
                    let (chunks, name) = (chunks.clone(), path.clone());
                    let updated =
                        tokio::task::spawn_blocking(move || update(&text, &chunks, &name))
                            .await
                            .map_err(|_| {
                                Error::with(
                                    "invalid_patch",
                                    format!("{path}: could not be matched"),
                                )
                            })??;
                    if updated.len() > FILE_BYTES {
                        return Err(Error::with("tool_output_limit", path.clone()));
                    }
                    staged += updated.len();
                    if staged > STAGED_BYTES {
                        return Err(Error::with(
                            "tool_output_limit",
                            "the files a patch changes hold at most 64 MiB together",
                        ));
                    }
                    match move_to {
                        Some(to) => {
                            writable(workspace, to).await?;
                            // A moved script stays executable.
                            let permissions = tokio::fs::metadata(&source).await?.permissions();
                            writes.push(Write::Put(workspace.join(to), updated, Some(permissions)));
                            writes.push(Write::Remove(source));
                            summary.push_str(&format!("M {to}\n"));
                        }
                        None => {
                            writes.push(Write::Put(source, updated, None));
                            summary.push_str(&format!("M {path}\n"));
                        }
                    }
                }
            }
        }
        Ok(Staged { writes, summary })
    }
}

/// A patch's changes, checked and computed, and its summary: each file as
/// added (A), modified (M) or deleted (D).
pub struct Staged {
    writes: Vec<Write>,
    summary: String,
}

enum Write {
    /// A file's new contents, and for a move the permissions of the file it
    /// came from.
    Put(PathBuf, String, Option<std::fs::Permissions>),
    Remove(PathBuf),
}

impl Staged {
    /// Whether committing writes any file, through the sandbox's helper
    /// when the bot has one; a patch that only deletes writes none.
    pub fn writes(&self) -> bool {
        self.writes
            .iter()
            .any(|write| matches!(write, Write::Put(..)))
    }

    /// Write the changes, in `sandbox` when the bot has one.
    pub async fn commit(self, sandbox: Option<Arc<Policy>>) -> Result<String> {
        for write in self.writes {
            match write {
                Write::Put(target, contents, permissions) => {
                    files::write(&target, contents.as_bytes(), sandbox.clone()).await?;
                    if let Some(permissions) = permissions {
                        tokio::fs::set_permissions(&target, permissions).await?;
                    }
                }
                // The sandbox's helper only writes. A removal was checked
                // against the bot's roots before anything ran.
                Write::Remove(target) => tokio::fs::remove_file(&target).await?,
            }
        }
        Ok(self.summary)
    }
}

/// Fail unless `path` can be written as a regular file: what is there, if
/// anything, is one, and every folder above it is a folder or missing.
/// Checked before anything is written, so a patch that cannot apply
/// changes nothing.
async fn writable(workspace: &Path, path: &str) -> Result<()> {
    let target = workspace.join(path);
    if let Ok(metadata) = tokio::fs::metadata(&target).await
        && !metadata.is_file()
    {
        return Err(Error::with("file_not_regular", path.to_owned()));
    }
    for above in target.ancestors().skip(1) {
        if above == workspace {
            break;
        }
        if let Ok(metadata) = tokio::fs::metadata(above).await
            && !metadata.is_dir()
        {
            return Err(Error::with("file_not_regular", path.to_owned()));
        }
    }
    Ok(())
}

fn path_of(path: &str, line: usize) -> Result<String> {
    let path = path.trim();
    if path.is_empty() || path.len() > 4096 || path.contains('\0') {
        return Err(invalid(line, "a file path is 1 to 4096 bytes"));
    }
    Ok(path.to_owned())
}

/// An update's chunks, up to the next file header.
fn chunks(body: &[&str], at: &mut usize) -> Result<Vec<Chunk>> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current: Option<Chunk> = None;
    while *at < body.len() && !is_header(body[*at]) {
        let line = body[*at];
        let number = *at + 2;
        *at += 1;
        if line.trim_end() == "*** End of File" {
            let Some(chunk) = current.as_mut() else {
                return Err(invalid(number, "*** End of File ends a chunk"));
            };
            chunk.end_of_file = true;
            chunks.extend(current.take());
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@") {
            chunks.extend(current.take());
            let context = rest.trim();
            current = Some(Chunk {
                context: (!context.is_empty())
                    .then(|| rest.strip_prefix(' ').unwrap_or(rest).to_owned()),
                ..Chunk::default()
            });
            continue;
        }
        let chunk = current.get_or_insert_with(Chunk::default);
        match line.chars().next() {
            Some('+') => chunk.new.push(line[1..].to_owned()),
            Some('-') => chunk.old.push(line[1..].to_owned()),
            Some(' ') => {
                chunk.old.push(line[1..].to_owned());
                chunk.new.push(line[1..].to_owned());
            }
            // An empty line is an empty context line whose space was lost.
            None => {
                chunk.old.push(String::new());
                chunk.new.push(String::new());
            }
            Some(_) => {
                return Err(invalid(
                    number,
                    format!("a change line starts with +, - or a space, found {line:?}"),
                ));
            }
        }
    }
    chunks.extend(current);
    Ok(chunks)
}

fn is_header(line: &str) -> bool {
    ["*** Add File: ", "*** Delete File: ", "*** Update File: "]
        .iter()
        .any(|header| line.starts_with(header))
}

/// `text` with every chunk applied in order, each found at or after the
/// end of the one before it.
fn update(text: &str, chunks: &[Chunk], path: &str) -> Result<String> {
    let crlf = text.contains("\r\n");
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    // The empty piece after a final newline is not a line.
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut hay = Haystack {
        lines: &lines,
        plain: None,
        left: COMPARED,
    };
    let mut replacements: Vec<(usize, usize, &[String])> = Vec::new();
    let mut cursor = 0;
    for chunk in chunks {
        if let Some(context) = &chunk.context {
            let found =
                seek(&mut hay, std::slice::from_ref(context), cursor, false)?.ok_or_else(|| {
                    Error::with(
                        "patch_context_not_found",
                        format!("{path}: no line matches @@ {context}"),
                    )
                })?;
            cursor = found + 1;
        }
        if chunk.old.is_empty() {
            let at = if chunk.context.is_some() {
                cursor
            } else {
                lines.len()
            };
            replacements.push((at, 0, &chunk.new));
            continue;
        }
        let (mut old, mut new) = (&chunk.old[..], &chunk.new[..]);
        let mut found = seek(&mut hay, old, cursor, chunk.end_of_file)?;
        // A trailing empty line can stand for the file's final newline.
        if found.is_none() && old.last().is_some_and(String::is_empty) {
            old = &old[..old.len() - 1];
            if new.last().is_some_and(String::is_empty) {
                new = &new[..new.len() - 1];
            }
            found = seek(&mut hay, old, cursor, chunk.end_of_file)?;
        }
        let at = found.ok_or_else(|| {
            Error::with(
                "patch_context_not_found",
                format!(
                    "{path}: these lines were not found{}:\n{}",
                    if cursor > 0 {
                        " after the previous chunk"
                    } else {
                        ""
                    },
                    chunk.old.join("\n")
                ),
            )
        })?;
        replacements.push((at, old.len(), new));
        cursor = at + old.len();
    }
    // An insertion with no context goes at the end of the file, so it can
    // come before chunks that are found earlier in it.
    replacements.sort_by_key(|&(at, _, _)| at);
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut next = 0;
    for (at, removed, added) in replacements {
        if at < next {
            return Err(Error::with(
                "patch_context_not_found",
                format!("{path}: two chunks change the same lines"),
            ));
        }
        out.extend(&lines[next..at]);
        out.extend(added.iter().map(String::as_str));
        next = at + removed;
    }
    out.extend(&lines[next..]);
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut updated = out.join(eol);
    // Every line ends: a file of one empty line is a newline.
    if !out.is_empty() {
        updated.push_str(eol);
    }
    Ok(updated)
}

/// Line comparisons one update may make before it gives up: well past
/// what a real patch needs, short of what stalls the daemon's blocking pool.
const COMPARED: usize = 50_000_000;

/// What `seek` reads a file through: its lines, the same lines with
/// typographic punctuation made plain once for every chunk, and the line
/// comparisons left.
struct Haystack<'a> {
    lines: &'a [&'a str],
    plain: Option<Vec<String>>,
    left: usize,
}

impl Haystack<'_> {
    /// Line `at` as pass `pass` reads it.
    fn line(&self, pass: usize, at: usize) -> &str {
        let line = self.lines[at];
        match pass {
            0 => line,
            1 => line.trim_end(),
            2 => line.trim(),
            _ => &self.plain.as_ref().unwrap()[at],
        }
    }
}

/// `line` as pass `pass` reads it: exactly, then without trailing
/// whitespace, then trimmed, then trimmed with punctuation made plain.
fn normal(pass: usize, line: &str) -> std::borrow::Cow<'_, str> {
    match pass {
        0 => line.into(),
        1 => line.trim_end().into(),
        2 => line.trim().into(),
        _ => line.trim().chars().map(plain).collect::<String>().into(),
    }
}

/// Where `pattern` first occurs in the file at or after `start`, matched
/// exactly, then ignoring trailing whitespace, then surrounding
/// whitespace, then common typographic punctuation. With `end_of_file`
/// a match at the very end is preferred. Fails with `invalid_patch` once
/// the update has compared too many lines.
fn seek(
    hay: &mut Haystack,
    pattern: &[String],
    start: usize,
    end_of_file: bool,
) -> Result<Option<usize>> {
    let lines = hay.lines.len();
    if pattern.len() > lines {
        return Ok(None);
    }
    let last = lines - pattern.len();
    let from = if end_of_file { last.max(start) } else { start };
    for pass in 0..4 {
        if pass == 3 && hay.plain.is_none() {
            hay.plain = Some(
                hay.lines
                    .iter()
                    .map(|l| normal(3, l).into_owned())
                    .collect(),
            );
        }
        let wanted: Vec<_> = pattern.iter().map(|line| normal(pass, line)).collect();
        for at in (from..=last).chain(start..from.min(last + 1)) {
            // The ends first: most places differ there, and a long run of
            // like lines is then not compared whole at every place.
            hay.left = hay.left.checked_sub(1).ok_or_else(too_slow)?;
            if hay.line(pass, at + pattern.len() - 1) != wanted[pattern.len() - 1] {
                continue;
            }
            hay.left = hay.left.checked_sub(pattern.len()).ok_or_else(too_slow)?;
            if (0..pattern.len()).all(|i| hay.line(pass, at + i) == wanted[i]) {
                return Ok(Some(at));
            }
        }
    }
    Ok(None)
}

fn too_slow() -> Error {
    Error::with(
        "invalid_patch",
        "matching this patch's chunks compares too many lines; split it into smaller patches",
    )
}

/// Typographic dashes, quotes and spaces as their ASCII forms.
fn plain(c: char) -> char {
    match c {
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{2018}'..='\u{201B}' => '\'',
        '\u{201C}'..='\u{201F}' => '"',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(body: &str) -> Patch {
        Patch::parse(&format!("*** Begin Patch\n{body}*** End Patch\n")).unwrap()
    }

    #[test]
    fn parses_every_kind_of_change_and_lists_its_paths() {
        let parsed = patch(
            "*** Add File: a.txt\n+one\n+two\n*** Delete File: b.txt\n\
             *** Update File: c.txt\n*** Move to: d/c.txt\n@@ fn main\n keep\n-old\n+new\n",
        );
        assert_eq!(
            parsed.paths().collect::<Vec<_>>(),
            ["a.txt", "b.txt", "c.txt", "d/c.txt"]
        );
        assert_eq!(
            parsed.changes[0],
            Change::Add {
                path: "a.txt".into(),
                contents: "one\ntwo\n".into()
            }
        );
        let Change::Update { chunks, .. } = &parsed.changes[2] else {
            panic!("an update")
        };
        assert_eq!(chunks[0].context.as_deref(), Some("fn main"));
        assert_eq!((chunks[0].old.len(), chunks[0].new.len()), (2, 2));
        // A heredoc around the patch is taken off.
        let wrapped = "<<'EOF'\n*** Begin Patch\n*** Delete File: x\n*** End Patch\nEOF\n";
        assert_eq!(
            Patch::parse(wrapped).unwrap().paths().collect::<Vec<_>>(),
            ["x"]
        );
    }

    #[test]
    fn malformed_patches_say_where() {
        for (text, wanted) in [
            ("*** Delete File: x\n", "line 1"),
            ("*** Begin Patch\n*** Delete File: x\n", "End Patch"),
            (
                "*** Begin Patch\n*** Add File: x\nnot plus\n*** End Patch",
                "line 3",
            ),
            ("*** Begin Patch\nrandom\n*** End Patch", "line 2"),
            (
                "*** Begin Patch\n*** Update File: x\n*** End Patch",
                "changes nothing",
            ),
            ("*** Begin Patch\n*** End Patch", "at least one file"),
            (
                "*** Begin Patch\n*** Update File: f\n-a\n+b\n*** Update File: ./f\n-c\n+d\n*** End Patch",
                "line 5: ./f is changed twice",
            ),
            (
                "*** Begin Patch\n*** Update File: f\n*** Move to: g\n-a\n+b\n*** Delete File: d/../g\n*** End Patch",
                "changed twice",
            ),
            (
                "*** Begin Patch\n*** Add File: new\n+a\n*** Add File: new/child\n+b\n*** End Patch",
                "new/child is below another file",
            ),
        ] {
            let error = Patch::parse(text).unwrap_err();
            assert_eq!(error.code, "invalid_patch", "{text}");
            assert!(
                error.detail.as_deref().unwrap().contains(wanted),
                "{error:?}"
            );
        }
    }

    #[test]
    fn many_files_parse_quickly() {
        let mut body = String::new();
        for n in 0..40_000 {
            body.push_str(&format!("*** Delete File: dir/file{n}.txt\n"));
        }
        let started = std::time::Instant::now();
        assert_eq!(patch(&body).paths().count(), 40_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn a_move_to_the_file_itself_is_an_update() {
        let parsed = patch("*** Update File: f\n*** Move to: ./f\n-a\n+b\n");
        let Change::Update { move_to, .. } = &parsed.changes[0] else {
            panic!()
        };
        assert_eq!(move_to, &None);
    }

    #[test]
    fn chunks_apply_in_order_with_lenient_matching() {
        let text = "fn a() {\n    x();\n}\nfn b() {\n    x();\n}\n";
        let parsed = patch("*** Update File: f\n@@ fn b() {\n-    x();\n+    y();\n");
        let Change::Update { chunks, .. } = &parsed.changes[0] else {
            panic!()
        };
        // The @@ line places the chunk: the second x() changes.
        assert_eq!(
            update(text, chunks, "f").unwrap(),
            "fn a() {\n    x();\n}\nfn b() {\n    y();\n}\n"
        );
        // Trailing spaces and typographic quotes still match.
        let parsed = patch("*** Update File: f\n-say(\"hi\")\n+say(\"bye\")\n");
        let Change::Update { chunks, .. } = &parsed.changes[0] else {
            panic!()
        };
        assert_eq!(
            update("say(\u{201C}hi\u{201D})  \r\nend\r\n", chunks, "f").unwrap(),
            "say(\"bye\")\r\nend\r\n"
        );
        // Lines that are not there fail with the lines it looked for.
        let error = update("other\n", chunks, "f").unwrap_err();
        assert_eq!(error.code, "patch_context_not_found");
        assert!(error.detail.unwrap().contains("say(\"hi\")"));
        // One empty line is still a line.
        let parsed = patch("*** Update File: f\n-a\n+\n");
        let Change::Update { chunks, .. } = &parsed.changes[0] else {
            panic!()
        };
        assert_eq!(update("a\n", chunks, "f").unwrap(), "\n");
        // A long run of like lines whose last line is not there fails fast.
        let text = "x\n".repeat(20_000);
        let mut long = "*** Update File: f\n".to_owned();
        long.push_str(&"-x\n".repeat(9_999));
        long.push_str("-y\n+z\n");
        let parsed = patch(&long);
        let Change::Update { chunks, .. } = &parsed.changes[0] else {
            panic!()
        };
        let started = std::time::Instant::now();
        assert!(update(&text, chunks, "f").is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        // Many chunks that each scan the rest of a long file stop at a
        // bound on the lines compared, rather than stall the daemon.
        let mut text = "\u{201C}q\u{201D}\n".repeat(300);
        text.push_str(&"x\n".repeat(100_000));
        let mut many = "*** Update File: f\n".to_owned();
        many.push_str(&"@@\n-\"q\"\n+r\n".repeat(300));
        let parsed = patch(&many);
        let Change::Update { chunks, .. } = &parsed.changes[0] else {
            panic!()
        };
        let error = update(&text, chunks, "f").unwrap_err();
        assert_eq!(error.code, "invalid_patch");
        assert!(error.detail.unwrap().contains("too many lines"));
        // An insertion with no context lands at the end even when a later
        // chunk changes an earlier line.
        let parsed = patch("*** Update File: f\n@@\n+tail\n@@\n-first\n+changed\n");
        let Change::Update { chunks, .. } = &parsed.changes[0] else {
            panic!()
        };
        assert_eq!(
            update("first\nsecond\n", chunks, "f").unwrap(),
            "changed\nsecond\ntail\n"
        );
    }

    #[tokio::test]
    async fn a_patch_that_fails_anywhere_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("patch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "a\nb\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "x\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("keep.txt"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let failing =
            patch("*** Add File: new.txt\n+n\n*** Update File: keep.txt\n-missing\n+b2\n");
        assert_eq!(
            failing.apply(&dir, None).await.unwrap_err().code,
            "patch_context_not_found"
        );
        assert!(!dir.join("new.txt").exists());
        // So does one that adds a file where a folder is, or below a file.
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        for target in ["folder", "gone.txt/inner"] {
            let failing = patch(&format!(
                "*** Update File: keep.txt\n-b\n+b2\n*** Add File: {target}\n+n\n"
            ));
            assert_eq!(
                failing.apply(&dir, None).await.unwrap_err().code,
                "file_not_regular"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("keep.txt")).unwrap(),
                "a\nb\n"
            );
        }
        std::fs::remove_dir(dir.join("folder")).unwrap();
        let good = patch(
            "*** Add File: sub/new.txt\n+n\n*** Update File: keep.txt\n*** Move to: moved.txt\n a\n-b\n+b2\n\
             *** Delete File: gone.txt\n",
        );
        assert_eq!(
            good.apply(&dir, None).await.unwrap(),
            "Success. Updated the following files:\nA sub/new.txt\nM moved.txt\nD gone.txt\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/new.txt")).unwrap(),
            "n\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("moved.txt")).unwrap(),
            "a\nb2\n"
        );
        assert!(!dir.join("keep.txt").exists() && !dir.join("gone.txt").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("moved.txt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "a move keeps the file's mode");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
