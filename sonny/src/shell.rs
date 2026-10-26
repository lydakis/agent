//! Enough of the shell's grammar to read what a model's command line does,
//! without running or expanding any of it: the commands a line holds, and
//! each command's words, its here-document and what it is piped into. A
//! command substitution stays as its text, except `$(cat <<EOF … EOF)`,
//! which is the text it would print.

/// A command line's commands, split where `;`, `&&`, `||` or a newline end
/// one outside quotes, substitutions and here-documents. Pipes stay inside
/// a command.
pub fn parts(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                let end = quoted(&chars, i);
                part.extend(&chars[i..end]);
                i = end;
                continue;
            }
            '\\' => {
                part.extend(&chars[i..(i + 2).min(chars.len())]);
                i += 2;
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                let end = substitution(&chars, i + 1);
                part.extend(&chars[i..end]);
                i = end;
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'{') => {
                let end = braces(&chars, i + 1);
                part.extend(&chars[i..end]);
                i = end;
                continue;
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                let (delimiter, strip, end) = delimiter(&chars, i + 2);
                part.extend(&chars[i..end]);
                heredocs.push((delimiter, strip));
                i = end;
                continue;
            }
            '\n' if !heredocs.is_empty() => {
                // The documents start on the next line and are part of
                // the command that opened them.
                let mut at = i + 1;
                for (delimiter, strip) in heredocs.drain(..) {
                    at = body(&chars, at, &delimiter, strip).1;
                }
                part.extend(&chars[i..at.min(chars.len())]);
                i = at;
                if !part.trim().is_empty() {
                    parts.push(std::mem::take(&mut part));
                }
                continue;
            }
            ';' | '\n' => {
                parts.push(std::mem::take(&mut part));
                i += 1;
                continue;
            }
            '&' | '|' if chars.get(i + 1) == Some(&c) => {
                parts.push(std::mem::take(&mut part));
                i += 2;
                continue;
            }
            _ => {}
        }
        part.push(c);
        i += 1;
    }
    parts.push(part);
    parts
        .into_iter()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

/// One command: its words, the here-document it reads, and the rest of
/// its pipeline.
#[derive(Debug, PartialEq)]
pub struct Simple {
    pub words: Vec<String>,
    pub stdin: Option<String>,
    pub piped: Option<String>,
}

