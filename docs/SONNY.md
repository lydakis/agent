# Sonny's capabilities

Sonny extends [Agent](../README.md) with background capabilities that compound across sessions, projects, and machines: OS-scheduled **automations**, shared **memory**, cross-project **commons & boards**, **skills**, **code-mode MCP (`exec`)**, multi-agent **workflows**, and SSH **links** to another machine's browser, desktop, or MCP servers.

For installation and everyday terminal controls, start with the [Sonny quickstart](../sonny/README.md).

[Automations](#automations) · [Scheduler](#the-scheduler) · [Memory](#memory) · [Commons](#commons) · [Boards](#boards) · [Pause](#pause) · [Upgrading](#upgrading) · [Skills](#skills) · [Tool host](#tool-host) · [Workflows](#workflows) · [MCP & code mode](#mcp-servers) · [Links](#links) · [Responsive UI](#keys-never-wait-on-the-daemon)

## Automations

Automations wake an agent with a message on a schedule, file change, git commit, or manual trigger—running through a single OS scheduler even when Sonny is closed. While idle (or skipped by an `--if` gate), an automation costs zero model calls.

*(Note: Leads hear task updates from the [Lookout](LOOKOUT.md), not automations. Use automations for scheduled jobs and file/git triggers.)*

### Adding and managing automations

Sonny's agents manage their own automations via the `automations` tool (`add`, `remove`, `run`, `read NAME`, or `{}` to list). People, scripts, and other harnesses use `~/.agent/automation` (written by Sonny as a wrapper around `sonny automation`):

```sh
~/.agent/automation add [--name NAME] [WHEN] \
  [--agent NAME | --start NAME --model PROVIDER/MODEL [--effort LEVEL] [--fresh]] \
  [--reply-to AGENT] [--if CMD] [--runs N] [--until 'YYYY-MM-DD HH:MM'] \
  [--turn-budget-tokens N [--budget-counts work]] [--shadow] [--live] -- MESSAGE

~/.agent/automation ls [--after NAME]
~/.agent/automation show NAME
~/.agent/automation run NAME
~/.agent/automation rm NAME
```

Every CLI reply is one JSON value on stdout; failures print `{"error": CODE, "detail": ...}` on stderr with exit `1`. View, run (`r`), or remove (`x`) automations interactively in Sonny with `/background`.

### Trigger types (`WHEN`)

| Flag | Behavior |
| --- | --- |
| `--every 30m` | Repeating interval (minutes dividing an hour, hours dividing a day, or `1d`); waits one full interval before firing |
| `--in 45m` / `--at 'YYYY-MM-DD HH:MM'` | One-off within a year (local time); removes itself after running |
| `--cron 'MIN HOUR DAY MONTH WEEKDAY'` | Standard 5-field cron expression (up to 1,024 calendar entries) |
| `--file PATH` | Fires when `PATH` is written (or files are added/removed in a directory); polled every 2s. Refuses daemon store paths and `~/.agent/automations` |
| `--commit REPO` | Fires when `HEAD` reaches a newly committed revision (`git rev-list --since=...`); ignores checkouts/resets/fast-forwards to old commits |
| *(omitted)* | `on demand` — fires only via `run NAME` |

### Target agents and execution options

- **Existing agent (`--agent NAME`):** Defaults to the calling shell's `AGENT_BOT`. Pinned by bot ID so a deleted/recreated bot is never messaged accidentally (`bot_not_found`).
- **Start on demand (`--start NAME --model PROVIDER/MODEL [--effort LEVEL]`):** Creates the agent on first run using Sonny's standard workspace policy, `auto` sandbox/reviewer, and tools (`memory`, `exec`). Add `--fresh` to create a fresh `NAME-N` agent per run (keeping at most the running, latest finished, and latest failed agents, and noting prior failures on line 1).
- **Pre-flight gate (`--if CMD`):** Runs `sh -c CMD` (60s timeout) before waking any agent. Exit `0` runs, exit `1` skips silently (zero model calls), any other exit or timeout fails the run and alerts the owner once on state change. Commands added by an agent run inside that agent's own sandbox (`agent sandbox --bot`); commands added by you run as your user.
- **Forward replies (`--reply-to BOT`):** Waits for the turn to finish (up to 24h) and steers its answer to `BOT`, skipping overlapping ticks while waiting.
- **Stop conditions & budgets:** `--runs N` stops after `N` deliveries; `--until 'YYYY-MM-DD HH:MM'` ends at local time; `--turn-budget-tokens N` (optionally with `--budget-counts work` to ignore re-sent history) caps spend per run.
- **Shadow dry runs (`--shadow`):** Forks the target into a read-only `BOT.shadow-HEX` copy with no shell, no human/reviewer approval gate, and read-only MCP calls only; sends the dry-run answer to `--reply-to` (or the creator) and deletes the copy. Incompatible with `--fresh`.
- **Live message templates (`--live`):** Evaluates Jinja blanks (`source('NAME')`, `file('PATH')`, `modified('PATH')`, `now()`, `| ago`, `| time`; up to 32 KiB) right as each run starts so the prompt arrives with fresh data already filled in.

### Permissions, delivery, and state

- **Stepwise approvals:** In the `automations` tool, reads need no approval. Adding, removing another's, or running an automation is approved per capability (`if` as `shell`, waking/replying to another bot as `send_message`, `start` as `start_task`).
- **Queued delivery & coalescing:** Runs submit with `delivery: queue` and `origin: "automation"` (never human consent for the reviewer). Repeating automations skip (`bot_busy`) if a prior run is already queued behind active work. Missed calendar runs during sleep coalesce to one run on wake. Turn failures alert the owner via lookout steer and appear in `/background`.
- **Idempotency & files:** Re-adding an identical definition returns `duplicate: true`; a differing one fails with `automation_exists` naming the differing field. Definitions live in `~/.agent/automations/jobs/NAME.json`, state in `~/.agent/automations/NAME.json`, on-demand asks in `NAME.asks`, locked via `~/.agent/automations/.lock`. Pinned to the daemon's store lineage (`store_mismatch` if another store reuses the socket).

Sonny ships the `automation` skill ([SKILL.md](../sonny/skills/automation/SKILL.md)) guiding agents to keep bookmarks/ledgers in files rather than relying on compacted chat history.

### The scheduler

Rather than creating one OS background item per automation, Sonny installs **one scheduler per `$HOME`** (`sonny.scheduler.TAG`, where `TAG` is an 8-hex hash of `$HOME`):

- **macOS:** `~/Library/LaunchAgents/sonny.scheduler.TAG.plist` (`RunAtLoad`, `KeepAlive` on crash)
- **Linux:** `~/.config/systemd/user/sonny.scheduler.TAG.service` (needs an active systemd user manager; run `loginctl enable-linger` for logged-out execution; fails with `automations_unsupported` if unavailable)

`sonny scheduler` handles all automations, `start_at_boot`, and 10-minute home backup syncs:

- Runs at most one concurrent run per automation (per-file lock in `~/.agent/automations/.scheduler`) and up to `max(8, 4 × CPUs)` runs in parallel in fair wait order.
- Tracks missed schedules in `looked` / `looked.log`, watches `--file` paths every 2s, wakes immediately on the `~/.agent/automations/.ring` FIFO, and re-execs itself on Sonny upgrades or system timezone changes.
- Starts daemons on Linux in an independent scope (`systemd-run --user --scope`) so the daemon outlives the job.
- `/background` and automation listings check unit health passively and report `scheduler_not_running` if the service failed.

![/background when the scheduler's systemd unit failed: one line under Automations, in systemd's words](sonny/scheduler-not-running.png)

## Memory

Memory stores lasting knowledge shared across conversations and git worktrees—decisions, user preferences, corrections, traps, and pointers—outside the repository checkout so every agent sees the same copy regardless of branch or commit.

### Where memory lives and how it loads

| Scope | Directory | Index |
| --- | --- | --- |
| **User (everywhere)** | `~/.agents/memory/` | `~/.agents/memory/MEMORY.md` |
| **Project** | `~/.agents/memory/projects/NAME/` | `~/.agents/memory/projects/NAME/MEMORY.md` |

- Each fact is one Markdown file (≤ 16 KiB, up to 16,384 facts/folder) with YAML front matter: `name`, `description`, `type` (`user`, `feedback`, `project`, `reference`, or `work`), `source`, `verified`, plus optional `external`, `live`, `confirmed`, and `pushed_back` counts.
- `MEMORY.md` is auto-generated under a folder lock, ordered with `work` facts first, then `user`/`feedback`, then the rest (ranked by `confirmed` count and `verified` date).
- New agents receive the head of both indexes (up to 16 KiB each) in their initial instructions ([client policy](CLIENT.md#layers)). If an index exceeds 16 KiB, descriptions automatically shorten (`160 → 80 → 40` chars → names only) so every fact remains visible without failing saves.
- **Live facts (`live: true`):** Store a Jinja template (`source()`, `file()`, `modified()`, `now()`) instead of stale copied numbers. `read` returns both template and `filled` text; new agent instructions fill the one-line description once at creation to keep prompt caches warm.
- **External provenance:** Facts saved during a turn that read web pages, search results, browser output, or MCP responses are automatically tagged `external: true` so later agents verify them before trusting them.

### Using memory

Agents use the `memory` tool (answered by the tool host outside the sandbox, no approval required); people and scripts can use `~/.agent/memory` (`sonny memory`):

```sh
~/.agent/memory show [SCOPE]
~/.agent/memory read NAME [SCOPE]
~/.agent/memory search WORDS... [SCOPE]
~/.agent/memory save NAME --type TYPE --description TEXT --source TEXT [--external] [SCOPE] -- TEXT|-
~/.agent/memory rm NAME [SCOPE]
~/.agent/memory index | check | said [SINCE_MS] | changed | cleanup start | cleanup finish
~/.agent/memory schedule [--model PROVIDER/MODEL [--effort LEVEL]] [--cron 'MIN HOUR DAY MONTH WEEKDAY']
  # SCOPE: --user | --project NAME (default: current folder's project)
```

- Browse and delete facts in Sonny with `/memory` (your browsing uses `memory::peek`, so it never affects agent read counts).
- `search` performs fast lexical matching across both scopes (up to 20 matches, weighting name/description 4×).
- `read` tracks agent read counts in `.reads` (outside git) so cleanup knows which facts went unread for 30 days.

### Nightly cleanup

Memory stays compact and accurate through a scheduled `memory-cleanup` automation (enabled automatically when you first open a lead, default `03:30` via `--fresh`, 1M token cap):

- **Skips quiet nights:** Gated by `--if` running `memory changed`, which checks for saved/removed facts, stale `work` facts (> 3 days old), or new human messages since the last run.
- **Extracts & reflects:** Reads what you wrote and reacted to since the last cleanup (`memory said`), mines recurring habits/preferences/confirmations (`confirmed` / `pushed_back`), removes facts pushed back on more than confirmed, retires stale `work` facts, and folds related facts when indexes exceed the 16 KiB head (`over_head`).
- **Safety rollback (`loss_guard`):** `~/.agents/memory` is a local git repo (never pushed). `cleanup start` snapshots the folder; if `cleanup finish` removes > 25% of lasting facts (and > 2, excluding folded text), it rolls back automatically while preserving concurrent saves from other agents.

## Commons

The commons (`~/.agents/commons/`) is a cross-project message board where agents across all your projects share findings, ask peers, and coordinate. Each thread is an append-only `NAME.jsonl` file (≤ 16 KiB/post, ≤ 256 posts/thread).

- **Agent tool (`commons`):** List threads, `read` (40 KiB pages), `search` (full-text + EventBridge-style `filter` on `by`, `by_id`, `at`, `model`, `project`, `sonny`, `external`, `title`), `post` (returns `similar_titles` to avoid duplicates), and `reply`.
- **Conflict guard:** Replying to a thread with unseen posts returns `posted: false` along with the missed posts first, preventing duplicate parallel answers.
- **Automatic provenance:** Sonny signs every post with author, model, project, Sonny version, and `external` taint if outside content was read.
- **Lookout integration:** Posting or replying auto-follows `commons:THREAD` (default `15m` digest under `[sonny.sources.commons]`).
- **Task discovery & invites:**
  - When a task starts, Sonny appends up to 3 BM25-matched related threads to its brief (`[sonny] commons_pointer = false` disables).
  - New threads invite a small, adaptive sample of relevant agents (searched via `history_search`, capped per day and on a 1M work-token budget) plus an occasional random "golden ticket" invite. Invites never chain from invited turns (`[sonny] commons_invites = 0` disables invites; `[sonny] commons = false` disables the commons tool).
- **View in Sonny:** `/commons` lists every project's threads (`a` narrows to the current project; Enter reads a thread).

![/commons as it opens, every project's threads, `a this project` among its keys](sonny/commons-every-project.png)

![/commons after `a`: the threads started in calc, the card's head naming it](sonny/commons-this-project.png)

### Boards

Boards turn commons threads into **self-claimed task queues** (e.g. `shop` or `shop/typed-errors`) so multi-agent teams can pull work without waiting on a lead.

- **Agent tool (`board`):** `open` (list open items), `add` (create an item with `board`, `needs` dependencies, `labels`, `for` agent/profile, 30-char verb-first title), and `act` (`claim`, `done`, `fail`, `release`, `cancel`, `reopen`, `label`, `assign`, or `item` to convert a thread).
- **Event-sourced & lease-free:** State (`open`, `waiting`, `claimed`, `done`, `failed`, `canceled`) is derived directly from append-only action posts. Finishing an item's last `needs` dependency automatically opens blocked items (`opened`). If a claiming agent closes or disappears, another project agent can take over without waiting on a lease.
- **Multi-agent tasks:** `start_task` accepts `board` and `agents` (a count or list of profiles like `["worker", "worker", "reviewer"]`), naming them `PROJECT.task.1`, `.2`, etc., each in its own worktree and subscribed to the board (`board.NAME` follow, with newly opened items steering immediately).
- **UI & toggle:** `/commons` displays a **Boards** section above threads (`m` mixes items into threads; `ctrl+f` filters); `/status` and the side pane group multi-agent tasks into a single expandable row. Disable with `[sonny] boards = false`.

![/commons with boards, at 150, 120, 60 and 44 columns](sonny/commons-boards.png)

![An item's page: where it stands, then its posts](sonny/commons-item-page.png)

![The pane at rest: a task with four agents, those that need you or work under it](sonny/pane-task-agents.png)

## Pause

`/pause REASON` pulls a fleet-wide **andon cord** across every project on the daemon:

- Running turns stop immediately and park (`Paused`), running commands terminate without auto-retry, and incoming messages, automations, and lookout notes wait durably in the store (across restarts too).
- Every Sonny window displays who paused, why, and which turns were parked—press `Esc` inside any parked agent's window to cancel that turn before resuming.
- `/resume` resumes all paused turns from where they stopped.
- Agents can pull the cord themselves via `pause_agents` when they spot runaway or harmful behavior; only a human client can resume. `/sky` animates pauses and resumes live.

![A lead pulling the cord with the pause_agents tool, its reason under the row, and the banner every window shows](sonny/agent-tools-pause.png)

Agents also coordinate peers natively through daemon tools (`send_message`, `list_agents`, `spawn_agent`, `close_agent`), which Sonny renders as clean folded rows:

![The steps view: a lead starting a reviewer with spawn_agent, then closing it with close_agent once its work is done](sonny/spawn-close-agent.png)

## Upgrading

`sonny upgrade` updates Sonny and its matching daemon cleanly with zero lost work:

- **Homebrew (`sonny` / `sonny-nightly`):** Runs `brew update` and upgrades both formulas (or reinstalls nightly when `main` advances).
- **Static Linux tarball (`install.sh`):** Downloads the latest release, verifies SHA-256 (and GitHub build attestation if `gh` is signed in), unpacks to `PREFIX/opt/sonny-VERSION-linux-ARCH`, and atomically updates `PREFIX/opt/sonny` and `PREFIX/bin/sonny`. (`--download` stages without switching.)
- **Source / Cargo builds:** Skips download and runs the handover steps below for the installed binary.

**What happens during handover:**

1. Locks `~/.agent/sonny/update.lock` (refused from inside an agent's shell).
2. Runs `agent start --replace`—running turns park cleanly and resume on the new daemon.
3. Verifies or restarts companions (`sonny-lookout`, `sonny-tool-host`, `sonny-reviewer`) on the new build and confirms stability.
4. Refreshes `~/.agent` wrapper scripts, automations, and the scheduler job path.
5. Reopens every open Sonny terminal window in place on the same agent with your unsent composer draft and pasted images intact.
6. Reports any edited skills or roles whose new shipped text is waiting for merge in `~/.agent/sonny/upgrades/`.

![sonny upgrade, from a release tarball install of 0.9.4 to 0.9.5](sonny/sonny-update.png)

![A window reopened on 0.9.5 by the update above, its unsent message still in the composer](sonny/update-reopened.png)

## Skills

At startup, Sonny writes its shipped skills into `~/.agents/skills/` (loaded via [client policy](CLIENT.md)):

- **Core workflow:** `plan`, `memory`, `memory-cleanup`, `zen`, `coordination`, `commons`, `board`, `automation`, `margin`, `workflow`, `pictures`
- **Setup & config:** `sonny-setup`, `sonny-config` (generated from `/config` keys)
- **Integrations:** `rex-windows`, `herdr-panes`, `cmux-tabs` (for detected multiplexers), plus `agent-browser` and `agent-desktop` when installed.

### Preserving your edits and upgrade notes

- Sonny fingerprints shipped files in `~/.agent/sonny/skills` and `~/.agent/sonny/agents`. If you edit a shipped skill or role (`lead.md`, `task.md`), Sonny stops overwriting it. (Delete your copy to restore Sonny's.)
- When a Sonny upgrade updates a file you edited, Sonny places the new version in `~/.agent/sonny/upgrades/` alongside your original baseline in `~/.agent/sonny/shipped/`, and queues a one-time upgrade note to the first lead you open offering to merge the changes, summarize new rows in [FEATURES.md](../sonny/FEATURES.md), offer new setup steps, and run any judgment-based upgrade steps (`sonny/upgrades/ID.md`).
- Mechanical migrations run automatically once via `upgrade::once` (`sonny/src/upgrade.rs`) at startup—never in the lookout.

## Tool host

Sonny's agent tools (`start_task`, `remove_task`, `memory`, `automations`, `follows`, `commons`, `board`, `exec`, `workflow`, `browser`, `desktop`, `margin`, `live`, `show_agent`, `new_project`, `react`, `ask`) are answered by the daemon companion `sonny-tool-host` (`sonny tool-host --store STORE --socket SOCKET`).

- Runs independently of `sonny-lookout` so slow MCP calls, workflows, or browser sessions never delay event delivery.
- Stays alive when all Sonny windows close (`STORE.tool-host.lock`, `STORE.tool-host.sonny`, `STORE.tool-host.cursor`), exiting 30s after the daemon stops.
- Runs each tool call as `sonny tool NAME` in the caller's workspace (`exec` and `workflow` run in-process where MCP connections live) and cancels in-flight calls if the turn ends.
- Enforces non-escalation: any task or workflow worker created by an agent inherits permissions equal to or stricter than its maker (`start_task` accepts a stricter `permissions` row; looser rows are refused).
- `/background` warns `tools not answered` if no tool host is running.

## Workflows

The `workflow` tool lets an agent orchestrate many short-lived worker bots from a single JavaScript script (running in QuickJS inside `sonny-tool-host`) instead of manual back-and-forth turns.

- **Async & summarized:** Returns a run ID immediately; when finished, queues one `workflow`-origin message with status, counts, tokens, and the result (or a 4 KiB outline + path for large results).
- **Workers (`PROJECT.wf-RUN-KEY`):** Created per `agent()` call in the caller's folder (or a temporary worktree with `isolation: 'worktree'`, kept only if it has commits), confined to the caller's sandbox and approval gates, with `memory`, `exec`, `browser`, and `desktop` (no agent-spawning or user-facing tools). Supports strict JSON `schema` validation with one automatic retry.
- **Durable journaling & resume:** Stored in `STORE.workflows/RUN/` (`run.json`, `script.js`, `events.jsonl`, `result.json`). Keyed by prompt + options hash so restarts replay completed calls from the journal at zero cost, and `resume_from_run_id` reuses answers from an earlier run.
- **Monitoring & config:** Active runs appear as one live row in `/status` (`m` expands phases/logs; `s` or `x` stops). Configure under `[sonny.workflows]` in `config.toml`: `enabled` (`true`), `concurrency` (`0` = `min(16, CPUs - 2)`), `max_agents` (`1000`), `max_runs` (`8`).

## MCP servers

Configure MCP servers in `~/.agent/config.toml` (using Codex-compatible keys) or import them from other harnesses:

```toml
[sonny.mcp_servers.linear]
url = "https://mcp.linear.app/mcp"
bearer_token_env_var = "LINEAR_TOKEN"      # from env or ~/.agent/keys/env
description = "Issues and projects in Linear"

[sonny.mcp_servers.playwright]
command = "npx"
args = ["@playwright/mcp@latest"]
enabled_tools = ["browser_navigate", "browser_snapshot", "browser_click"]
default_tools_approval_mode = "writes"

[sonny.mcp_servers.playwright.tools.browser_snapshot]
approval_mode = "approve"
```

### Managing and importing servers

- `sonny mcp add NAME -- COMMAND ARGS` (or `--url URL`), `sonny mcp remove NAME`, `sonny mcp list`, `sonny mcp tool`, or `/mcp` in Sonny.
- `sonny mcp import` discovers servers from Codex (`~/.codex/config.toml`), Claude Code (`~/.claude.json`, `.mcp.json`), OpenCode (`opencode.json`), and Kiro (`.kiro/settings/mcp.json`). Project-local servers start unticked for safety; literal secrets are extracted into `~/.agent/keys/env` rather than `config.toml`. (Legacy SSE servers are skipped.)

![sonny mcp import](sonny/mcp-import.png)

### Code mode (`exec`)

Instead of dumping dozens of individual MCP tool schemas into every model prompt, Sonny gives agents **one `exec` tool** running JavaScript in sandboxed QuickJS (no filesystem, network, or timers):

- Call MCP tools via `await tools.mcp__SERVER__TOOL(args)`, filter/transform in JS, and return only what matters via `text(...)` and `image(...)` (up to 20 images / 5 MiB; PNG/JPEG/GIF/WebP auto-scaled to 1568px).
- **Code mode (`[sonny] code_mode = true`, default):** Exposes the agent's built-in tools (`tools.shell`, `tools.read`, `tools.write`, `tools.edit`, `tools.apply_patch`, `tools.web_fetch`) inside `exec` as well, hiding raw tool schemas via `steps_only`.
- **Tool discovery & limits:** `ALL_TOOLS` and `tools.tool_search(query)` find tools beyond the 3,000-token TypeScript header; `store`/`load` persist small values between scripts. Limits: 256 MiB heap, 10m timeout (overridable with `// @exec: {"max_output_tokens": 1000, "timeout_ms": 60000}`), 10k-token return (full output saved to disk up to 1 GiB for 7 days), 8 concurrent in-script calls, 16 MiB per call result.
- **Shared server pool:** `sonny-tool-host` maintains one connection per server across all agents (started on first call, stopped after 10 idle minutes) and caches tool schemas in `~/.agent/sonny/mcp`.

![The every-tool-call view: a lead's script ran shell once as a step of its exec call, its result under the row](sonny/code-mode-script.png)

### Per-call approvals and auditing

Scripts are never approved as a black box—every tool call inside `exec` goes through `approve_step` individually:

| `approval_mode` | When it asks the agent's gate |
| --- | --- |
| `auto` *(default)* | Tools marked `destructiveHint` (unless also `readOnlyHint`) |
| `writes` | Every tool except `readOnlyHint` |
| `prompt` | Every call |
| `approve` | Never asks |

Built-in tool steps (`tools.shell`, etc.) go through the agent's normal sandbox, command rules, and approval gates. Every MCP, browser, desktop, and automation call is recorded in the daemon audit trail (`/audit` in Sonny; `agent audit` on CLI).

## Links

`sonny link` connects a machine where you are signed in (like your laptop) to a remote host (like a cloud devbox) over outbound SSH—letting the host's agents use your laptop's **signed-in browser**, **desktop**, and **local MCP servers**, while approvals still appear in your Sonny window on the host.

```sh
# Run on your laptop (the machine with logins/browser/MCP)
sonny link devbox              # or me@devbox.example.com [--machine NAME] [--name NAME]
sonny link                     # inspect active links and connected machines
sonny link off devbox
sonny link forget laptop       # run on the host to forget a linked machine
```

### How it works

1. `sonny link HOST` checks Sonny on `HOST` (installing via `install.sh` if needed), writes `[sonny.links.NAME]` to `~/.agent/config.toml`, and enables `start_at_boot`.
2. Your laptop's tool host maintains an outbound `ssh -T HOST sonny link serve MACHINE` tunnel (auto-reconnecting with backoff). No open ports, port forwarding, or `sshd` changes are needed.
3. On the host, `sonny link serve` exposes `~/.agent/links/MACHINE.sock` (cleaning up after 45s of silence if the laptop sleeps). Connections must prove host-user ownership via `~/.agent/links/.proof`, preventing sandboxed agents from bypassing the tool host.

### What host agents can do on a linked machine

- **Browser (`browser` with `machine: "laptop"`):** Runs tabs in a dedicated shared Chrome instance copied once from your `browser_profile` (default `"Default"`; refreshes cookies and native messaging hosts on start, closes after 30 idle minutes). Screenshots return inline; file outputs (PDF, HAR, recordings up to 30 MiB) stream back to approved host paths.
- **Desktop (`desktop` with `machine: "laptop"`):** Runs `agent-desktop` on the linked machine with action approvals routed to the host agent's gate.
- **MCP (`exec`):** Prefix a script with `// @exec: {"machine": "laptop"}` to run against that machine's MCP servers (listed under `# On NAME`).
- **Opening links & files:** On headless hosts, clicking an `http(s)` URL or large image/PDF in Sonny opens it directly on your connected laptop.

Configuration in `~/.agent/config.toml`:

```toml
[sonny.links.devbox]
host = "devbox"
machine = "laptop"
command = "/home/me/.local/bin/sonny"
browser_profile = "Default"    # Chrome profile name, path, or "" for fresh browsers
calls = 16                     # max concurrent remote calls (default: 4 × CPUs)
```

Error codes: `not_connected` (asleep/offline), `tool_outcome_unknown` (link dropped mid-call), `link_busy` (at `calls` cap). Both ends must speak link protocol 1.

## Keys never wait on the daemon

Sonny's UI event loop never blocks on daemon RPCs:

- Daemon streaming, outgoing sends (`Enter`, steers, answers, interrupts, `/compact`), background refreshes, and reconnect catch-up all run off the UI thread. Sent messages appear in the conversation immediately.
- Incoming events buffer in a bounded queue (`4,096` events / `8 MiB`). If a window falls behind, it drops the connection, reconnects, and catches up cleanly from history in order.

![After: the steer shows under the reply at once, and the next words show as they are typed](sonny/keys-held-daemon.png)

## Checks

- Unit tests: `cargo test -p agent-client` and `cargo test -p sonny`.
- Runtime automation & scheduler integration tests: after `cargo build --release` and `cargo build -p sonny`, run `AGENT_TEST_RUNTIME=1 python3 -m unittest tests.test_trigger`.
- Real OS scheduler tests: add `AGENT_TEST_LAUNCHD=1` on macOS or `AGENT_TEST_SYSTEMD=1` on Linux.
