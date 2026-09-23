//! Shared CLI grammar. Validation happens before touching the store or socket.
use agent_runtime::{Error, Result, fail_with};

const CONNECTION: &str = "--store --socket";
const STARTUP: &str = "--provider --max-processes --max-active --max-connecting --max-pending --max-pending-bytes --max-output-tokens --stall-timeout --idle-exit --context-bytes --context-items --note-turns --compact-at --compact-keep --retain-turns";

struct Command {
    name: &'static str,
    usage: &'static str,
    flags: &'static str,
    startup: bool,
}

const COMMANDS: &[Command] = &[
    Command {
        name: "run",
        usage: "run [OPTIONS] [--] PROMPT...",
        flags: "--bot --new --detach --delivery --turn --model --tools --workspace --instructions --instructions-file --reasoning --request-id --bot-id --budget-tokens --compaction-instructions --compaction-instructions-file --compaction-model --no-compaction --agents --pretty --no-spawn",
        startup: true,
    },
    Command {
        name: "follow",
        usage: "follow (--bot NAME | --all) [--after CURSOR]",
        flags: "--bot --all --after --pretty",
        startup: false,
    },
    Command {
        name: "fork",
        usage: "fork --source NAME --bot NAME [--checkpoint NODE] [--instructions TEXT]",
        flags: "--source --bot --checkpoint --workspace --budget-tokens --instructions --instructions-file --agents --pretty",
        startup: false,
    },
    Command {
        name: "interrupt",
        usage: "interrupt --bot NAME",
        flags: "--bot",
        startup: false,
    },
    Command {
        name: "ls",
        usage: "ls [OPTIONS]",
        flags: "--pretty",
        startup: false,
    },
    Command {
        name: "turns",
        usage: "turns --bot NAME [--after TURN]",
        flags: "--bot --after --pretty --no-spawn",
        startup: true,
    },
    Command {
        name: "result",
        usage: "result --bot NAME --turn TURN",
        flags: "--bot --turn --pretty --no-spawn",
        startup: true,
    },
    Command {
        name: "wait",
        usage: "wait [--any] [--timeout-ms N] HANDLE...",
        flags: "--any --timeout-ms --pretty",
        startup: false,
    },
    Command {
        name: "rm",
        usage: "rm --bot NAME",
        flags: "--bot --pretty --no-spawn",
        startup: true,
    },
    Command {
        name: "prune",
        usage: "prune --bot NAME --keep-turns N",
        flags: "--bot --keep-turns --pretty --no-spawn",
        startup: true,
    },
    Command {
        name: "stats",
        usage: "stats [OPTIONS]",
        flags: "--pretty --no-spawn",
        startup: true,
    },
    Command {
        name: "shutdown",
        usage: "shutdown [OPTIONS]",
        flags: "",
        startup: false,
    },
    Command {
        name: "serve",
        usage: "serve --store PATH --provider SPEC... [OPTIONS]",
        flags: "",
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
        if c.startup {
            println!("\nDaemon options (client commands apply these on startup):");
            print_flags(STARTUP);
        }
    } else {
        println!("Usage: agent COMMAND [OPTIONS]\n");
        for c in COMMANDS {
            println!("  {}", c.usage);
        }
        println!(
            "\nUse agent COMMAND --help for its flags.\n  -h, --help     Show help\n  --version      Show version"
        );
    }
    println!(
        "\nValue flags accept --flag VALUE or --flag=VALUE; -- ends option parsing.\nJSON is the default; --pretty selects human output where supported.\nExit status: 0 success, 1 failed/incomplete operation, 2 invalid usage.\nProvider: NAME[=FAMILY[,BASE_URL[,KEY_ENV]]]; families: responses, anthropic."
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
            "--checkpoint" => ("NODE", "Fork at this history node; default: current head"),
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
            "--timeout-ms" => ("N", "Wait at most N milliseconds; 0 polls immediately"),
            "--workspace" => ("DIR", "Select the working directory"),
            "--instructions" => (
                "TEXT",
                "A new bot's instructions (default: the built-in text); for fork, replace the source's",
            ),
            "--instructions-file" => ("FILE", "Read the instructions from a file"),
            "--agents" => (
                "",
                "Compose instructions: the preamble, AGENTS.md files from the workspace up, and skills",
            ),
            "--reasoning" => ("LEVEL", "low, medium, high, xhigh, or max"),
            "--request-id" => ("ID", "Idempotency key for this submission"),
            "--bot-id" => ("N", "Refuse if --bot no longer names this identity"),
            "--budget-tokens" => ("N", "New bot's lifetime input + output token cap"),
            "--pretty" => ("", "Render human-readable output"),
            "--no-spawn" => ("", "Require an already running daemon"),
            "--keep-turns" => ("N", "Keep the latest N turns' operational records"),
            "--provider" => ("SPEC", "Register a provider; repeat for multiple providers"),
            "--model" => (
                "PROVIDER/MODEL",
                "Set the model; new bots default to AGENT_MODEL, existing bots keep theirs",
            ),
            "--tools" => (
                "LIST",
                "A new bot's tools; default shell,read,write,edit,wait,history",
            ),
            "--max-processes" => ("N", "Concurrent process limit; 0 is unbounded"),
            "--max-active" => ("N", "Concurrent active turn limit; 0 is unbounded"),
            "--max-connecting" => ("N", "Concurrent provider startup limit; 0 is unbounded"),
            "--max-pending" => ("N", "Submissions waiting to start; 0 is unbounded"),
            "--max-pending-bytes" => ("N", "Prompt bytes waiting to start; 0 is unbounded"),
            "--max-output-tokens" => ("N", "Output token cap per model call"),
            "--stall-timeout" => (
                "SECONDS",
                "Retry a provider stream with no content this long; default 120",
            ),
            "--idle-exit" => ("SECONDS", "Exit after idle time; 0 disables"),
            "--context-bytes" => ("N", "Maximum model context bytes"),
            "--context-items" => ("N", "Maximum model context items"),
            "--note-turns" => ("N", "Omitted turns the context note lists; 0 lists none"),
            "--compact-at" => (
                "PERCENT",
                "Compact when the window holds this share of the context budget",
            ),
            "--compact-keep" => (
                "PERCENT",
                "Share of the context budget kept verbatim at compaction",
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
            "--retain-turns" => ("N", "Automatically retain N turns' operational records"),
            _ => unreachable!("flag missing help"),
        };
        println!(
            "  {:<28} {description}",
            format!("{flag} {value}").trim_end()
        );
    }
}

