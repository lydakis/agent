# Lookout

`sonny lookout` is Sonny's event pipe from **sources** to **bots**—like SNS topics to subscribers. It delivers updates from tasks, external scripts (chat, CI, PRs, email), and Sonny's own built-in feeds (`commons`, `memory`, `links`) into subscribed bots as steered notes, with EventBridge-style filters, digests, and instant priority steers.

The lookout does **one job**: pass events on, fast and durably. It makes no judgment calls on content and holds no per-source rules—each source puts what it knows in its event fields, and the receiving bot decides what to do.

[Quick example](#quick-example) · [Follows](#follows) · [Sources](#sources) · [Filters](#filters) · [Tasks source](#the-tasks-source) · [Digests & steers](#digests) · [Delivered notes](#the-note) · [Runtime architecture](#one-lookout-a-daemon)

## Quick example

1. **Add an external source** in `~/.agent/config.toml`:

```toml
[sonny.sources.ci]
command = "~/bin/ci-poll"
every = "1m"
digest = "15m"
steer = { status = ["failed"] }
```

2. **Follow it** from an agent using its `follows` tool:

```json
{"add": {"name": "main-ci", "sources": ["ci:main"], "filter": {"status": ["failed"]}}}
```

3. **Receive steered notes** in the bot's conversation:

```text
[ci · main · 10:31]
deploy failed on prod
[lookout]
This note is news for you, unseen by the person. Reply when it bears on what the person is doing now or is urgent; otherwise end your turn with an empty reply.
```

In Sonny's window, lookout notes fold into one quiet line (`↳ Lookout · calc.fix completed · ▲ ci`) and stay silent if the agent decides there's nothing to report.

## Follows

Each agent manages its own subscriptions through the `follows` tool (stored in `STORE.lookout/BOT.json`, reloaded every 15s and immediately after `follows` or `commons` calls).

When Sonny opens a project, it creates the lead's default `tasks` follow if missing:

```json
{"name": "tasks", "tasks_of": ["shop.lead"], "agents": ["shop.*"]}
```

Additional peer leads in the same folder (`shop.lead.billing`) get `tasks_of` only, so each lead hears only its own tasks. Another lead (which has its own follows) is heard only if named explicitly without `*`.

### `follows` tool operations

| Action | Example | What it does |
| --- | --- | --- |
| **List** | `{}` (or `{"from": "..."}`) | Paged one-line summary of follows, 24h `lately` counts, configured sources, and `lookout` sync status |
| **Add** | `{"add": {"name": "deploys", "agents": ["ops.*"]}}` | Adds a follow (same spec is a no-op; conflicting spec under an existing name is refused) |
| **Dry run** | `{"add": {...}, "test": true}` | Tests against the last 24h of source history (`events`, `heard`, `steers`, `notes`, 3 sample events) without adding |
| **Read one** | `{"read": "deploys"}` | Full follow definition, 24h counts, and current retained state (`now`) of its sources |
| **Inspect source** | `{"source": "prs"}` or `{"source": "prs:12"}` | Schema of recent fields (`fields`, `topics`) plus current retained state (`now`) without subscribing |
| **Remove** | `{"remove": "deploys"}` | Removes the follow and its counters |

> Use `/background` in Sonny to inspect all follows for the project and remove one with `x`.

### Follow fields

A follow must specify at least one target (`tasks_of`, `agents`, or `sources`):

| Field | Meaning |
| --- | --- |
| `tasks_of` | Bots created by any listed agent or its descendants |
| `agents` | Bots matching name globs (`*` wildcard); never matches side chats or auto-expiring bots |
| `sources` | Configured or built-in source names (`"slack"`) or specific topics (`"slack:deploys"`) |
| `filter` | Event pattern narrowing which events are heard ([Filters](#filters)) |
| `digest` | Max delivery frequency (e.g. `"15m"`, `"1h"`, `"0"` for immediate); overrides the source's digest |
| `steer` | Event pattern that cuts through any digest immediately ([Steers](#steers)) |
| `until` | Local timestamp (`"2026-11-08 18:00"`) when the follow automatically ends |
| `runs` | Ends the follow after delivering `N` notes (`"runs": 1` for a one-shot alert) |

Examples:

```json
{"add": {"name": "api", "agents": ["api.lead"],
         "filter": {"type": ["turn.ended"], "$or": [{"status": ["failed"]}, {"done": [true]}]}}}
{"add": {"name": "leads", "agents": ["*.lead"], "digest": "1h"}}
{"add": {"name": "deploy", "agents": ["ops.deploy"], "steer": {"status": ["failed"]}, "runs": 1}}
{"add": {"name": "launch", "sources": ["slack:launch"], "until": "2026-11-08 18:00"}}
```

### Lifecycle and counters

- Follows are pinned by bot ID—a new bot reusing an old name inherits none.
- A follow ends automatically when `until` passes, `runs` is reached, its owner bot is deleted, all explicitly named target bots are deleted (with no sources), or its owner is closed via `close_agent` (except follows marked `outlives_close`, such as commons thread subscriptions).
- 24-hour rolling counters (`heard`, `notes`, `steers`, `last`) are tracked hourly in `STORE.lookout/.counts/BOT.json` so noisy follows are easy to spot without reading logs.

## Sources

External sources are shell commands configured by the user in `~/.agent/config.toml` and executed with `sh -c` as your user (outside any sandbox):

```toml
[sonny.sources.slack]
command = "~/bin/slack-poll --channel eng"
every = "1m"                   # delay after exit before rerunning (default 1m); streaming commands stay running
digest = "15m"                 # batching interval (default 0 = immediate)
steer = { from = ["boss"] }    # event pattern that bypasses digest
watch = ["~/bin/slack-poll"]   # restart source within 15s when these files change
icon = "💬"                    # optional icon shown in Sonny UI
```

> **Security:** Keep source commands and `watch` files outside agent-writable directories—they run as your user outside any sandbox.

### Source output protocol

Each line printed to stdout is one event (lines over 64 KiB are skipped; `text` is capped at 4,000 chars):

- **Plain text:** Delivered as `{"text": LINE}`.
- **JSON object:** Uses `text` plus optional standard fields (`id`, `topic`, `from`, `url`, `steer: true`) and any custom fields for filtering. Objects without `text` are delivered whole.
- **Clearing a topic:** An object with empty `"text": ""` clears the retained latest value for that `topic` (or the whole source if no topic) without waking followers.
- **Deduplication & state:** The last 1,024 event `id`s are deduplicated automatically. Sources receive `$SONNY_SOURCE` (name) and `$SONNY_SOURCE_DIR` (`STORE.sources/NAME`) for storing their own cursors.
- **Logs & health:** The first 20 lines of stderr per run go to `STORE.companions/sonny-lookout.log`. After **3 consecutive failed runs**, the lookout marks the source **down** (stored in `STORE.sources/.health/NAME.json`, surfaced in `/background`), notifies all followers once per day with the exit reason and last 5 stderr lines, and announces recovery when it succeeds or emits an event.
- **Hot reload:** The lookout reloads `config.toml` every 15 seconds, starting added/changed sources, stopping removed ones, and restarting sources whose `watch` files changed.

### Dynamic fetch hints (`wants`)

For large systems (like a code-review tracker) where polling everything is impractical, set `wants` to an event field (e.g. `wants = "id"`, `"topic"`, or `"meta.repo"`):

```toml
[sonny.sources.cr]
command = "~/bin/cr-poll"
wants = "id"
```

The lookout writes `$SONNY_SOURCE_WANTS` before starting the command and restarts it whenever subscriptions change:

```json
{"field": "id", "all": false, "values": ["CR-12", "CR-14"]}
```

`all` is `true` if any follower uses a bare or non-exact filter. An agent following `{"filter": {"id": ["CR-12"]}}` automatically tells the source what to fetch.

### Built-in sources

Sonny provides four built-in sources (user sources cannot reuse their names):

| Source | Poll / trigger | What it emits |
| --- | --- | --- |
| **`tasks`** | Live daemon stream | Activity in watched bots ([The tasks source](#the-tasks-source)) |
| **`commons`** | Every 10s | New posts in `~/.agents/commons` (`topic` = thread, `from` = author, `id` = `THREAD/N`). Default digest: `15m` (configurable under `[sonny.sources.commons]`). Posting in a thread auto-follows `commons:THREAD`. |
| **`memory`** | Every 10s (opt-in) | Saved or removed facts in `~/.agents/memory` (`topic` = fact name, fields: `project`, `type`, `file`). |
| **`links`** | Every 10s (opt-in) | Linked machines (`sonny link`) connecting or going down after 45s silence (`topic` = machine, `state` = `up`/`down`). |

### Source shapes and retained state

To help agents write filters and inspect current status without subscribing, the lookout maintains two files per source (on a background writer thread):

- **Shape (`STORE.sources/.shapes/NAME.json`):** Field names/types across the last 200 events and up to 32 recent `topics`. Shown by `{"source": "NAME"}` (sample values are only shown if an existing follow of the caller can see them).
- **Recent & retained log (`STORE.sources/.recent/NAME.jsonl`):** Keeps 24 hours of events (up to 2,000 events / 4 MiB) for `"test": true` dry runs, plus the latest event per topic indefinitely (up to 1,000 topics / 512 KiB) for `{"source": "NAME:TOPIC"}` and `{"read": "FOLLOW"}` status queries (and live margin templates). `commons` and `tasks` do not use `.recent` (use the `commons` and `history` tools instead).

### Filters

Event patterns follow AWS EventBridge / SNS filter policy syntax. Every listed field must match; a field maps to a list of allowed values (OR) or a nested object pattern; list fields match if any element matches.

| Matcher | Example |
| --- | --- |
| Exact value | `{"status": ["failed", "stopped"]}` (string, number, boolean, or `null`) |
| Prefix / suffix | `{"paths": [{"prefix": "src/billing/"}]}` · `{"file": [{"suffix": ".rs"}]}` |
| Wildcard (`*`) | `{"branch": [{"wildcard": "release/*"}]}` |
| Case-insensitive | `{"env": [{"equals-ignore-case": "prod"}]}` |
| Negation | `{"author": [{"anything-but": "me"}]}` (value, list, or string matcher) |
| Numeric range | `{"seq": [{"numeric": [">", 0, "<=", 100]}]}` (exact integer comparison past 2^53) |
| Existence | `{"url": [{"exists": true}]}` |
| Logical OR | `{"$or": [{"status": ["failed"]}, {"done": [true]}]}` |

Limits: max 4 KiB, 256 values, 6 levels deep. Filters run before queuing, so unmatched events wake nobody. A filter referencing a field never seen in recent events is accepted with an `unseen` warning.

Example:

```json
{"add": {"name": "billing-changes", "sources": ["cr"],
         "filter": {"paths": [{"prefix": "src/billing/"}], "author": [{"anything-but": "me"}]}}}
```

## The tasks source

The built-in `tasks` source emits structured events for watched bots (`topic` = bot name):

| `type` | When it fires | `from` in header | Text |
| --- | --- | --- | --- |
| `wrote` | Someone other than the follower writes in the bot | `the person`, or sender name | Human words quoted verbatim by daemon; other senders inline |
| `turn.ended` | A turn finishes | `TASK replied to WHO, STATUS` (or `ended`, plus error / `, marked done`) | The task's reply |
| `question` | Task calls `ask` | `TASK asked the person` | Question and options (or command to run) |
| `approval` | Call waits on human approval (once/turn) | `TASK waits for the person's approval` | Command or tool call summary |
| `margin` | Task posts or removes a margin note | `TASK put/took away the margin note "ID"` | `TASK/ID` |

Filterable fields: `type`, `bot`, `maker`, `from`, `turn`, `text`, plus kind-specific `status` (`completed`, `failed`, `stopped`, `interrupted`), `done`, `asked_by`, `options`, `command`, `call`, `id`, `removed`.

- `wrote` (human), `question`, and `approval` are automatically marked as steers.
- **Loop prevention:** Turns started by a lookout note emit nothing, and automation runs that end with an empty reply emit nothing.
- Replies are fetched concurrently (one connection per logical CPU, 60s timeout) while preserving per-task event order.

### What it tells

Because the lookout reports task completions, questions, approvals, and failures automatically, leads don't need polling automations to watch running work.

### Bounced messages

If a bot sends a message to another bot (`from` recorded on `accepted`/`queued`/`steered`) and that turn ends `failed` or `interrupted`, the lookout steers a bounce back to the sender (unless the sender already follows that bot via `tasks`):

```text
[your message to BOT was not carried out: turn N ended CODE]
```

Fixable causes (like expired logins) are [held](RUST_PROTOTYPE.md#held-turns) for `hold_ms` (default 5 days) rather than failing immediately, so bounces represent final failures or expired holds.

## Digests and steers

### Digests

- By default, events for a follower are grouped with `group_wait = 0` and `group_interval = 2s` (first event goes immediately; bursts within 2s batch into one note).
- Setting `digest` (e.g. `"15m"`, `"1h"`) on a source or follow caps delivery frequency: the first event after a quiet period goes immediately, and subsequent events wait until the interval elapses before sending together.
- A follow's `digest` overrides the source's (`"0"` disables batching). If multiple follows match one event, the earliest allowed schedule wins. Pending events and digest timers persist in `STORE.sources/.held/BOT.json`.

### Steers

- Events marked `"steer": true` by the source, matched by the source's `steer` pattern, or matched by a follow's `steer` pattern bypass all digests and deliver immediately.
- Convention: agents add `steer` patterns for items they own (like an open PR), while `filter` and `digest` are normally set only when requested by the user.

## The note

Notes are delivered one at a time per follower as steered messages with `origin: "lookout"` (so the Auto reviewer never treats lookout content as human consent). Human messages written inside tasks are attached via daemon `quotes` so the lead reads exact words rather than retellings.

```text
[tasks · calc.docs · the person]
(their words are quoted below, from calc.docs/3)
[tasks · calc.fix · calc.fix replied, completed]
Fixed avg: it skips empty lists now, with a test.
[ci · main · 10:31]
deploy failed on prod
[lookout]
This note is news for you, unseen by the person. Reply when it bears on what the person is doing now or is urgent; otherwise end your turn with an empty reply.

[Message calc.lead/41, sent by lookout.]

[Quoted from message calc.docs/3, written by a person:]

Use tabs.
```

- **Size limits:** Up to 192 KiB and 64 quotes per note (each event up to 32,000 chars text + 32,000 chars extra fields). Overflow waits for the next note, announced under `[lookout]`.
- **Queue cap:** Up to `1,024` waiting events per follower (`[sonny] lookout_waiting`, `0` for unlimited), persisted on disk. Past the cap, newer non-steer events are dropped (steers displace the newest non-steer) and the next note reports the exact gap under `[lookout]`.
- **Trailer:** Controlled globally by `[sonny] lookout_trailer` (`""` for none). Included on every note so agents reliably stay quiet on non-urgent updates.
- **Retries:** Daemon rejections retry after 2 minutes; connection drops retry immediately upon reconnect.

![A lead's window: the lookout's note folded to one row, ↳ Lookout · calc.fix-avg completed, then the lead's reply](sonny/lookout-note-folded.png)

![The same note opened: its [tasks · calc.fix-avg · calc.fix-avg replied, completed] header and the task's reply under it](sonny/lookout-note-open.png)

## One lookout a daemon

- **Daemon companion:** `sonny lookout --store STORE --socket SOCKET` runs as the daemon companion `sonny-lookout` (added when Sonny opens a project). It stays alive across closed Sonny windows, restarts on failure, and exits 30s after the daemon stops.
- **Single owner & upgrades:** Holds `STORE.lookout.lock` and records its build in `STORE.lookout.sonny`. A newer Sonny build replaces an older lookout cleanly; a duplicate of the same build prints `{"event":"already_watching"}` and exits.
- **Separated from tools:** Sonny's agent tools (`start_task`, `exec`, `memory`, etc.) run in a separate companion, `sonny-tool-host` ([tool host](SONNY.md#tool-host)), so slow MCP servers or browser calls never stall lookout delivery.
- **Durable recovery:** `STORE.lookout.cursor` tracks the global event cursor, per-follower delivery marks (so a digesting lead never stalls other leads), and the replay start point for in-flight turns. On restart or reconnect, it replays only what it missed—never the entire log.
- **Fleet scale:** Subscribes only to needed event kinds (skipping streamed text deltas), indexes creator relationships in memory so startup scales with watched tasks, and dispatches daemon calls off the event loop (`bench/lookout_scale.py`).
- **Logs & state:** Companion log in `STORE.companions/sonny-lookout.log`; lifecycle events (`seated`, `reloaded`, `woke`) in `STORE.lookout.jsonl`; file sync state in `STORE.lookout.taken.json`.
