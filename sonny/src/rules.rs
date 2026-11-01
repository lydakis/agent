//! Command rules files, read as Codex's execpolicy reads them, and given
//! to each agent Sonny makes as its `rules` setting
//! ([`agent_client::rules`]), which the daemon applies to its shell calls.
//!
//! Sonny reads every `*.rules` file in `~/.agent/rules`, in name order. A
//! file is Starlark, run in its extended dialect with f-strings and the
//! standard globals, as Codex runs it; `prefix_rule` and `host_executable`
//! are ported from Codex's `execpolicy` crate (Apache-2.0; see NOTICE), so
//! a rules file written for Codex works here unchanged. Codex's
//! `network_rule` is refused: it needs a proxy that filters a sandbox's
//! network by host.
//!
//! The folder is read again when a file in it changes. Rules that fail to
//! read keep the rules last read; with none read yet, they are given as
//! invalid, and the daemon refuses every shell call with why. The folder is
//! read-only in every sandbox (the CLI starts the daemon with
//! `--protect-path`), and a file in it that is a link, which the sandbox
//! would not keep read-only, does not read.
use agent_client::rules::{Decision, Policy, PrefixRule, Word};
use starlark::any::ProvidesStaticType;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value;
use starlark::values::list::{ListRef, UnpackList};
use starlark::values::none::NoneType;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

pub const USAGE: &str = "sonny rules check [--pretty] [--] COMMAND...";

/// Every `*.rules` file in `folder`, in name order, as one set of rules;
/// none when the folder is missing. What fails to read says where.
pub fn load(folder: &Path) -> Result<Policy, String> {
    let mut policy = Policy::default();
    for path in files(folder)? {
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        parse(&mut policy, &name, &text)?;
    }
    Ok(policy)
}

/// The rules in `text`, a file named `name`, added to `policy`; none are
/// added when any of them is wrong.
pub fn parse(policy: &mut Policy, name: &str, text: &str) -> Result<(), String> {
    let built = run(name, text)?;
    policy.prefix_rules.extend(built.rules);
    policy.host_executables.extend(built.hosts);
    Ok(())
}

/// What a file's calls add as it runs.
#[derive(Debug, Default, ProvidesStaticType)]
struct Builder {
    rules: Vec<PrefixRule>,
    hosts: BTreeMap<String, Vec<String>>,
    /// Each rule's examples, checked once the whole file has run, as
    /// Codex checks them: against that rule alone, with the file's
    /// `host_executable` paths.
    examples: Vec<Examples>,
}

/// A rule and the commands it says it matches and doesn't.
type Examples = (PrefixRule, Vec<Vec<String>>, Vec<Vec<String>>);