/// A command's words as the shell would split them, quotes removed and
/// redirections dropped; none for a subshell or a group.
pub fn simple(command: &str) -> Option<Simple> {
    let chars: Vec<char> = command.trim().chars().collect();
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut stdin = None;
    let mut heredoc: Option<(String, bool)> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                let end = quoted(&chars, i);
                let inner = &chars[i + 1..end.saturating_sub(1).max(i + 1)];
                word.get_or_insert_with(String::new).extend(inner);
                i = end;
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    match chars[i] {
                        '\\' if i + 1 < chars.len() => {
                            match chars[i + 1] {
                                c @ ('"' | '\\' | '$' | '`') => w.push(c),
                                '\n' => {}
                                c => {
                                    w.push('\\');
                                    w.push(c);
                                }
                            }
                            i += 2;
                        }
                        '$' if chars.get(i + 1) == Some(&'(') => {
                            let end = substitution(&chars, i + 1);
                            w.push_str(&substituted(&chars[i..end]));
                            i = end;
                        }
                        c => {
                            w.push(c);
                            i += 1;
                        }
                    }
                }
                i += 1;
            }
            '`' => {
                let end = quoted(&chars, i);
                word.get_or_insert_with(String::new).extend(&chars[i..end]);
                i = end;
            }
            '\\' => {
                if let Some(&next) = chars.get(i + 1)
                    && next != '\n'
                {
                    word.get_or_insert_with(String::new).push(next);
                }
                i += 2;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                let end = substitution(&chars, i + 1);
                word.get_or_insert_with(String::new)
                    .push_str(&substituted(&chars[i..end]));
                i = end;
            }
            // `${…}` is one word whatever it holds, as in
            // `${AGENT_EFFORT:+--effort "$AGENT_EFFORT"}`.
            '$' if chars.get(i + 1) == Some(&'{') => {
                let end = braces(&chars, i + 1);
                word.get_or_insert_with(String::new).extend(&chars[i..end]);
                i = end;
            }
            ' ' | '\t' => {
                words.extend(word.take());
                i += 1;
            }
            '\n' => {
                words.extend(word.take());
                let (delimiter, strip) = heredoc.take()?;
                let (text, end) = body(&chars, i + 1, &delimiter, strip);
                stdin = Some(text);
                i = end;
                // Anything after the document is another command.
                if chars[i.min(chars.len())..]
                    .iter()
                    .any(|c| !c.is_whitespace())
                {
                    return None;
                }
            }
            // More than one command.
            ';' => return None,
            '&' | '|' if chars.get(i + 1) == Some(&c) => return None,
            '|' => {
                words.extend(word.take());
                let rest: String = chars[i + 1..].iter().collect();
                return Some(Simple {
                    words,
                    stdin,
                    piped: Some(rest.trim().to_owned()),
                })
                .filter(|s| !s.words.is_empty());
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                words.extend(word.take());
                let (delimiter, strip, end) = delimiter(&chars, i + 2);
                heredoc = Some((delimiter, strip));
                i = end;
            }
            '<' | '>' => {
                // A redirection and its target are not words: `2>&1`,
                // `>/dev/null`, `< file`.
                if word
                    .as_deref()
                    .is_some_and(|w| w.bytes().all(|b| b.is_ascii_digit()))
                    || word.as_deref() == Some("&")
                {
                    word = None;
                }
                words.extend(word.take());
                i += 1;
                while matches!(chars.get(i), Some('>' | '<' | '&' | '|')) {
                    i += 1;
                }
                while matches!(chars.get(i), Some(' ' | '\t')) {
                    i += 1;
                }
                let target = simple_word_end(&chars, i);
                i = target;
            }
            '(' | ')' | '{' | '}' if word.is_none() => return None,
            '&' if word.is_none() && chars.get(i + 1) == Some(&'>') => {
                word = Some("&".into());
                i += 1;
            }
            c => {
                word.get_or_insert_with(String::new).push(c);
                i += 1;
            }
        }
    }
    words.extend(word);
    (!words.is_empty()).then_some(Simple {
        words,
        stdin,
        piped: None,
    })
}

/// Where a quoted stretch starting at `start` ends, past its closing quote.
fn quoted(chars: &[char], start: usize) -> usize {
    let quote = chars[start];
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' if quote != '\'' => i += 2,
            c if c == quote => return i + 1,
            '$' if quote == '"' && chars.get(i + 1) == Some(&'(') => {
                i = substitution(chars, i + 1);
            }
            _ => i += 1,
        }
    }
    chars.len()
}

