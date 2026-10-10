//! Shared CLI grammar. Validation happens before touching the store or socket.
use agent_runtime::{Error, Result, fail_with};

const CONNECTION: &str = "--store --socket";
const STARTUP: &str = "--provider --max-processes --max-detached --max-active --max-connecting --max-pending --max-pending-bytes --stall-timeout --idle-exit";

struct Command {
    name: &'static str,
    /// One line for `agent --help`.
    about: &'static str,
    usage: &'static str,
    flags: &'static str,
    /// A new bot's settings, listed apart: the defaults are right.
    settings: &'static str,
    startup: bool,
}

const COMMANDS: &[Command] = &[
    Command {
        name: "run",
        about: "Send a prompt to a bot, or make one with --new; prints the turn as it runs",
        usage: "run [OPTIONS] [--] PROMPT...",
        flags: "--bot --new --detach --delivery --turn --model --workspace --effort --turn-budget-tokens --agents --profile --request-id --bot-id --pretty --no-spawn",
        settings: "--tools --instructions --instructions-file --budget-tokens --compaction-instructions --compaction-instructions-file --compaction-model --no-compaction --fallbacks --approval --approve --context-bytes --context-items --note-turns --compact-at --compact-keep --keep-turns --approval-hold --max-output-tokens --keep-warm --cache-ttl",
        startup: true,
    },
    Command {
        name: "follow",
        about: "Stream a bot's events, or every bot's with --all",
        usage: "follow (--bot NAME | --all) [--after CURSOR]",
        flags: "--bot --all --after --pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "fork",
        about: "Copy a bot's conversation up to a message into a new bot",
        usage: "fork --source NAME --bot NAME [--checkpoint NODE] [--allow LIST]",
        flags: "--source --bot --checkpoint --workspace --budget-tokens --allow --request-id --pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "interrupt",
        about: "Stop a bot's running turn; prints its turn view",
        usage: "interrupt --bot NAME",
        flags: "--bot --pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "ls",
        about: "List the bots",
        usage: "ls [OPTIONS]",
        flags: "--pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "turns",
        about: "A bot's turns and how each ended",
        usage: "turns --bot NAME [--after TURN]",
        flags: "--bot --after --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "wait",
        about: "A turn's or command's result: status, text, usage; --timeout 0 polls",
        usage: "wait [--any] [--timeout DURATION] HANDLE...",
        flags: "--any --timeout --pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "rm",
        about: "Delete an idle bot; prints what was freed",
        usage: "rm --bot NAME [--bot-id N]",
        flags: "--bot --bot-id --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "prune",
        about: "Drop a bot's older turns' operational records, keeping the newest N",
        usage: "prune --bot NAME --keep-turns N",
        flags: "--bot --keep-turns --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "approvals",
        about: "Tool calls waiting for a verdict",
        usage: "approvals [--bot NAME] [--tag TAG] [--full]",
        flags: "--bot --tag --full --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "answer",
        about: "Allow or deny a waiting tool call",
        usage: "answer --bot NAME --turn TURN --call ID --request N [--tag TAG] [--reason TEXT] allow|deny",
        flags: "--bot --turn --call --request --tag --reason --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "approver",
        about: "Judge a gate's calls with a model, until stopped",
        usage: "approver [--tag TAG] [--judge PROVIDER/MODEL] [--effort LEVEL] [--note FILE] [--judge-url URL]",
        flags: "--tag --judge --effort --note --judge-url",
        settings: "",
        startup: false,
    },
    Command {
        name: "models",
        about: "The models the providers serve",
        usage: "models [--discover]",
        flags: "--discover --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "start",
        about: "Start the store's daemon if none runs; prints its ready line",
        usage: "start [OPTIONS]",
        flags: "--pretty",
        settings: "",
        startup: true,
    },
    Command {
        name: "stats",
        about: "The daemon's limits and load",
        usage: "stats [OPTIONS]",
        flags: "--pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "shutdown",
        about: "Stop the daemon once running turns end or the grace passes",
        usage: "shutdown [--grace DURATION]",
        flags: "--grace --pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "serve",
        about: "Run the daemon in the foreground; other commands start it for you",
        usage: "serve --store PATH --provider SPEC... [OPTIONS]",
        flags: "",
        settings: "",
        startup: true,
    },
];