impl Builder {
    fn check_examples(&self) -> Result<(), String> {
        for (rule, matches, not_matches) in &self.examples {
            // Whether it matches, whatever it decides: an allow rule lets
            // out no path `host_executable` doesn't name, but matches it.
            let alone = Policy {
                prefix_rules: vec![PrefixRule {
                    decision: Decision::Prompt,
                    ..rule.clone()
                }],
                host_executables: self.hosts.clone(),
                invalid: None,
            };
            let at = rule.source.as_deref().unwrap_or_default();
            for example in not_matches {
                if !alone.matching(example).is_empty() {
                    return Err(format!(
                        "{at}: the rule matches `{}`, which it says it doesn't",
                        example.join(" ")
                    ));
                }
            }
            for example in matches {
                if alone.matching(example).is_empty() {
                    return Err(format!(
                        "{at}: the rule doesn't match its example `{}`",
                        example.join(" ")
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Runs one rules file as Codex does: Starlark's extended dialect with
/// f-strings and its standard globals, plus the rule builtins.
fn run(name: &str, text: &str) -> Result<Builder, String> {
    let mut dialect = Dialect::Extended.clone();
    dialect.enable_f_strings = true;
    let ast = AstModule::parse(name, text.to_owned(), &dialect).map_err(said)?;
    let globals = GlobalsBuilder::standard().with(builtins).build();
    let builder = RefCell::new(Builder::default());
    Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&builder);
        eval.eval_module(ast, &globals).map(|_| ())
    })
    .map_err(said)?;
    let builder = builder.into_inner();
    builder.check_examples()?;
    Ok(builder)
}

/// A Starlark error on one line: `FILE:LINE:COLUMN: what`, from the
/// traceback Starlark writes.
fn said(error: starlark::Error) -> String {
    let text = error.to_string();
    let what = (text.lines()).find_map(|line| line.strip_prefix("error: "));
    let at = (text.lines()).find_map(|line| line.trim_start().strip_prefix("--> "));
    match (at, what) {
        (Some(at), Some(what)) => format!("{}: {}", at.trim(), what.trim()),
        _ => text.trim_end().to_owned(),
    }
}

fn builder<'a>(eval: &Evaluator<'_, 'a, '_>) -> std::cell::RefMut<'a, Builder> {
    eval.extra
        .as_ref()
        .and_then(|extra| extra.downcast_ref::<RefCell<Builder>>())
        .expect("a rules file runs with its builder")
        .borrow_mut()
}

/// Where the call being made is: `FILE:LINE`.
fn here(eval: &Evaluator<'_, '_, '_>) -> Option<String> {
    let span = eval.call_stack_top_location()?;
    Some(format!(
        "{}:{}",
        span.filename(),
        span.resolve_span().begin.line + 1
    ))
}

fn strings(list: &ListRef<'_>, what: &str) -> anyhow::Result<Vec<String>> {
    (list.content().iter())
        .map(|value| {
            value
                .unpack_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("{what} are strings, not {}", value.get_type()))
        })
        .collect()
}

fn pattern(pattern: UnpackList<Value<'_>>) -> anyhow::Result<Vec<Word>> {
    let mut words = Vec::new();
    for value in pattern.items {
        let word = if let Some(word) = value.unpack_str() {
            Word::One(word.to_owned())
        } else if let Some(list) = ListRef::from_value(value) {
            match strings(list, "a pattern's alternatives")?.as_slice() {
                [] => anyhow::bail!("a pattern's alternatives may not be empty"),
                [one] => Word::One(one.clone()),
                any => Word::Any(any.to_vec()),
            }
        } else {
            anyhow::bail!(
                "a pattern's words are strings or lists of them, not {}",
                value.get_type()
            );
        };
        words.push(word);
    }
    if words.is_empty() {
        anyhow::bail!("pattern is empty");
    }
    Ok(words)
}

/// Examples, each a string read as shell words, as Codex reads them, or a
/// list of words.
fn examples(examples: Option<UnpackList<Value<'_>>>) -> anyhow::Result<Vec<Vec<String>>> {
    let Some(examples) = examples else {
        return Ok(Vec::new());
    };
    let mut read = Vec::new();
    for value in examples.items {
        let words = if let Some(text) = value.unpack_str() {
            shlex::split(text)
                .ok_or_else(|| anyhow::anyhow!("example {text:?} isn't shell words"))?
        } else if let Some(list) = ListRef::from_value(value) {
            strings(list, "an example's words")?
        } else {
            anyhow::bail!(
                "an example is a string or a list of strings, not {}",
                value.get_type()
            );
        };
        if words.is_empty() {
            anyhow::bail!("an example is empty");
        }
        read.push(words);
    }
    Ok(read)
}

#[starlark_module]
fn builtins(globals: &mut GlobalsBuilder) {
    fn prefix_rule<'v>(
        pattern: UnpackList<Value<'v>>,
        decision: Option<&'v str>,
        r#match: Option<UnpackList<Value<'v>>>,
        not_match: Option<UnpackList<Value<'v>>>,
        justification: Option<&'v str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let decision = match decision {
            None => Decision::Allow,
            Some(decision) => Decision::parse(decision).map_err(anyhow::Error::msg)?,
        };
        if justification.is_some_and(|text| text.trim().is_empty()) {
            anyhow::bail!("justification is empty");
        }
        let rule = PrefixRule {
            pattern: self::pattern(pattern)?,
            decision,
            justification: justification.map(str::to_owned),
            source: here(eval),
        };
        let (matches, not_matches) = (examples(r#match)?, examples(not_match)?);
        let mut builder = builder(eval);
        builder.examples.push((rule.clone(), matches, not_matches));
        builder.rules.push(rule);
        Ok(NoneType)
    }

    /// Codex's rule for a host's network, which needs a proxy that
    /// filters by host: refused until there is one, so no rule the
    /// sandbox wouldn't keep is taken as kept.
    fn network_rule<'v>(
        host: &'v str,
        protocol: &'v str,
        decision: &'v str,
        justification: Option<&'v str>,
    ) -> anyhow::Result<NoneType> {
        let _ = (host, protocol, decision, justification);
        anyhow::bail!(
            "network_rule is not supported: a sandbox's network is on or off, with no proxy to filter it by host"
        )
    }

