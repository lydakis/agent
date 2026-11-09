Protocol 55 -> 61 and schema 84 -> 89. The first `sonny` or `agent` command after the upgrade replaces the running daemon at its next turn boundary. The store is copied and checked before it migrates. Restart Sonny's windows after the upgrade, so they speak protocol 61 too.

## Sonny

**Leads and tasks**
- `/status` opens quiet: each row is the agent, its state and time, and what needs you says what for. Its model, profile, latest step and what it tracks show on `m`. Only what is broken shows under the heading (#803).
- `/status` reads each agent's state from the daemon's listing, and keeps what it read of histories for the next window. Opening it at 1,000 agents went from 1.2 s to 52 ms, and a refresh from 333 ms to 48 ms (#831, #834, #842).
- Needs you is back to hard facts, as in 0.8.1: a call waiting for your approval, an open question, a failed turn, or a hold you can fix. The 0.9.0 reply check, the 8-hour done check and the code-review tracker are removed. A row shows the pull request its agent's last reply linked, and agents mark themselves done with `mark_done` (#800, #844).
- A lead can be marked done, as a task is. Its automations pause until a message reopens it (#838). `mark_done` is answered once its mark is kept, with no window needed (#843).
- `remove_task` keeps the agent and its history and marks the task done. Only you delete an agent, with `agent rm` (#808).
- Tasks run at their starter's speed. `threads_speed` in `.agents/project.toml` and `[sonny] task_speed` set one (#804).
- A lead opens without a file it cannot read or fit, and says what it left out, instead of refusing to open (#832).
- `/context` shows what fills an agent's context (#835).
- Quiet mode is removed: `/quiet`, `[sonny] hours`, `quiet_after` and the away skill (#846).

**Conversation**
- Words only by default: what you said and the agent's words, questions and failures. `[sonny] steps = true` brings the steps back. A waiting message carries a mark, ↪ or ⋯ (#806).
- One renderer for every message: your own and queued ones draw their Markdown as a reply does (#828).
- The message under the mouse says when it was written (#839).
- The conversation keeps two blank columns from the window's edge (#824). A gap across history pages is one row (#799). Older pages are drawn after the frame and read ahead while scrolled up (wheel to frame p99 10.7 -> 5.6 ms) (#805).
- An agent's whole name opens it everywhere, and only a link is blue (#802). A source's icon marks it wherever Sonny names it (#807).
- An idea or a tip may fill the empty composer when no next message is obvious. `[sonny] suggest_ideas = false` turns it off (#813).
- A key in a 1 MiB prompt costs what it does in a short one (#814). Lists rank once per letter typed (#819).

**Reviewer and words**
- The judge and the approver are one word, the reviewer: `[judge]` is `[reviewer]`, `sonny approver --judge` is `sonny reviewer --model`, and the companion is `sonny-reviewer` (#815).
- Agents say `follows` for the lookout's tool (was `subscriptions`) and name the reviewer in denials (#837).

**ACP clients** (`sonny acp`)
- Any client picks the lead behind a session, and lists leads and their tasks to reopen (#817, #836).
- A prompt can name its writer (`_meta.person`), and the lead reads who wrote it. The reviewer takes none of it as your consent (#826).
- The lead's questions, and its tasks', come as forms where the client fills them in, and a pick answers them (#829, #849).
- Reactions go both ways over `_sonny/react` (#852).

**Commons**
- An invite is the one turn the commons starts. After that, replies reach agents through their follow alone (#809).
- `search` finds threads by their words, and a new thread names threads with similar titles (#821).

**Linked machines and browser**
- An agent's tools always fit: MCP servers' instructions are capped, so a linked machine's servers no longer hide `browser` and `desktop` (#850).
- A linked machine renamed in its config is listed once, by its new name (#851).
- A link's agents share one Chrome on a profile kept for the link, a tab each, with the system keychain, so sign-ins and device checks hold (#853).
- Agents ask `sonny where` for their terminal, with a skill for each multiplexer the machine has (#810). The browser tool says a screenshot without a path is the agent's only (#811). Browser commands share the process budget (#819).

**Sky and window**
- A click on a star or a name in `/sky` centres on it (#818).
- An idle window wakes once a second, not ten times: idle CPU 0.40% -> 0.18% of a core (#820).
- F28, H18, E2 re-checks: compaction no longer holds the last words, and a waiting automation fire says nothing (#825).

## Daemon
- Audit trail: a row for every tool call of every agent and every step Sonny's tool host runs, with its kind, where it ran, who approved it, read or write, and how it ended. Read it with `agent audit`, `/audit` in Sonny, or an agent's history tool. Nothing trims it on its own (#823).
- Usage is kept under the model that spent it, and `/usage` prices it from your own sheet, `~/.agent/prices.toml` (#816).
- A reply the model's `max_tokens` cuts is dropped whole and the turn goes on; `max_drops` (3) in a row end it. The daemon's own reply bounds are one 16 MiB backstop (#822).
- A `configure` while a turn runs is kept for the turn's end, not refused as busy (#848).
- A summary due as a turn ends is written after it, while its cache is warm, not before the next call (#833).
- A pause admits no call taken and no step allowed until resume (#841).
- Each bot row carries its whole status: its turn, what it waits on and how its last turn ended (#831).
- Providers: Grok and other models on Bedrock work. Custom tools go only to api.openai.com, hosted search only where served, and the endpoint's own errors show. `web_search_off` leaves search out on models you name (#801).
- Speed: a turn's start parses nothing it already has (daemon CPU 0.305 -> 0.245 s on the c32 bench) (#840). Waiters leaving, sessions closing and parked calls draining are no longer quadratic: 16,000 at once went from seconds to milliseconds (#812, #827, #830).
- Sandbox (Linux): bubblewrap's placeholder folders are held across processes by lock files in `~/.agent/placeholders` and removed by the last one out, gone writable roots are skipped, and an empty folder at a protected name counts as missing, as Codex does (#854).

## Also
- Two load-dependent test races are fixed (#847).

## Upgrade
- **Restart the daemon after upgrading, and reopen Sonny's windows.** The first command replaces the daemon, and a window from 0.9.0 cannot speak protocol 61.
- **Linux: delete the old empty sandbox placeholders by hand.** Before 0.9.1 a restart or crash could leave empty `.git`, `.agents` or `.codex` folders in your workspaces. 0.9.1 no longer makes new strays and treats an empty one as missing, but it does not remove the old ones. List them with `find ~ -type d -empty \( -name .git -o -name .agents -o -name .codex \) 2>/dev/null`, then delete the ones you didn't make (`rmdir PATH`).
- The audit trail starts empty; earlier calls stay in history only. `agent audit trim --older-than 90d` is yours alone (#823).
- The usage migration files each existing turn under its model, about 1.4 s per million turns. Turns from before have no cache writes recorded (#816).
- Grok on Mantle needs `web_search_off = ["bedrock-openai/xai.*"]` in `~/.agent/config.toml` (#801).
- Once, on first start: `[judge]` becomes `[reviewer]`, and the companion `approver-auto` becomes `sonny-reviewer` (#815). Quiet mode's `hours` and `quiet_after` leave the config, with its files and skill (#846). `needs_check` and `task_done_check` leave the config (`checks-gone`, #855). The commons' wake marks go (#809).
- Marks the removed checks left beside the store stay on disk, unread (#844).
- These migrations retire at 0.11.0.