fn command(name: &str) -> Result<&'static Command> {
    COMMANDS
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| Error::with("usage", format!("unknown command {name}; use agent --help")))
}

pub fn help(name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        let c = command(name)?;
        println!("Usage: agent {}\n\nOptions:", c.usage);
        print_flags(CONNECTION);
        print_flags(c.flags);
        println!("  -h, --help                   Show help");
        if !c.settings.is_empty() {
            println!("\nNew bot settings (the defaults are right; rarely needed):");
            print_flags(c.settings);
        }
        if c.startup {
            println!("\nDaemon options (client commands apply these on startup):");
            print_flags(STARTUP);
        }
    } else {
        println!("Usage: agent COMMAND [OPTIONS]\n\nCommands:");
        for c in COMMANDS {
            println!("  {:<11}{}", c.name, c.about);
        }
        println!(
            "\nUse agent COMMAND --help for its usage and flags.\n  -h, --help     Show help\n  --version      Show version"
        );
    }
    println!(
        "\nValue flags accept --flag VALUE or --flag=VALUE; -- ends option parsing.\nJSON is the default; --pretty selects human output.\nExit status: 0 success, 1 failed/incomplete operation, 2 invalid usage.\nProvider: NAME[=FAMILY[,BASE_URL[,KEY_ENV]]]; families: responses, responses-ws (Responses over WebSocket), anthropic."
    );
    Ok(())
}