    /// Where a program named by its bare name may be found: a command run
    /// by one of these paths is ruled as one run by the name.
    fn host_executable<'v>(
        name: &'v str,
        paths: UnpackList<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let bare = Path::new(name);
        if name.is_empty()
            || bare.components().count() != 1
            || bare.file_name().and_then(|n| n.to_str()) != Some(name)
        {
            anyhow::bail!("host_executable's name is a program's bare name, not {name:?}");
        }
        let mut found: Vec<String> = Vec::new();
        for value in paths.items {
            let Some(path) = value.unpack_str() else {
                anyhow::bail!(
                    "host_executable's paths are strings, not {}",
                    value.get_type()
                );
            };
            if !Path::new(path).is_absolute() {
                anyhow::bail!("host_executable's paths are absolute, not {path:?}");
            }
            if Path::new(path).file_name().and_then(|n| n.to_str()) != Some(name) {
                anyhow::bail!("host_executable path {path:?} must end in `{name}`");
            }
            if !found.iter().any(|kept| kept == path) {
                found.push(path.to_owned());
            }
        }
        builder(eval).hosts.insert(name.to_owned(), found);
        Ok(NoneType)
    }
}

/// A folder of rules, read again when its files change.
#[derive(Debug, Default)]
pub struct Folder {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The files as last read; None before the first read.
    seen: Option<Vec<Stamp>>,
    current: Policy,
    /// Whether `current` was read from the files, not refused.
    read: bool,
}

/// A rules file as the folder lists it: what changes when it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    changed: (i64, i64),
}

impl Folder {
    /// The rules in `path`; with none, no rules at all.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            state: Mutex::default(),
        }
    }

    /// The rules as the folder holds them now: read again when its files
    /// changed since, and kept as they were when the new ones don't read.
    /// With none read yet, ones that don't read are given as invalid.
    pub fn current(&self) -> Policy {
        let Some(path) = &self.path else {
            return Policy::default();
        };
        let stamps = stamps(path);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.seen.as_ref() == Some(&stamps) {
            return state.current.clone();
        }
        match load(path) {
            Ok(policy) => {
                state.current = policy;
                state.read = true;
            }
            // The rules last read stay, so a file saved half-written
            // loses no rule.
            Err(error) if state.read => {
                println!(
                    "{}",
                    serde_json::json!({"event": "rules_unread", "error": error})
                );
            }
            Err(error) => {
                state.current = Policy {
                    invalid: Some(error),
                    ..Policy::default()
                };
            }
        }
        state.seen = Some(stamps);
        state.current.clone()
    }
}

/// Sonny's rules, in `~/.agent/rules`.
static RULES: LazyLock<Folder> = LazyLock::new(|| Folder::new(agent_client::config::rules()));

/// What Sonny gives an agent as its `rules` setting now: none when there
/// are no rules.
pub fn given() -> Option<Policy> {
    Some(RULES.current()).filter(|policy| *policy != Policy::default())
}

/// The `*.rules` files in `folder`, in name order: none when it is missing.
fn files(folder: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", folder.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", folder.display()))?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "rules") {
            continue;
        }
        // A file gone since the listing is not one, but one that can't be
        // looked at is no rule at all: refused, so it is not read as none.
        let found = match std::fs::symlink_metadata(&path) {
            Ok(found) => found,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        // The sandbox keeps the folder read-only, not what a link in it
        // names, nor another name for the same file: an agent could write
        // one of those and so its own rules.
        let linked = {
            use std::os::unix::fs::MetadataExt;
            found.is_file() && found.nlink() > 1
        };
        if found.file_type().is_symlink() || linked {
            return Err(format!(
                "{}: a rules file is a file of its own in the rules folder, not a link",
                path.display()
            ));
        }
        if found.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// What says whether `folder`'s rules changed: each file's size and times.
/// A folder that can't be listed lists as it is, so its error is read.
fn stamps(folder: &Path) -> Vec<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let Ok(files) = files(folder) else {
        return vec![Stamp {
            path: folder.to_owned(),
            len: u64::MAX,
            modified: None,
            changed: (0, 0),
        }];
    };
    files
        .into_iter()
        .map(|path| {
            let found = std::fs::metadata(&path).ok();
            Stamp {
                len: found.as_ref().map_or(0, |found| found.len()),
                modified: found.as_ref().and_then(|found| found.modified().ok()),
                changed: (found.as_ref()).map_or((0, 0), |f| (f.ctime(), f.ctime_nsec())),
                path,
            }
        })
        .collect()
}