/// Where a parameter expansion whose `{` is at `open` ends, past its `}`.
fn braces(chars: &[char], open: usize) -> usize {
    let mut depth = 0;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            '\'' | '"' => {
                i = quoted(chars, i);
                continue;
            }
            '\\' => i += 1,
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

/// Where a substitution whose `(` is at `open` ends, past its `)`.
fn substitution(chars: &[char], open: usize) -> usize {
    let mut depth = 0;
    let mut i = open;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    while i < chars.len() {
        match chars[i] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            '\'' | '"' | '`' => {
                i = quoted(chars, i);
                continue;
            }
            '\\' => i += 1,
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                let (delimiter, strip, end) = delimiter(chars, i + 2);
                heredocs.push((delimiter, strip));
                i = end;
                continue;
            }
            '\n' if !heredocs.is_empty() => {
                let mut at = i + 1;
                for (delimiter, strip) in heredocs.drain(..) {
                    at = body(chars, at, &delimiter, strip).1;
                }
                i = at;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

/// A substitution as a word: `$(cat <<EOF … EOF)` is its document, any
/// other stays as written.
fn substituted(raw: &[char]) -> String {
    let text: String = raw.iter().collect();
    // An unclosed one is the shell's error to report, not a word to read.
    let Some(inner) = text.strip_prefix("$(").and_then(|t| t.strip_suffix(')')) else {
        return text;
    };
    if let Some(simple) = simple(inner)
        && simple.words == ["cat"]
        && let Some(document) = simple.stdin
    {
        return document;
    }
    text
}

/// A here-document's delimiter after `<<` at `start`: its word without
/// quotes, whether `<<-` strips tabs, and where the word ends.
fn delimiter(chars: &[char], start: usize) -> (String, bool, usize) {
    let mut i = start;
    let strip = chars.get(i) == Some(&'-');
    if strip {
        i += 1;
    }
    while matches!(chars.get(i), Some(' ' | '\t')) {
        i += 1;
    }
    let end = simple_word_end(chars, i);
    let word: String = chars[i..end]
        .iter()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect();
    (word, strip, end)
}

/// Where an unquoted-or-quoted word starting at `start` ends.
fn simple_word_end(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            ' ' | '\t' | '\n' | ';' | '|' | '&' | ')' | '<' | '>' => break,
            '\'' | '"' => i = quoted(chars, i),
            _ => i += 1,
        }
    }
    i
}

/// A here-document's text from `start` up to its delimiter line, and
/// where what follows it begins.
fn body(chars: &[char], start: usize, delimiter: &str, strip: bool) -> (String, usize) {
    let rest: String = chars[start.min(chars.len())..].iter().collect();
    let mut text = String::new();
    let mut used = 0;
    for line in rest.split_inclusive('\n') {
        used += line.chars().count();
        let bare = line.trim_end_matches('\n');
        let bare = if strip {
            bare.trim_start_matches('\t')
        } else {
            bare
        };
        if bare == delimiter {
            return (text, start + used);
        }
        text.push_str(bare);
        text.push('\n');
    }
    (text, chars.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unclosed_substitution_stays_as_written() {
        let read = simple("echo $(unterminated").unwrap();
        assert_eq!(read.words, ["echo", "$(unterminated"]);
    }

    #[test]
    fn a_line_splits_outside_quotes_substitutions_and_documents() {
        assert_eq!(
            parts(
                "ls \"$HOME/.agent\"; \"$AGENT_BIN\" run --bot x -- 'a; b && c' && env | grep rex\npwd"
            ),
            [
                "ls \"$HOME/.agent\"",
                "\"$AGENT_BIN\" run --bot x -- 'a; b && c'",
                "env | grep rex",
                "pwd"
            ]
        );
        let line = "h=$(\"$AGENT_BIN\" run --bot x -- \"$(cat <<'EOF'\nFix it; then test\nEOF\n)\")\n\"$AGENT_BIN\" wait \"$h\"";
        assert_eq!(parts(line).len(), 2);
        let line = "\"$AGENT_BIN\" run --bot x <<'EOF'\nline one\nline two; still\nEOF\npwd";
        assert_eq!(parts(line).len(), 2);
    }

    #[test]
    fn a_command_reads_its_document_and_drops_redirections() {
        let one = simple("\"$AGENT_BIN\" run --bot x -- \"$(cat <<'EOF'\nFix it.\nEOF\n)\" 2>&1")
            .unwrap();
        assert_eq!(
            one.words,
            ["$AGENT_BIN", "run", "--bot", "x", "--", "Fix it.\n"]
        );
        let two = simple("\"$AGENT_BIN\" run --bot x <<'EOF'\nBrief\nEOF").unwrap();
        assert_eq!(two.stdin.as_deref(), Some("Brief\n"));
        let three = simple("\"$AGENT_BIN\" ls >/dev/null | jq -r '.[].name'").unwrap();
        assert_eq!(three.words, ["$AGENT_BIN", "ls"]);
        assert_eq!(three.piped.as_deref(), Some("jq -r '.[].name'"));
        assert!(simple("( cd x && agent ls )").is_none());
    }
}