fn print_flags(flags: &str) {
    for flag in flags.split_whitespace() {
        let (value, description) = match flag {
            "--store" => ("PATH", "Select the durable store"),
            "--socket" => ("PATH", "Select the daemon socket"),
            "--bot" => ("NAME", "Select a bot; for fork, name the destination"),
            "--source" => ("NAME", "Select the bot to fork"),
            "--checkpoint" => (
                "NODE",
                "Fork at this history node; default: an idle source's head, or a running turn's newest finished round",
            ),
            "--after" => ("ID", "Start after this event cursor or turn ID"),
            "--turn" => (
                "ID",
                "Select a turn; with run --delivery steer, the turn to steer",
            ),
            "--new" => ("", "Create a new bot instead of continuing a named bot"),
            "--detach" => ("", "Submit and return a JSON turn handle immediately"),
            "--delivery" => (
                "MODE",
                "If the bot is busy: reject, queue, or steer; default AGENT_DELIVERY or reject",
            ),
            "--all" => ("", "Follow every bot on one connection"),
            "--any" => ("", "Return when the first handle resolves"),
            "--timeout" => ("DURATION", "Wait at most this long; 0 polls immediately"),
            "--workspace" => (
                "DIR",
                "The bot's folder: a new bot's defaults to here, a fork's to its source's; run moves a bot there",
            ),
            "--instructions" => (
                "TEXT",
                "A new bot's instructions (default: the built-in text)",
            ),
            "--instructions-file" => ("FILE", "Read the instructions from a file"),
            "--agents" => (
                "",
                "Compose instructions: the preamble, AGENTS.md files from the workspace up, skills and profiles",
            ),
            "--profile" => (
                "ROLE",
                "Compose instructions (as --agents) in the role .agents/agents/ROLE.md, with its model and tools",
            ),
            "--effort" => (
                "LEVEL",
                "Effort: low, medium, high, or xhigh; Claude also max. A new bot keeps it (default AGENT_EFFORT on AGENT_MODEL, else the model's own); on an existing bot, for this turn",
            ),
            "--request-id" => (
                "ID",
                "Idempotency key: resending the same command gets what it made, duplicate: true",
            ),
            "--bot-id" => (
                "N",
                "The identity --bot must name; rm then succeeds as a duplicate once it is gone",
            ),
            "--budget-tokens" => ("N", "New bot's lifetime input + output token cap"),
            "--turn-budget-tokens" => (
                "N",
                "This turn's input + output token cap, beside the bot's own budget",
            ),
            "--pretty" => ("", "Render human-readable output"),
            "--no-spawn" => ("", "Require an already running daemon"),
            "--keep-turns" => (
                "N",
                "Keep the latest N turns' operational records; for a new bot, after each of its turns",
            ),
            "--provider" => ("SPEC", "Register a provider; repeat for multiple providers"),
            "--discover" => (
                "",
                "Write ~/.agent/models from the providers' own model listings",
            ),
            "--model" => (
                "PROVIDER/MODEL",
                "Set the model; new bots default to AGENT_MODEL, existing bots keep theirs",
            ),
            "--tools" => (
                "LIST",
                "A new bot's tools; default shell,read,write,edit,wait,history",
            ),
            "--allow" => (
                "LIST",
                "The tools a fork may call, within its source's; '' allows none; default: the source's",
            ),
            "--max-processes" => ("N", "Concurrent process limit; 0 is unbounded"),
            "--max-detached" => ("N", "Detached commands still running; 0 is unbounded"),
            "--max-active" => ("N", "Concurrent active turn limit; 0 is unbounded"),
            "--max-connecting" => ("N", "Concurrent provider startup limit; 0 is unbounded"),
            "--max-pending" => ("N", "Submissions waiting to start; 0 is unbounded"),
            "--max-pending-bytes" => ("N", "Prompt bytes waiting to start; 0 is unbounded"),
            "--max-output-tokens" => (
                "N",
                "A new bot's output token cap per model call; default the model's limit",
            ),
            "--stall-timeout" => (
                "DURATION",
                "Retry a provider stream with no content this long; default 2m",
            ),
            "--keep-warm" => (
                "DURATION",
                "A new bot refreshes its idle Anthropic prompt cache during a tool call after this long; default 4m, 0 disables",
            ),
            "--cache-ttl" => (
                "5m|1h",
                "A new bot's Anthropic prompt-cache lifetime; 1h bills writes at 2x input and sends no refresh; default 5m",
            ),
            "--idle-exit" => ("DURATION", "Exit after this long idle; 0 disables"),
            "--context-bytes" => ("N", "A new bot's model context bytes; default 8 MiB"),
            "--context-items" => ("N", "A new bot's model context items; default 4096"),
            "--note-turns" => (
                "N",
                "Omitted turns a new bot's context note lists; 0 lists none; default 48",
            ),
            "--compact-at" => (
                "PERCENT",
                "A new bot elides answered tool results, then compacts, once its context holds this share of its budget; default 75",
            ),
            "--compact-keep" => (
                "PERCENT",
                "Share of a new bot's context budget kept verbatim by elision and compaction; default 25",
            ),
            "--compaction-instructions" => (
                "TEXT",
                "A new bot's summarizer instructions; default: the built-in text",
            ),
            "--compaction-instructions-file" => (
                "FILE",
                "Read a new bot's summarizer instructions from a file",
            ),
            "--compaction-model" => (
                "PROVIDER/MODEL",
                "A new bot's summarizer; default: its own model",
            ),
            "--no-compaction" => ("", "Create the bot without compaction"),
            "--fallbacks" => (
                "",
                "A new bot's declined Anthropic requests rerun on the recommended model",
            ),
            "--grace" => (
                "DURATION",
                "Let running turns finish for up to this long, starting none; default 0",
            ),
            "--approval" => (
                "MODE",
                "A new bot's approval: full, manual, or auto; default AGENT_APPROVAL or full",
            ),
            "--approve" => (
                "LIST",
                "Tools whose calls need a verdict; default every tool but history, wait, note, echo",
            ),
            "--call" => ("ID", "The tool call to answer"),
            "--full" => ("", "Every call's whole arguments, not their preview"),
            "--request" => ("N", "The request number the call was announced with"),
            "--tag" => (
                "TAG",
                "The gate to list, answer, or serve; default: the call's only gate, or auto",
            ),
            "--note" => (
                "FILE",
                "Text the judge always sees: trusted remotes, hosts, limits; default AGENT_APPROVER_NOTE",
            ),
            "--judge" => (
                "PROVIDER/MODEL",
                "Who judges: typesafe/jev-latest or any model the daemon serves; default AGENT_APPROVER_JUDGE, Jev when TYPESAFE_API_KEY is set, else AGENT_MODEL",
            ),
            "--judge-url" => (
                "URL",
                "Jev's base URL; default TYPESAFE_BASE_URL or https://api.typesafe.ai",
            ),
            "--reason" => ("TEXT", "Why, shown to the model with a denial"),
            "--approval-hold" => (
                "DURATION",
                "A new bot's gated calls wait this long for a verdict before the turn parks; default 2s",
            ),
            _ => unreachable!("flag missing help"),
        };
        println!(
            "  {:<28} {description}",
            format!("{flag} {value}").trim_end()
        );
    }
}

