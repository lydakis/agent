//! Shared CLI grammar. Validation happens before touching the store or socket.
use agent_runtime::{Error, Result, fail_with};

const CONNECTION: &str = "--store --socket";
const STARTUP: &str = "--provider --max-processes --max-lasting --max-active --max-connecting --max-pending --max-pending-bytes --max-input-bytes --max-readers --max-load --max-depth --stall-timeout --idle-exit";

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
        flags: "--bot --new --created-by --fork --allow --expires --detach --delivery --attach-wait --turn --model --workspace --effort --turn-budget-tokens --budget-counts --image --quote --agents --profile --request-id --bot-id --pretty --no-spawn",
        settings: "--tools --instructions --instructions-file --budget-tokens --compaction-instructions --compaction-instructions-file --compaction-model --no-compaction --fallback --approval --approve --context-bytes --context-items --note-turns --compact-at --compact-keep --compact-tokens --max-rounds --max-drops --keep-turns --approval-hold --max-output-tokens --cache-ttl --speed --sandbox --writable --network --hide-senders --quiet-background --hold --unix-socket --outputs",
        startup: true,
    },
    Command {
        name: "follow",
        about: "Stream a bot's events, or every bot's with --all",
        usage: "follow (--bot NAME | --all) [--after CURSOR | --since DURATION]",
        flags: "--bot --all --after --since --pretty --no-spawn",
        settings: "",
        startup: false,
    },
    Command {
        name: "fork",
        about: "Copy a bot's conversation up to a message into a new bot",
        usage: "fork --source NAME --bot NAME [--checkpoint NODE] [--allow LIST] [--expires DURATION]",
        flags: "--source --bot --checkpoint --workspace --budget-tokens --budget-counts --allow --expires --request-id --pretty --no-spawn",
        settings: "",
        startup: false,
    },
    Command {
        name: "interrupt",
        about: "Stop a bot's running turn, or with --turn N withdraw message N still in line; prints its view",
        usage: "interrupt --bot NAME [--turn N]",

        flags: "--bot --turn --pretty --no-spawn",
        settings: "",
        startup: false,
    },
    Command {
        name: "ls",
        about: "List the bots",
        usage: "ls [--name GLOB] [--active] [--created-by BOT] [--limit N]",
        flags: "--name --active --created-by --limit --pretty --no-spawn",
        settings: "",
        startup: false,
    },
    Command {
        name: "inspect",
        about: "A bot's whole record: model, tools, gates, settings, instructions, tokens",
        usage: "inspect --bot NAME [--bot-id N]",
        flags: "--bot --bot-id --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "turns",
        about: "A bot's turns and how each ended",
        usage: "turns --bot NAME [--after TURN] [--newest] [--limit N]",
        flags: "--bot --after --newest --limit --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "messages",
        about: "A bot's messages, each by its number: where it stands and the turn that took it in",
        usage: "messages --bot NAME [--after N] [--newest] [--limit N]",
        flags: "--bot --after --newest --limit --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "usage",
        about: "Tokens the bots spent: in all, by bot, by model, and over time",
        usage: "usage [--name GLOB] [--since DURATION] [--bucket DURATION] [--limit N]",
        flags: "--name --since --bucket --limit --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "wait",
        about: "A turn's or command's result: status, text, usage; --timeout 0 polls",
        usage: "wait [--any] [--timeout DURATION] HANDLE... | wait --bot NAME [--bot-id N] [--timeout DURATION]",
        flags: "--bot --bot-id --any --timeout --pretty --no-spawn",
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
        name: "compact",
        about: "Summarize a bot's context before its next model call",
        usage: "compact --bot NAME [--bot-id N]",
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
        name: "audit",
        about: "Every tool call of every bot, newest first: tool, where it ran, who allowed it, how it ended",
        usage: "audit [--bot NAME] [--turn ID] [--kind KIND] [--place PLACE] [--approved WHO] [--access ACCESS] [--outcome OUTCOME] [--tool NAME] [--since DURATION] [--before ID] [--limit N] | audit trim (--before ID | --older-than DURATION)",
        flags: "--bot --turn --kind --place --approved --access --outcome --tool --since --before --older-than --limit --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "approvals",
        about: "Tool calls waiting for a verdict",
        usage: "approvals [--bot NAME] [--tag TAG] [--full] [--limit N]",
        flags: "--bot --tag --full --limit --pretty --no-spawn",
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
        name: "models",
        about: "The models the providers serve",
        usage: "models [--discover]",
        flags: "--discover --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "login",
        about: "Sign in with ChatGPT so the chatgpt provider spends your plan",
        usage: "login chatgpt [--port N]",
        flags: "--port --pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "companion",
        about: "Keep a command running for as long as the daemon runs, stop one, or list them",
        usage: "companion (add NAME [--cwd DIR] [--env KEY=VALUE]... -- COMMAND [ARGS]... | rm NAME | ls)",
        flags: "--cwd --env --pretty",
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
        about: "Stop the daemon; running turns park and the next start continues them",
        usage: "shutdown [--grace DURATION]",
        flags: "--grace --pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "pause",
        about: "Stop every agent until resume, across restarts: running turns park, queued work waits",
        usage: "pause --reason TEXT",
        flags: "--reason --pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "resume",
        about: "Lift a pause; parked turns continue, what waited starts",
        usage: "resume",
        flags: "--pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "pauses",
        about: "Every pause, held or lifted: why, who pulled it and let it go, whose turns it parked",
        usage: "pauses [--after ID] [--newest] [--limit N]",
        flags: "--after --newest --limit --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "held",
        about: "Turns held on a cause you can fix (a sign-in, a key, a quota, the config): each with its cause and next try",
        usage: "held [--bot NAME] [--after TURN] [--limit N]",
        flags: "--bot --after --limit --pretty --no-spawn",
        settings: "",
        startup: true,
    },
    Command {
        name: "retry",
        about: "Try held turns again now, every bot's or one's, once what held them is fixed",
        usage: "retry [--bot NAME]",
        flags: "--bot --pretty",
        settings: "",
        startup: false,
    },
    Command {
        name: "sandbox",
        about: "Run a command in a sandbox: read-only, writing only the folders named, or as one of a bot's commands",
        usage: "sandbox [--writable PATH]... [--bot NAME [--bot-id ID]] -- COMMAND [ARGS...]",
        flags: "--writable --bot --bot-id",
        settings: "",
        startup: false,
    },
    Command {
        name: "serve",
        about: "Run the daemon in the foreground; other commands start it for you",
        usage: "serve --store PATH --provider SPEC... [OPTIONS]",
        flags: "--shell-exclude --redact-field --redact-block --protect --protect-existing --protect-path --wall --own-copy --context-window --speeds --fallbacks --models --provider-config --await-owner",
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
        "\nValue flags accept --flag VALUE or --flag=VALUE; -- ends option parsing.\nJSON is the default; --pretty selects human output.\nExit status: 0 success, 1 failed/incomplete operation, 2 invalid usage; serve exits 75 when another daemon owns the store; sandbox exits with its command's status, or 125 for a failure of its own.\nProvider: NAME[=FAMILY[,BASE_URL[,KEY_ENV]]]; families: responses, responses-ws (Responses over WebSocket), anthropic."
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
                "Select a turn; with run --delivery steer, the turn to steer; with interrupt, the turn to end, also one still queued",
            ),
            "--new" => ("", "Create a new bot instead of continuing a named bot"),
            "--fork" => (
                "NAME",
                "Send the prompt to a fork of NAME made as --bot, leaving NAME as it is",
            ),
            "--expires" => (
                "DURATION",
                "Delete the fork once it has been idle this long, from 1s to 168h",
            ),
            "--detach" => (
                "",
                "Submit and return the message's JSON handle immediately",
            ),

            "--delivery" => (
                "MODE",
                "If the bot is busy: steer joins its running turn, queue runs after it, reject refuses; attach adds the message to its next model call without starting a turn; default AGENT_DELIVERY, else steer, or queue with --workspace or --turn-budget-tokens",
            ),
            "--attach-wait" => (
                "DURATION",
                "With --delivery attach: how long it waits for a call before it goes as a steer, up to 365d; 0 as long as it takes; default attach_wait in config.toml, else 1d",
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
            "--budget-counts" => (
                "tokens|work",
                "What the budgets given here count: tokens, the default, or work, what calls add beside the history they send again",
            ),
            "--image" => (
                "PATH",
                "Send this PNG, JPEG, GIF or WebP with the prompt; repeat for more",
            ),
            "--quote" => (
                "BOT/N",
                "Copy exactly message N, as its number shows it, after this prompt, with who wrote it; BOT/N:reply copies the reply of the turn that took it in; repeat for more",
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
                "Rewrite the model catalog ~/.agent/models.discovered from the providers' own listings, as each daemon start does",
            ),
            "--model" => (
                "PROVIDER/MODEL",
                "Set the model; new bots default to AGENT_MODEL, existing bots keep theirs",
            ),
            "--tools" => (
                "LIST",
                "A new bot's tools; default shell,write_stdin,read,write,wait,history, apply_patch for a Responses model else edit, web_fetch, and web_search where the provider offers it",
            ),
            "--allow" => (
                "LIST",
                "The tools a fork may call, within its source's; '' allows none; default: the source's",
            ),
            "--max-processes" => ("N", "Concurrent process limit; 0 is unbounded"),
            "--max-lasting" => (
                "N",
                "Terminals and detached commands still running; 0 is unbounded",
            ),
            "--max-active" => ("N", "Concurrent active turn limit; 0 is unbounded"),
            "--max-connecting" => ("N", "Concurrent provider startup limit; 0 is unbounded"),
            "--max-pending" => ("N", "Submissions waiting to start; 0 is unbounded"),
            "--max-pending-bytes" => ("N", "Prompt bytes waiting to start; 0 is unbounded"),
            "--max-input-bytes" => (
                "N",
                "Request bytes read ahead of the service; default memory/16; 0 is unbounded",
            ),
            "--max-readers" => (
                "N",
                "Most store reader connections, opened as reads wait; default one per CPU",
            ),
            "--max-load" => (
                "LOAD",
                "New commands wait while the 1-minute load average is this or more and one is running; 0 is no gate",
            ),
            "--max-depth" => (
                "N",
                "Most generations a new bot may be below the bot that starts its line; 0 is unbounded",
            ),
            "--max-output-tokens" => (
                "N",
                "A new bot's output token cap per model call; default the model's limit",
            ),
            "--stall-timeout" => (
                "DURATION",
                "Retry a provider stream with no content this long; default 10m",
            ),
            "--cache-ttl" => (
                "5m|1h",
                "A new bot's Anthropic prompt-cache lifetime; 1h bills writes at 2x input; default 5m",
            ),
            "--speed" => (
                "standard|fast|ultrafast",
                "A new bot's speed tier, sent to each model that serves it, at a higher price per token; standard on the rest; default standard",
            ),
            "--idle-exit" => ("DURATION", "Exit after this long idle; 0 disables"),
            "--await-owner" => (
                "DURATION",
                "Wait up to this long for the daemon holding the store and socket to exit, then take them, rather than refusing at once; clients replacing an older daemon pass it, so the new one starts as the old drains and no other start can take the store in between",
            ),
            "--shell-exclude" => (
                "PATTERN",
                "Leave variables named like this (* and ? wildcards, any case) out of tool shells; repeatable; clients pass [shell_environment_policy] from ~/.agent/config.toml",
            ),
            "--redact-field" => (
                "NAME",
                "Redact the value printed after a field named like this (its end, any case, _ and - ignored) from tool output; repeatable",
            ),
            "--redact-block" => (
                "LABEL",
                "Redact the body of each PEM block whose header names LABEL, such as PRIVATE KEY, from tool output; repeatable",
            ),
            "--protect-path" => (
                "PATH",
                "Keep PATH, by its absolute path, read-only in every sandbox, and from being made when missing; repeatable",
            ),
            "--context-window" => (
                "PATTERN=TOKENS",
                "The context window of each model whose PROVIDER/MODEL path matches PATTERN (* and ? wildcards), over its provider's listing; the most specific pattern wins; repeatable; clients pass [context_windows] from ~/.agent/config.toml",
            ),
            "--speeds" => (
                "PATTERN=TIERS",
                "The tiers faster than standard (fast, ultrafast, comma-separated, or none) each model whose PROVIDER/MODEL path matches PATTERN (* and ? wildcards) serves, over the built-in list; a bot's speed reaches only models that serve it, others run at standard; the most specific pattern wins; repeatable; clients pass [speeds] from ~/.agent/config.toml",
            ),
            "--fallbacks" => (
                "PATTERN=PROVIDER/MODEL",
                "The model a request a safety classifier declined goes to, that request alone, for each bot that names none of its own whose turn's PROVIDER/MODEL path matches PATTERN (* and ? wildcards); the most specific pattern wins; repeatable; clients pass [fallbacks] from ~/.agent/config.toml",
            ),
            "--provider-config" => (
                "PATH",
                "The config file the providers came from (its `provider`), watched: an edit binds the new set for turns that start after it, running turns finish on the old, and a set that fails to bind or drops a provider agents use is refused; clients pass ~/.agent/config.toml when no --provider or AGENT_PROVIDER names them",
            ),
            "--models" => (
                "PATH",
                "Rewrite the model catalog at PATH from the providers' listings each time the daemon starts, after it is ready; clients pass ~/.agent/models.discovered",
            ),
            "--wall" => (
                "FOLDER",
                "Keep every sandboxed command, the network on or not, from the Unix sockets in FOLDER, made private if missing; repeatable",
            ),
            "--own-copy" => (
                "FOLDER",
                "On Linux, show the system folder FOLDER in every sandbox from a read-only copy the person owns, kept in step with it, for files a program takes only from root or the person; repeatable",
            ),
            "--protect" => (
                "NAME",
                "Keep NAME at the top of each sandboxed bot's writable folders read-only, and from being made when missing; repeatable",
            ),
            "--protect-existing" => (
                "NAME",
                "Keep NAME at the top of each sandboxed bot's writable folders read-only where it exists; repeatable",
            ),
            "--port" => (
                "N",
                "Wait for the browser's sign-in on 127.0.0.1:N; default 1455",
            ),
            "--context-bytes" => ("N", "A new bot's model context bytes; default 8 MiB"),
            "--context-items" => ("N", "A new bot's model context items; default 4096"),
            "--max-rounds" => (
                "N",
                "Model calls each of a new bot's turns may make, the last with no tool call; default unbounded",
            ),
            "--max-drops" => (
                "N",
                "Replies in a row the model's output cap may cut short in a new bot's turn, the last ending it; default 3",
            ),
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
            "--compact-tokens" => (
                "N",
                "A new bot elides, then compacts, once a call's input reaches N tokens, whichever of this and --compact-at comes first; 0 leaves --compact-at; by default its model's context window less 33000 tokens, at most 90% of it",
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
            "--sandbox" => (
                "MODE",
                "A new bot's command sandbox: read-only, workspace-write or danger-full-access; default none",
            ),
            "--cwd" => ("DIR", "The folder a companion runs in"),
            "--env" => ("KEY=VALUE", "A variable a companion gets; repeat for more"),
            "--writable" => (
                "PATH",
                "Another folder a workspace-write bot, or a sandboxed command, may change; repeat for more",
            ),
            "--network" => ("", "A workspace-write bot's commands may use the network"),
            "--outputs" => (
                "PATH",
                "Where a new bot's tool outputs cut for the model are written whole, one file per stream; default the outputs folder beside the store",
            ),
            "--hide-senders" => (
                "",
                "A new bot's model reads what other bots and clients send it with no line naming the sender",
            ),
            "--quiet-background" => (
                "",
                "A new bot is not told when a background command no wait collected ends",
            ),
            "--hold" => (
                "DURATION",
                "How long a new bot's turn waits on a cause a person can fix (a sign-in, a usage limit, a provider down) before it fails; up to 365d, 0 fails at once; default hold in config.toml, else 5d",
            ),
            "--unix-socket" => (
                "PATH",
                "A Unix socket, or a folder of them, the only ones a sandboxed bot's commands reach; repeat for more",
            ),
            "--fallback" => (
                "PROVIDER/MODEL",
                "A new bot's model for a request a safety classifier declined, that request alone, over the daemon's [fallbacks]; default none",
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
                "Tools whose calls need a verdict; default every tool but history, wait, note, echo, web_search",
            ),
            "--call" => ("ID", "The tool call to answer"),
            "--full" => ("", "Every call's whole arguments, not their preview"),
            "--name" => (
                "GLOB",
                "Only bots whose name matches, as SQLite GLOB: * any run, ? one byte, [..] a set",
            ),
            "--active" => ("", "Only bots with a turn running"),
            "--created-by" => (
                "BOT",
                "Only bots this bot made; with run --new, the bot whose new bot this is, for a client starting one for its agent",
            ),
            "--newest" => ("", "Newest first, read from the end"),
            "--limit" => ("N", "List at most N"),
            "--since" => (
                "DURATION",
                "From this long ago: for usage, only turns started since (default: all); for follow, events written since",
            ),
            "--bucket" => (
                "DURATION",
                "Add a series of what was spent every DURATION from --since",
            ),
            "--request" => ("N", "The request number the call was announced with"),
            "--kind" => (
                "KIND",
                "Only calls of this kind of tool: shell, file, mcp, browser, desktop, agents or web",
            ),
            "--place" => (
                "PLACE",
                "Only calls that ran here: sandbox, outside (this machine, outside a sandbox) or external (a service elsewhere)",
            ),
            "--approved" => (
                "WHO",
                "Only calls decided by: nobody, person, reviewer or rule",
            ),
            "--access" => (
                "ACCESS",
                "Only calls that read, write, or are unknown (a command, or a tool that says nothing)",
            ),
            "--outcome" => (
                "OUTCOME",
                "Only calls that ended so: completed, failed, denied or unknown",
            ),
            "--tool" => ("NAME", "Only calls of this tool, or steps of this name"),
            "--before" => (
                "ID",
                "Rows older than this one; for audit trim, delete the rows older than it",
            ),
            "--older-than" => (
                "DURATION",
                "audit trim: delete the rows written longer ago than this",
            ),
            "--tag" => (
                "TAG",
                "The gate to list or answer; default: the call's only gate, or auto",
            ),
            "--reason" => (
                "TEXT",
                "Why: shown to the model with a denial, or to everyone with a pause",
            ),
            "--approval-hold" => (
                "DURATION",
                "A new bot's gated calls wait this long for a verdict, and its terminal reads for output, before the turn parks; default 2s",
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
    ("--expires", 1000, 604_800_000, false, "from 1s to 168h"),
    ("--stall-timeout", 1000, 86_400_000, true, "from 1s to 24h"),
    (
        "--idle-exit",
        0,
        u64::MAX,
        true,
        "in whole seconds; 0 disables",
    ),
    (
        "--await-owner",
        0,
        86_400_000,
        true,
        "in whole seconds, up to 24h",
    ),
];

/// A duration flag's value in milliseconds: digits and a unit (`ms`, `s`,
/// `m` or `h`), or a bare `0`. Any other bare number is refused, so no flag
/// guesses its unit.
pub fn duration_ms(flag: &str, value: &str) -> Result<u64> {
    let refuse = || {
        Error::with(
            "usage",
            format!("{flag} needs a duration such as 500ms, 30s, 5m, 1h or 7d"),
        )
    };
    agent_client::config::duration_ms(value).ok_or_else(refuse)
}

/// `--max-load`: a load average, 0 or more, as uptime prints one.
pub fn load(value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|load| load.is_finite() && *load >= 0.0)
        .ok_or_else(|| Error::with("usage", "--max-load needs a load average, 0 or more"))
}

/// A duration flag the daemon counts in whole seconds.
pub fn duration_secs(flag: &str, value: &str) -> Result<u64> {
    let ms = duration_ms(flag, value)?;
    if ms % 1000 != 0 {
        return fail_with("usage", format!("{flag} counts whole seconds"));
    }
    Ok(ms / 1000)
}

/// Whether `prepare` accepted `--pretty` as a flag, so a failure, even one
/// later in the same command line, is reported the way output was asked
/// for; a `--pretty` taken as another flag's missing value is not a request.
pub static PRETTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
            if matches!(c.name, "run" | "wait" | "companion" | "sandbox") {
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
        if flags.iter().any(|f| f == flag)
            && !matches!(
                flag,
                "--provider"
                    | "--image"
                    | "--quote"
                    | "--writable"
                    | "--unix-socket"
                    | "--env"
                    | "--shell-exclude"
                    | "--redact-field"
                    | "--redact-block"
                    | "--protect"
                    | "--protect-existing"
                    | "--protect-path"
                    | "--wall"
                    | "--own-copy"
                    | "--context-window"
                    | "--speeds"
                    | "--fallbacks"
            )
        {
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
                | "--active"
                | "--newest"
                | "--no-compaction"
                | "--agents"
                | "--network"
                | "--hide-senders"
                | "--quiet-background"
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
                    | "--limit"
                    | "--max-output-tokens"
                    | "--budget-tokens"
                    | "--turn-budget-tokens"
                    | "--turn"
                    | "--checkpoint"
                    | "--request"
                    | "--before"
            ) {
                let max = match flag {
                    "--max-output-tokens" => u32::MAX as u64,
                    "--turn"
                    | "--checkpoint"
                    | "--budget-tokens"
                    | "--turn-budget-tokens"
                    | "--request"
                    | "--before" => i64::MAX as u64,
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
        ("--after", "--since"),
        ("--fork", "--new"),
        ("--fork", "--turn"),
        ("--fork", "--bot-id"),
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
    } else if c.name == "login" {
        if positional != 1 {
            return fail_with("usage", "login needs a provider: chatgpt");
        }
    } else if c.name == "sandbox" {
        if positional == 0 {
            return fail_with("usage", "sandbox needs a command after --");
        }
    } else if c.name == "audit" && positional != 0 {
        // `audit trim` deletes what it names and nothing else.
        if positional != 1 {
            return fail_with("usage", "audit takes at most one word: trim");
        }
        if has("--before") == has("--older-than") {
            return fail_with(
                "usage",
                "audit trim needs one of --before ID or --older-than DURATION",
            );
        }
        if let Some(flag) = c.flags.split_whitespace().find(|f| {
            !matches!(*f, "--before" | "--older-than" | "--pretty" | "--no-spawn") && has(f)
        }) {
            return fail_with("usage", format!("audit trim does not take {flag}"));
        }
    } else if !matches!(c.name, "run" | "wait" | "companion" | "audit") && positional != 0 {
        return fail_with("usage", format!("{} takes no positional arguments", c.name));
    }
    if has("--budget-counts") && !has("--budget-tokens") && !has("--turn-budget-tokens") {
        return fail_with(
            "usage",
            "--budget-counts names what a budget counts; give --budget-tokens or --turn-budget-tokens",
        );
    }
    if c.name == "run" && has("--fork") {
        if !has("--bot") {
            return fail_with("usage", "run --fork needs --bot to name the copy");
        }
        // A fork keeps all its source chose but a budget of its own.
        if let Some(flag) = ["--agents", "--profile"]
            .into_iter()
            .chain(c.settings.split_whitespace())
            .find(|f| *f != "--budget-tokens" && has(f))
        {
            return fail_with(
                "usage",
                format!("a fork keeps its source's choices; {flag} is for a new bot"),
            );
        }
    } else if c.name == "run"
        && let Some(flag) = ["--allow", "--expires"].into_iter().find(|f| has(f))
    {
        return fail_with("usage", format!("{flag} is for a copy made with --fork"));
    }
    if c.name == "run"
        && has("--bot")
        && !has("--new")
        && !has("--fork")
        && [
            "--instructions",
            "--instructions-file",
            "--agents",
            "--profile",
            "--budget-tokens",
            "--fallback",
            "--sandbox",
            "--writable",
            "--network",
            "--hide-senders",
            "--quiet-background",
            "--hold",
            "--unix-socket",
            "--approval",
            "--approve",
            "--no-compaction",
            "--compaction-model",
            "--compaction-instructions",
            "--compaction-instructions-file",
        ]
        .iter()
        .any(|f| has(f))
    {
        return fail_with(
            "usage",
            "instructions, budget, approval, sandbox, senders and compaction are creation options; use --new",
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
        assert_eq!(ms("7d"), Some(604_800_000));
        for refused in [
            "30",
            "s",
            "1.5s",
            "-1s",
            "5 m",
            "1w",
            "",
            "9999999999999999h",
        ] {
            assert_eq!(ms(refused), None, "{refused}");
        }
        assert_eq!(duration_secs("--idle-exit", "4m").ok(), Some(240));
        assert!(duration_secs("--idle-exit", "1500ms").is_err());
    }

    #[test]
    fn a_sandboxed_command_keeps_its_words_after_the_separator() {
        let words = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let prepared = prepare(words(&[
            "sandbox",
            "--writable",
            "/r",
            "--",
            "git",
            "--no-pager",
            "status",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            prepared,
            words(&[
                "sandbox",
                "--writable",
                "/r",
                "--",
                "git",
                "--no-pager",
                "status"
            ])
        );
        // Each folder named; no command is refused, as is another flag.
        assert!(
            prepare(words(&[
                "sandbox",
                "--writable",
                "/a",
                "--writable",
                "/b",
                "--",
                "ls"
            ]))
            .is_ok()
        );
        assert!(prepare(words(&["sandbox", "--writable", "/a"])).is_err());
        assert!(prepare(words(&["sandbox", "--"])).is_err());
        assert!(prepare(words(&["sandbox", "--bot", "x", "--", "ls"])).is_ok());
        assert!(
            prepare(words(&[
                "sandbox", "--bot", "x", "--bot-id", "3", "--", "ls"
            ]))
            .is_ok()
        );
        assert!(prepare(words(&["sandbox", "--model", "m", "--", "ls"])).is_err());
        // With the network off, it reaches no socket, the daemon's least.
        assert!(prepare(words(&["sandbox", "--network", "--", "ls"])).is_err());
    }
}