/// `sonny rules check [--pretty] [--] COMMAND...`: what Sonny's rules say
/// of one command, its words as given, as `codex execpolicy check` reads
/// them. Exits 1 when the files don't read.
pub fn cli(args: &[String]) -> i32 {
    let (mut pretty, mut words) = (false, Vec::new());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => words.extend(iter.by_ref().cloned()),
            "--pretty" => pretty = true,
            "-h" | "--help" => {
                println!("usage: {USAGE}");
                return 0;
            }
            _ => words.push(arg.clone()),
        }
    }
    let command = match words.split_first() {
        Some((check, command)) if check == "check" && !command.is_empty() => command,
        _ => {
            eprintln!("usage: {USAGE}");
            return 2;
        }
    };
    let Some(folder) = agent_client::config::rules() else {
        eprintln!("sonny rules: HOME is not set");
        return 1;
    };
    match check(&folder, command, pretty) {
        Ok(said) => {
            print!("{said}");
            0
        }
        Err(error) => {
            eprintln!("sonny rules: {error}");
            1
        }
    }
}

/// What the rules in `folder` say of `command`: JSON on a line, or as a
/// person reads it.
fn check(folder: &Path, command: &[String], pretty: bool) -> Result<String, String> {
    let verdict = load(folder)?.verdict(&[command.to_vec()]);
    if !pretty {
        return Ok(format!("{}\n", serde_json::json!(verdict)));
    }
    let name = |decision: Decision| match decision {
        Decision::Allow => "allow",
        Decision::Prompt => "prompt",
        Decision::Forbidden => "forbidden",
    };
    let mut said = match verdict.decision {
        Some(decision) => format!("{}\n", name(decision)),
        None => "no rule matches\n".to_owned(),
    };
    for matched in &verdict.matched {
        let at = matched.source.as_deref().unwrap_or("a rule");
        let prefix = matched.prefix.join(" ");
        said.push_str(&format!("  {} {prefix} ({at})\n", name(matched.decision)));
        if let Some(why) = &matched.justification {
            said.push_str(&format!("    {why}\n"));
        }
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(text: &str) -> Result<Policy, String> {
        let mut policy = Policy::default();
        parse(&mut policy, "test.rules", text)?;
        Ok(policy)
    }

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(String::from).collect()
    }

    fn decision(policy: &Policy, command: &str) -> Option<Decision> {
        policy.verdict(&[words(command)]).decision
    }

    #[test]
    fn rules_read_as_codex_writes_them() {
        let policy = policy(
            r#"# Rules.
prefix_rule(pattern = ["git"], decision = "prompt")
prefix_rule(
    pattern = ["git", "push", ["--force", "-f"]],
    decision = "forbidden",
    justification = """Use
--force-with-lease.""",
    match = ["git push --force origin main", ["git", "push", "-f"]],
    not_match = ["git push origin main"],
)
prefix_rule(pattern=["cargo", ["test", 'check']], decision="allow")
prefix_rule(["say"], "forbidden", justification = "No \"say\"\tthere é\x21")
"#,
        )
        .unwrap();
        assert_eq!(policy.prefix_rules.len(), 4);
        assert_eq!(decision(&policy, "git push -f"), Some(Decision::Forbidden));
        assert_eq!(decision(&policy, "git push origin"), Some(Decision::Prompt));
        assert_eq!(decision(&policy, "cargo check"), Some(Decision::Allow));
        assert_eq!(decision(&policy, "cargo build"), None);
        let rule = &policy.prefix_rules[1];
        assert_eq!(rule.source.as_deref(), Some("test.rules:3"));
        assert_eq!(
            rule.justification.as_deref(),
            Some("Use\n--force-with-lease.")
        );
        assert_eq!(
            policy.prefix_rules[2].pattern,
            [
                Word::One("cargo".into()),
                Word::Any(vec!["test".into(), "check".into()])
            ]
        );
        assert_eq!(
            policy.prefix_rules[3].justification.as_deref(),
            Some("No \"say\"\tthere \u{e9}!")
        );
        // What "always allow" writes reads back.
        let words = ["cargo", "test", "it's \"quoted\" \\ \u{1F600}"].map(String::from);
        let line = format!(
            "prefix_rule(pattern={}, decision=\"allow\")",
            serde_json::to_string(&words).unwrap()
        );
        let written = super::tests::policy(&line).unwrap();
        assert_eq!(
            written.prefix_rules[0].pattern,
            words.map(Word::One).to_vec()
        );
    }

    #[test]
    fn a_rules_file_is_starlark() {
        // Variables, loops, functions and f-strings, as Codex runs them.
        let policy = policy(
            r#"
linters = ["ruff", "mypy"]
for tool in linters:
    prefix_rule(pattern = [tool], match = [tool + " check"])

def ask(*words, why = None):
    prefix_rule(pattern = list(words), decision = "prompt", justification = why)

ask("git", "push", why = f"Pushing goes to {'a remote'}.")
"#,
        )
        .unwrap();
        assert_eq!(policy.prefix_rules.len(), 3);
        assert_eq!(decision(&policy, "mypy x"), Some(Decision::Allow));
        let rule = &policy.prefix_rules[2];
        assert_eq!(
            rule.justification.as_deref(),
            Some("Pushing goes to a remote.")
        );
        // A rule made in a function is placed where it was made.
        assert_eq!(rule.source.as_deref(), Some("test.rules:7"));
    }

    #[test]
    fn a_program_by_its_path_is_ruled_by_its_name_where_codex_finds_it() {
        let policy = policy(
            r#"
prefix_rule(pattern = ["git", "push"], decision = "forbidden")
host_executable(name = "git", paths = ["/usr/bin/git", "/opt/homebrew/bin/git"])
prefix_rule(
    pattern = ["rg"],
    match = ["/usr/local/bin/rg x"],
    not_match = ["rgx"],
)
"#,
        )
        .unwrap();
        assert_eq!(
            decision(&policy, "/opt/homebrew/bin/git push"),
            Some(Decision::Forbidden)
        );
        assert_eq!(decision(&policy, "/tmp/git push"), None);
        // Allowed by its name only where host_executable says it is, since
        // a bot could write a program of that name anywhere else.
        assert_eq!(decision(&policy, "/usr/local/bin/rg x"), None);
        assert_eq!(decision(&policy, "rg x"), Some(Decision::Allow));
        let error =
            super::tests::policy("host_executable(name = \"git\", paths = [\"/usr/bin/hg\"])")
                .unwrap_err();
        assert!(error.contains("must end in `git`"), "{error}");
    }

    #[test]
    fn a_rule_that_doesnt_read_says_where() {
        for (text, wanted) in [
            (
                "prefix_rule(pattern = [])",
                "test.rules:1:1: pattern is empty",
            ),
            (
                "\n\nprefix_rule(pattern = [\"git\"], decision = \"deny\")",
                "test.rules:3:1: decision must be allow, prompt or forbidden, not \"deny\"",
            ),
            (
                "prefix_rule(pattern = [\"git\", \"push\"], match = [\"git pull\"])",
                "test.rules:1: the rule doesn't match its example `git pull`",
            ),
            (
                "prefix_rule(pattern = [\"git\"], not_match = [[\"git\", \"x\"]])",
                "test.rules:1: the rule matches `git x`, which it says it doesn't",
            ),
            (
                "prefix_rule(pattern = [\"git\"], justification = \" \")",
                "justification is empty",
            ),
            (
                "network_rule(host = \"x\", protocol = \"https\", decision = \"deny\")",
                "network_rule is not supported",
            ),
            ("prefix_rule(pattern = [\"git\"]", "test.rules:1:"),
            (
                "prefix_rule(pattern = [\"git\"], color = \"red\")",
                "test.rules:1:",
            ),
            ("prefix_rule(pattern = [tool])", "test.rules:1:"),
        ] {
            let error = policy(text).unwrap_err();
            assert!(error.contains(wanted), "{text}: {error}");
            assert!(!error.contains('\n'), "{error}");
        }
    }

    #[test]
    fn a_folder_reads_again_when_its_files_change_and_keeps_good_rules() {
        let dir = std::env::temp_dir().join(format!("sonny-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folder = Folder::new(Some(dir.clone()));
        let rm = |folder: &Folder| decision(&folder.current(), "rm x");
        // No folder, no rules.
        assert_eq!(folder.current(), Policy::default());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("b.rules"),
            "prefix_rule(pattern = [\"rm\"], decision = \"forbidden\")",
        )
        .unwrap();
        std::fs::write(dir.join("a.rules"), "prefix_rule(pattern = [\"rm\"])").unwrap();
        std::fs::write(dir.join("notes.txt"), "not rules").unwrap();
        assert_eq!(rm(&folder), Some(Decision::Forbidden));
        // A half-written file keeps the rules last read.
        std::fs::write(dir.join("b.rules"), "prefix_rule(pattern = [").unwrap();
        assert_eq!(rm(&folder), Some(Decision::Forbidden));
        std::fs::write(dir.join("b.rules"), "").unwrap();
        assert_eq!(rm(&folder), Some(Decision::Allow));
        // With none read yet, a broken file makes the rules invalid.
        let fresh = Folder::new(Some(dir.clone()));
        std::fs::write(dir.join("b.rules"), "prefix_rule(pattern = [").unwrap();
        let invalid = fresh.current().invalid.unwrap();
        assert!(invalid.contains("b.rules:1"), "{invalid}");
        std::fs::write(dir.join("b.rules"), "# fixed\n").unwrap();
        assert_eq!(rm(&fresh), Some(Decision::Allow));
        // A link, which the sandbox wouldn't keep read-only, is refused.
        let elsewhere = dir.with_extension("elsewhere");
        std::fs::write(&elsewhere, "prefix_rule(pattern = [\"rm\"])").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("c.rules")).unwrap();
        assert!(load(&dir).unwrap_err().contains("c.rules: a rules file"));
        std::fs::remove_file(dir.join("c.rules")).unwrap();
        std::fs::hard_link(&elsewhere, dir.join("c.rules")).unwrap();
        assert!(load(&dir).is_err());
        std::fs::remove_file(&elsewhere).unwrap();
        std::fs::remove_file(dir.join("c.rules")).unwrap();
        // A folder that lists but can't be looked into is refused, not read
        // as no rules: the rules last read stay. Root looks in regardless.
        // SAFETY: geteuid takes nothing and always succeeds.
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let forbid = "prefix_rule(pattern = [\"rm\"], decision = \"forbidden\")";
            std::fs::write(dir.join("b.rules"), forbid).unwrap();
            assert_eq!(rm(&folder), Some(Decision::Forbidden));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o644)).unwrap();
            let refused = load(&dir).is_err();
            let kept = rm(&folder);
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(refused);
            assert_eq!(kept, Some(Decision::Forbidden));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rules_check_says_what_the_rules_decide() {
        let dir = std::env::temp_dir().join(format!("sonny-rules-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("default.rules"),
            "# Mine.\n\
             prefix_rule(pattern=[\"rm\", [\"-rf\", \"-fr\"]], decision=\"forbidden\",\n\
             \x20           justification=\"Use the trash.\", match=[\"rm -rf x\"])\n\
             prefix_rule(pattern=[\"git\", \"push\"], decision=\"prompt\")\n",
        )
        .unwrap();
        let check = |command: &str, pretty| check(&dir, &words(command), pretty);
        let said: serde_json::Value =
            serde_json::from_str(&check("rm -rf build", false).unwrap()).unwrap();
        assert_eq!(
            said,
            serde_json::json!({"decision": "forbidden", "matched": [{"decision": "forbidden",
                "justification": "Use the trash.", "prefix": ["rm", "-rf"],
                "source": "default.rules:2"}]})
        );
        assert_eq!(check("ls", false).unwrap(), "{\"matched\":[]}\n");
        assert_eq!(
            check("git push --force", true).unwrap(),
            "prompt\n  prompt git push (default.rules:4)\n"
        );
        std::fs::write(
            dir.join("later.rules"),
            "for tool in [\"ruff\"]:\n    prefix_rule(pattern=[tool], decision=\"ask\")\n",
        )
        .unwrap();
        let error = check("ls", false).unwrap_err();
        assert!(error.starts_with("later.rules:2:"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