/// Flags that take a duration: the least and most each allows in
/// milliseconds, whether the daemon counts it in whole seconds, and the
/// range as a refusal says it.
const DURATIONS: &[(&str, u64, u64, bool, &str)] = &[
    ("--timeout", 0, 86_400_000, false, "up to 24h; 0 polls"),
    (
        "--approval-hold",
        0,
        3_600_000,
        false,
        "up to 1h; 0 parks at once",
    ),
    ("--grace", 0, 86_400_000, false, "up to 24h"),
    ("--stall-timeout", 1000, 86_400_000, true, "from 1s to 24h"),
    (
        "--idle-exit",
        0,
        u64::MAX,
        true,
        "in whole seconds; 0 disables",
    ),
    ("--keep-warm", 0, 299_000, true, "below 5m; 0 disables"),
];

/// A duration flag's value in milliseconds: digits and a unit (`ms`, `s`,
/// `m` or `h`), or a bare `0`. Any other bare number is refused, so no flag
/// guesses its unit.
pub fn duration_ms(flag: &str, value: &str) -> Result<u64> {
    let refuse = || {
        Error::with(
            "usage",
            format!("{flag} needs a duration such as 500ms, 30s, 5m or 1h"),
        )
    };
    if value == "0" {
        return Ok(0);
    }
    let unit = value
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(refuse)?;
    let scale = match &value[unit..] {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err(refuse()),
    };
    value[..unit]
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .ok_or_else(refuse)
}

/// A duration flag the daemon counts in whole seconds.
pub fn duration_secs(flag: &str, value: &str) -> Result<u64> {
    let ms = duration_ms(flag, value)?;
    if ms % 1000 != 0 {
        return fail_with("usage", format!("{flag} counts whole seconds"));
    }
    Ok(ms / 1000)
}

