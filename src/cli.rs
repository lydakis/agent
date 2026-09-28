//! Shared CLI grammar. Validation happens before touching the store or socket.
use agent_runtime::{Error, Result, fail_with};

const CONNECTION: &str = "--store --socket";
const STARTUP: &str = "--provider --max-processes --max-detached --max-active --max-connecting --max-pending --max-pending-bytes --stall-timeout --idle-exit";

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
        flags: "--bot --new --detach --delivery --turn --model --tools --workspace --instructions --instructions-file --reasoning --request-id --bot-id --budget-tokens --compaction-instructions --compaction-instructions-file --compaction-model --no-compaction --fallbacks --agents --profile --approval --approve --context-bytes --context-items --note-turns --compact-at --compact-keep --retain-turns --approval-hold-ms --max-output-tokens --keep-warm --cache-ttl --pretty --no-spawn",
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
        usage: "fork --source NAME --bot NAME [--checkpoint NODE] [--allow LIST]",
        flags: "--source --bot --checkpoint --workspace --budget-tokens --approval --approve --allow --pretty",
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
        name: "approvals",
        usage: "approvals [--bot NAME] [--tag TAG]",
        flags: "--bot --tag --pretty --no-spawn",
        startup: true,
    },
    Command {
        name: "answer",
        usage: "answer --bot NAME --turn TURN --call ID --request N [--tag TAG] [--reason TEXT] allow|deny",
        flags: "--bot --turn --call --request --tag --reason --no-spawn",
        startup: true,
    },
    Command {
        name: "approver",
        usage: "approver [--tag TAG] [--judge PROVIDER/MODEL] [--reasoning LEVEL] [--note FILE] [--judge-url URL]",
        flags: "--tag --judge --reasoning --note --judge-url",
        startup: false,
    },
    Command {
        name: "models",
        usage: "models [--discover]",
        flags: "--discover --pretty --no-spawn",
        startup: true,
    },
    Command {
        name: "start",
        usage: "start [OPTIONS]",
        flags: "--pretty",
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
        usage: "shutdown [--grace SECONDS]",
        flags: "--grace",
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
        "\nValue flags accept --flag VALUE or --flag=VALUE; -- ends option parsing.\nJSON is the default; --pretty selects human output where supported.\nExit status: 0 success, 1 failed/incomplete operation, 2 invalid usage.\nProvider: NAME[=FAMILY[,BASE_URL[,KEY_ENV]]]; families: responses, responses-ws (Responses over WebSocket), anthropic."
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
            "--timeout-ms" => ("N", "Wait at most N milliseconds; 0 polls immediately"),
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
            "--reasoning" => ("LEVEL", "low, medium, high, xhigh, or max"),
            "--request-id" => ("ID", "Idempotency key for this submission"),
            "--bot-id" => ("N", "Refuse if --bot no longer names this identity"),
            "--budget-tokens" => ("N", "New bot's lifetime input + output token cap"),
            "--pretty" => ("", "Render human-readable output"),
            "--no-spawn" => ("", "Require an already running daemon"),
            "--keep-turns" => ("N", "Keep the latest N turns' operational records"),
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
                "SECONDS",
                "Retry a provider stream with no content this long; default 120",
            ),
            "--keep-warm" => (
                "SECONDS",
                "A new bot refreshes its idle Anthropic prompt cache during a tool call after this long; default 240, 0 disables",
            ),
            "--cache-ttl" => (
                "5m|1h",
                "A new bot's Anthropic prompt-cache lifetime; 1h bills writes at 2x input and sends no refresh; default 5m",
            ),
            "--idle-exit" => ("SECONDS", "Exit after idle time; 0 disables"),
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
            "--retain-turns" => (
                "N",
                "A new bot keeps N turns' operational records, pruning older ones as its turns finish",
            ),
            "--grace" => (
                "SECONDS",
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
            "--approval-hold-ms" => (
                "N",
                "A new bot's gated calls wait this long for a verdict before the turn parks; default 2000",
            ),
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
                | "--fallbacks"
                | "--discover"
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
                    | "--request"
            ) {
                let max = match flag {
                    "--max-output-tokens" => u32::MAX as u64,
                    "--stall-timeout" => 86_400,
                    "--turn" | "--checkpoint" | "--budget-tokens" | "--request" => i64::MAX as u64,
                    _ => usize::MAX as u64,
                };
                if !value.parse::<u64>().is_ok_and(|n| n > 0 && n <= max) {
                    return fail_with(
                        "usage",
                        format!("{flag} needs a positive integer up to {max}"),
                    );
                }
            }
            if flag == "--approval-hold-ms" && !value.parse::<u64>().is_ok_and(|n| n <= 3_600_000) {
                return fail_with(
                    "usage",
                    "--approval-hold-ms needs milliseconds up to 3600000 (0 parks at once)",
                );
            }
            if flag == "--keep-warm" && !value.parse::<u64>().is_ok_and(|n| n < 300) {
                return fail_with("usage", "--keep-warm needs seconds below 300 (0 disables)");
            }
            if flag == "--grace" && !value.parse::<u64>().is_ok_and(|n| n <= 86_400) {
                return fail_with("usage", "--grace needs seconds up to 86400");
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
            "--reasoning",
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
            "instructions, reasoning, budget, and approval are creation options; use --new",
        );
    }
    Ok(Some(out))
}
