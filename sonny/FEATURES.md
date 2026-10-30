# Sonny's features

Every Sonny feature we have added, changed or removed, why, and whether
anyone uses it. It is a register of what Sonny is and was, so a feature can
be looked at again later: kept, changed, or cut. What a feature does today
is in [README.md](README.md) and [docs/SONNY.md](../docs/SONNY.md); this
file keeps its history and its reasons.

It borrows two conventions. From [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
the words for what happened (added, changed, removed) and newest last in
each history. From feature-flag registries the idea that every experiment
is listed with its status, so none stays an experiment by accident.

## Keeping it

A pull request that adds, changes or removes a Sonny feature edits its row
here in the same pull request: a new row for a new feature, `, changed
MM-DD #N` or `, removed MM-DD #N` on an old one, and the status. One row
per feature; group small things into the row they belong to.

- **Status.** `experimental`: built to try, kept only if it proves itself
  in daily use. `shipped`: part of Sonny's settled experience. `removed`:
  gone; the row stays, with why.
- **History.** Dates are 2026, UTC, the day the pull request merged.
  Pull requests before #146 belong to the Agent app or the daemon, which
  Sonny took over.
- **Used?** Filled in by the person who uses Sonny, from use, not from the
  code: `yes`, `no`, `rarely`, or a note. `?` means nobody has said.

## Leads and tasks

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| A lead per folder: `sonny` in a folder opens that folder's own lasting agent | shipped | added 10-26 #146; every folder outside git gets its own 10-28 #259; a project keeps its subfolders 10-28 #263; a subfolder's lead reads its own project's memory 10-28 #298 | One coordinator per project that the person talks to and that starts the work | ? |
| Several leads in one folder (`/lead`) | experimental | added 10-29 #365; each store notes its last lead 10-29 #376 | Parallel conversations in one project without a second folder | ? |
| Tasks: the lead starts agents, each in its own git worktree (`sonny task`) | shipped | added 10-26 #142, #162; brief names its copy 10-28 #234; copies isolated, done tasks removed 10-28 #227; worktree starts with the lead's build folders 10-28 #286 | Only the lead works in its folder, so tasks never step on it or each other | ? |
| Task states and resolving (`/status` groups, `resolve_task`) | shipped | added 10-28 #244; tasks resolve themselves 10-28 #284; a goal's passed check reads `checked` 10-30 #379 | See at a glance what needs you, is working, or is done | ? |
| Lead and task roles as editable files (`~/.agents/agents/lead.md`, `task.md`) | shipped | added 10-28 #240; leads and tasks run as any profile, profiles extend others 10-29 #296; an upgrade offers a merge with an edited role 10-29 #357 | The person owns the roles; Sonny never overwrites their edits | ? |
| How leads work: act across tasks, care about the person, carry what one task lacks to whoever has it, react to the person's words in a task, pick task models | shipped | added 10-27 #229; routing 10-29 #295; reaction 10-29 #330, #340; models from the list 10-30 #385; asked why it starts another task 10-30 #386 | Leads that coordinate instead of only relaying | ? |
| Straight feedback, coaching repeated habits, weighing what sources report | experimental | added 10-30 #394 | Agents that flag what the person missed and say what they see, unsugared | ? |
| Sandbox for leads and tasks by default; trusted commands run outside it | shipped | added 10-28 #285; allow rules 10-29 #309 | A secure default, as Codex does, with ways out when blocked | ? |
| `sonny agent ARGS`, the one way agents reach the daemon | shipped | added 10-27 #199, #214 | One documented path instead of `$AGENT_BIN` or a bare `agent` | ? |
| Agent actions as declared tools instead of `sonny X` commands | shipped | six commands 10-28 #268; memory and triggers 10-28 #278; current tools given to agents 10-29 #382 | First-class tools the daemon knows, not shell commands | ? |
| SandFS task copies (`/copies`, `--copies`, `sonny task changes`) | removed | added 10-26 #142, #162; another disk 10-27 #184; sandfs 0.9 10-28 #271; removed 10-28 #301 | Added to give tasks copies outside git; removed because it weighed too much and worktrees plus the sandbox cover it | tried; better without |

## Laying work out

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| Rex: tasks in Rex windows, one Rex home per project | shipped | added 10-26 #146; one home per project 10-26 #160; multiplexer interface 10-26 #166 | Watch and click into every agent from the multiplexer | ? |
| Herdr: the same layout in Herdr, Sonny as a Herdr harness | shipped | added 10-26 #167; as Herdr's own agents 10-27 #178; survives Herdr restarts 10-28 #231 | George's other multiplexer | ? |
| cmux: the same layout in cmux, Automation mode for agents | experimental | added 10-29 #300, #323 | A third multiplexer to try | ? |
| Sessions for leads, tabs for tasks, a split only to show something now (`sonny run`, `sonny show`) | shipped | `sonny run` renamed `sonny split` 10-26 #165; sessions and tabs 10-27 #200; no programs started while a window idles 10-29 #311 | Tabs are a job stream to click into; a split is for attention now | ? |

## The conversation

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| Full screen, Codex-like | shipped | added 10-28 #235 behind `/fullscreen`; default 10-28 #255; 100-column conversation 10-28 #264; only mode 10-29 #326 | Room for the pane, folds, buttons and drag-to-copy | ? |
| Inline mode (Sonny drawn into the terminal's scrollback) | removed | added 10-26 #146; removed 10-29 #326 | The first view; full screen became the experience, and one view is less to keep | ? |
| Clean view; `/verbose` shows every tool call | shipped | quiet steps 10-27 #188; clean view 10-27 #221 | Show what moves the work on, hide ordinary calls | ? |
| Folds: an agent call is one row, agent messages and task updates are quiet rows, started tasks a live roster | shipped | task updates as one row 10-26 #177; rows, roster 10-29 #326; one row per call 10-29 #369; updates gather into one row 10-30 #384 | Less noise between the person's words and the replies | ? |
| Staged messages, the open line while a reply streams, turn timers | shipped | streaming 10-26 #172; staged 10-27 #175; timers 10-28 #266 | Show what is in flight | ? |
| Footer: context size, branch, ↻ background count, agents at work | shipped | context 10-26 #163; branch 10-27 #182; ↻ 10-28 #267; agents at work 10-29 #367 | One line of state under the composer | ? |
| Colour: 16 terminal colours, one meaning each | shipped | added 10-29 #326 | Colour that carries meaning, in any theme | ? |
| Links and file paths clickable | shipped | a path opens from the folder that named it 10-10 #98; web addresses 10-27 #179; link text, paths 10-27 #230 | Open what an agent names | ? |
| Find (Ctrl+F) and long histories (older pages on scroll, Home to the first turn) | shipped | Ctrl+F 10-28 #258, 10-29 #343; pages 10-29 #331; lay out near the view 10-29 #353; Home 10-29 #364 | Long conversations stay fast and searchable | ? |
| One Markdown renderer for every surface; code coloured with Oniguruma | shipped | renderer 10-29 #325; Oniguruma 10-29 #336 | Same rendering everywhere, three times faster colouring | ? |
| Pictures: SVG, Mermaid, math and HTML drawn in the terminal | shipped | added 10-26 #147; `\[ \]` math 10-29 #313; sent once 10-29 #338 | Agents can show, not only tell | ? |
| Reactions, on the message they answer (`/react`) | shipped | added 10-27 #224; under the message 10-29 #326 | A light acknowledgement without a reply | ? |
| Welcome and first run: sets itself up and says hello | shipped | added 10-28 #253; heads every lead's history 10-29 #320 | A new person gets going without docs | ? |
| `sonny view`: long writing beside the lead | removed | added 10-27 #197; reverted 10-27 #198 | Viewers are skills, not Sonny; long writing stays in a file | ? |

## Composer

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| Editing: wrap, arrow keys, shell keys, Shift-Enter, mouse selection | shipped | wrap 10-26 #157; arrows 10-27 #174; shell keys and pictures in messages 10-28 #242; Shift-Enter, selection 10-29 #297 | A composer that edits like a shell | ? |
| `!COMMAND`: run your own shell command and tell the agent | shipped | added 10-27 #209 | Do it yourself without leaving the conversation | ? |
| A guess at your next message, Tab takes it (`[sonny] suggest`) | experimental | added 10-29 #329 | As Claude Code and Codex do; cost per guess not yet measured | ? |

## Side pane

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| The pane beside the conversation: files, `/status`, `/background`, tasks; back and forward | shipped | added 10-28 #264; `/background` 10-28 #283; tasks in the pane 10-29 #326; `/status` opens the agent clicked 10-30 #384 | Look at something without losing the conversation | ? |
| Pictures, HTML and diagrams in the pane; a folder as a tree | shipped | drawing 10-29 #303; tree 10-29 #319 | Read what agents made, in place | ? |
| `/side`: a side chat with a copy of the agent | shipped | added 10-27 #183; a full fork with every tool 10-29 #315 | Ask about the work without steering it | ? |
| Margin: notes agents keep beside the conversation (`/margin`) | experimental | added 10-29 #308; a task's notes in the lead's margin 10-29 #367; replay 10-30 #383; opens on its first note 10-30 #384 | A place for state that outlives one reply | ? |

## What agents get from Sonny

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| `ask`: a question for the person, as a decision card with reasons | shipped | pinned questions above the composer 10-27 #192; declared tool 10-27 #225; decision card where it was asked 10-28 #245; options are buttons 10-29 #326 | Agents ask instead of guessing; the person answers in one click | ? |
| `plan`: the agent's plan, drawn in the conversation | shipped | added 10-27 #225 | See the steps an agent intends | ? |
| Memory (`sonny memory`, the memory skills, nightly cleanup) | shipped | app memory 10-09 #82, 10-10 #104, #120; moved into Sonny 10-27 #189; a tool 10-28 #278; 16 KiB 10-28 #241; read tracking, 30-day unread check 10-29 #354; outside-content marks 10-29 #356 | Later agents start knowing what earlier ones learned | ? |
| Triggers (`sonny trigger`): wake an agent at a time, on a file, a commit, or by name | shipped | app triggers 10-10 #89-#92; moved into Sonny 10-27 #189; skipped checks, `--replace` 10-28 #241; a tool 10-28 #278; Linux via systemd 10-29 #344 | Work that starts while nobody is at the terminal | ? |
| Turn-end triggers | removed | added 10-10 #89-#92; removed 10-27 #189 | Leads hear of task turns from task updates and the lookout | ? |
| Goals: keep an agent working until a check command passes | experimental | added 10-29 #341 | Autonomy with a check, not a model's say-so | ? |
| MCP servers through one `exec` script tool (`/mcp`, `sonny mcp tool`) | experimental | added 10-29 #321; saved files kept a week 10-29 #327; four scripts at once 10-29 #334 | The person's MCP servers without loading every tool's schema | ? |
| Skills Sonny installs (automation, memory, plan, pictures, multiplexer skills) | shipped | moved into Sonny 10-27 #189; monitors 10-27 #206; an edited skill is never overwritten 10-29 #357 | Teach agents Sonny's features as normal skills | ? |

## Lookout and background

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| Lookout: tells a lead what its tasks did | shipped | added 10-26 #154, #161; one per daemon 10-27 #180; subscriptions 10-27 #195; subscribers only, leads subscribed 10-27 #212; news within a minute 10-27 #228, #232; a companion of the daemon 10-29 #370; a turn's end carries its reply 10-30 #378 | A lead can rest while tasks work and hear the moment they need it | ? |
| Lookout judge: a model decides what interrupts the lead (drift, interruptions, look-fors) | removed | added 10-26 #154; judge forks expire 10-27 #203; drift check 10-27 #204; interruptions only 10-27 #215; one judge with the approver 10-27 #217; picks 10-28 #239, #273; removed 10-29 #310 | Removed because the checks felt ad hoc and did not scale; the lookout now forwards everything and only picks the timing | ? |
| Lookout sources: pollers the person sets up feed news to agents | experimental | added 10-29 #305 | Agents hear about the world, not only their tasks | ? |
| `/background`: triggers and lookout subscriptions, fire or remove | shipped | added 10-28 #267; in the pane 10-28 #283 | See and control what runs while nobody watches | ? |

## Approvals

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| `/approvals`, auto-review shown in progress, `/approve` retries a denial | shipped | added 10-28 #261 | Approvals stay visible and easy to override | ? |
| Auto approver (a model judges gated calls) | shipped | added 09-27 #47; permissive 10-27 #176; Codex's guardian policy 10-28 #237; sees history 10-28 #243; sandbox escapes only 10-28 #275 | Few prompts, stop only likely unasked danger | ? |
| OpenAI Decisions judge for the approver | removed | added 10-25 #141; removed 10-30 #389 | Removed as unused, with no clear gain over the other judges; may come back when the API matures | no |

## Settings and install

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| `/config`: the settings in `~/.agent/config.toml`; `/sound` chime | shipped | never overwrite the person's settings 10-26 #149; effort defaults 10-28 #251; `/config`, chime 10-29 #315 | One settings file, editable in Sonny | ? |
| `/usage`: tokens spent over time, by agent and model | shipped | added 10-29 #302 | See where the tokens go | ? |
| Homebrew and Linux installs | shipped | formula 10-26 #148; Linux builds 10-27 #191; source formulas 10-27 #196; install script 10-28 #253 | Install with one command | ? |

## Other clients

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| Sonny in T3 Code (`sonny acp`) | experimental | added 10-29 #323; reads blocks 10-29 #337; model picker 10-29 #339; tasks as child threads 10-29 #352; model `id` 10-29 #358; effort picker 10-29 #363; provider names 10-30 #377 | Sonny's leads and tasks from a GUI | ? |
| Errand support (`/status`'s On Errand, the errand skill) | removed | added 10-26 #146; removed 10-29 #314 | George doesn't plan to use Errand from Sonny | no plans to |

## Before Sonny

Sonny took these over from the Agent app, or dropped them with it.

| Feature | Status | History | Why | Used? |
|---|---|---|---|---|
| The Agent app (desktop) | removed | Home 10-10 #109; rendering 10-10 #81; removed 10-27 #213 | Sonny stopped depending on it, so it went | ? |
| Swarms: a board agents post to | removed | added 09-28 #60; as a skill 10-10 #112; removed 10-27 #213 | Parked as no priority, dropped with the app | ? |