/// Normalize value flags once for both parsers; reject unused flags instead of
/// silently accepting them. None means help was printed successfully.
/// Whether `prepare` accepted `--pretty` as a flag, so a failure, even one
/// later in the same command line, is reported the way output was asked
/// for; a `--pretty` taken as another flag's missing value is not a request.
pub static PRETTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn prepare(args: Vec<String>) -> Result<Option<Vec<String>>> {
    let c = command(&args[0])?;
    let mut out = Vec::with_capacity(args.len());
    out.push(args[0].clone());
    let mut iter = args.into_iter().skip(1);
    let mut flags = Vec::new();
    let mut positional = 0;
    while let Some(arg) = iter.next() {
        if arg == "--" {
            if matches!(c.name, "run" | "wait") {
                out.push(arg);
            }
            for value in iter {
                positional += 1;
                out.push(value);
            }
            break;
        }
        if matches!(arg.as_str(), "--help" | "-h") {
            help(Some(c.name))?;
            return Ok(None);
        }
        if !arg.starts_with('-') || arg == "-" {
            positional += 1;
            out.push(arg);
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(f, v)| (f, Some(v)));
        let allowed = CONNECTION
            .split_whitespace()
            .chain(c.flags.split_whitespace())
            .chain(c.settings.split_whitespace())
            .chain(if c.startup { STARTUP } else { "" }.split_whitespace())
            .any(|f| f == flag);
        if !allowed {
            return fail_with("usage", format!("{} does not accept {flag}", c.name));
        }
        if flags.iter().any(|f| f == flag) && flag != "--provider" {
            return fail_with("usage", format!("{flag} may only be specified once"));
        }
        flags.push(flag.to_owned());
        out.push(flag.to_owned());
        if matches!(
            flag,
            "--pretty"
                | "--no-spawn"
                | "--new"
                | "--detach"
                | "--all"
                | "--any"
                | "--full"
                | "--no-compaction"
                | "--agents"
                | "--fallbacks"
                | "--discover"
        ) {
            if inline.is_some() {
                return fail_with("usage", format!("{flag} takes no value"));
            }
            if flag == "--pretty" {
                PRETTY.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        } else {
            let value = match inline {
                Some(v) => v.to_owned(),
                None => iter
                    .next()
                    .ok_or_else(|| Error::with("usage", format!("{flag} needs a value")))?,
            };
            // A flag-looking value must use '=' to be unambiguous.
            if inline.is_none() && value.starts_with("--") {
                return fail_with(
                    "usage",
                    format!("{flag} needs a value; use {flag}=VALUE for values starting with --"),
                );
            }
            if matches!(
                flag,
                "--context-bytes"
                    | "--context-items"
                    | "--keep-turns"
                    | "--max-output-tokens"
                    | "--budget-tokens"
                    | "--turn-budget-tokens"
                    | "--turn"
                    | "--checkpoint"
                    | "--request"
            ) {
                let max = match flag {
                    "--max-output-tokens" => u32::MAX as u64,
                    "--turn"
                    | "--checkpoint"
                    | "--budget-tokens"
                    | "--turn-budget-tokens"
                    | "--request" => i64::MAX as u64,
                    _ => usize::MAX as u64,
                };
                if !value.parse::<u64>().is_ok_and(|n| n > 0 && n <= max) {
                    return fail_with(
                        "usage",
                        format!("{flag} needs a positive integer up to {max}"),
                    );
                }
            }
            if let Some(&(_, least, most, whole, range)) = DURATIONS.iter().find(|d| d.0 == flag) {
                let ms = if whole {
                    duration_secs(flag, &value)? * 1000
                } else {
                    duration_ms(flag, &value)?
                };
                if !(least..=most).contains(&ms) {
                    return fail_with("usage", format!("{flag} needs a duration {range}"));
                }
            }
            if flag == "--after" && !value.parse::<i64>().is_ok_and(|n| n >= 0) {
                return fail_with("usage", "--after needs a nonnegative integer");
            }
            out.push(value);
        }
    }
    let has = |f: &str| flags.iter().any(|seen| seen == f);
    for (a, b) in [
        ("--all", "--bot"),
        ("--instructions", "--instructions-file"),
        ("--agents", "--instructions"),
        ("--agents", "--instructions-file"),
        ("--profile", "--instructions"),
        ("--profile", "--instructions-file"),
    ] {
        if has(a) && has(b) {
            return fail_with("usage", format!("{a} conflicts with {b}"));
        }
    }
    if c.name == "answer" {
        if positional != 1 {
            return fail_with("usage", "answer needs one decision: allow or deny");
        }
    } else if !matches!(c.name, "run" | "wait") && positional != 0 {
        return fail_with("usage", format!("{} takes no positional arguments", c.name));
    }
    if c.name == "run"
        && has("--bot")
        && !has("--new")
        && [
            "--instructions",
            "--instructions-file",
            "--agents",
            "--profile",
            "--budget-tokens",
            "--fallbacks",
            "--approval",
            "--approve",
        ]
        .iter()
        .any(|f| has(f))
    {
        return fail_with(
            "usage",
            "instructions, budget, and approval are creation options; use --new",
        );
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_name_their_unit() {
        let ms = |v: &str| duration_ms("--timeout", v).ok();
        for (value, millis) in [("0", 0), ("500ms", 500), ("30s", 30_000), ("5m", 300_000)] {
            assert_eq!(ms(value), Some(millis), "{value}");
        }
        assert_eq!(ms("1h"), Some(3_600_000));
        for refused in [
            "30",
            "s",
            "1.5s",
            "-1s",
            "5 m",
            "1d",
            "",
            "9999999999999999h",
        ] {
            assert_eq!(ms(refused), None, "{refused}");
        }
        assert_eq!(duration_secs("--keep-warm", "4m").ok(), Some(240));
        assert!(duration_secs("--keep-warm", "1500ms").is_err());
    }
}