/// Normalize value flags once for both parsers; reject unused flags instead of
/// silently accepting them. None means help was printed successfully.
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
                | "--no-compaction"
                | "--agents"
        ) {
            if inline.is_some() {
                return fail_with("usage", format!("{flag} takes no value"));
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
                    | "--retain-turns"
                    | "--keep-turns"
                    | "--max-output-tokens"
                    | "--stall-timeout"
                    | "--budget-tokens"
                    | "--turn"
                    | "--checkpoint"
            ) {
                let max = match flag {
                    "--max-output-tokens" => u32::MAX as u64,
                    "--stall-timeout" => 86_400,
                    "--turn" | "--checkpoint" | "--budget-tokens" => i64::MAX as u64,
                    _ => usize::MAX as u64,
                };
                if !value.parse::<u64>().is_ok_and(|n| n > 0 && n <= max) {
                    return fail_with(
                        "usage",
                        format!("{flag} needs a positive integer up to {max}"),
                    );
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
    ] {
        if has(a) && has(b) {
            return fail_with("usage", format!("{a} conflicts with {b}"));
        }
    }
    if !matches!(c.name, "run" | "wait") && positional != 0 {
        return fail_with("usage", format!("{} takes no positional arguments", c.name));
    }
    if c.name == "run"
        && has("--bot")
        && !has("--new")
        && [
            "--instructions",
            "--instructions-file",
            "--agents",
            "--reasoning",
            "--budget-tokens",
        ]
        .iter()
        .any(|f| has(f))
    {
        return fail_with(
            "usage",
            "instructions, reasoning, and budget are creation options; use --new",
        );
    }
    Ok(Some(out))
}
