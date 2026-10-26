---
name: errand
description: Run a command on another machine the person owns (a Mac mini, a Linux box, a GPU machine) with Errand, from this folder, uncommitted edits included. Use it when a command needs hardware, an OS or an architecture this machine lacks, or would tie this machine up for long.
---

# Errand

`errand` sends this folder's files, uncommitted edits included, to a runner,
runs the command there, streams its output back and returns its exit code.
It's optional: run things here as usual, and reach for Errand when the work
needs another machine. A GPU, Linux instead of macOS, another CPU
architecture, or a long build or test run that would keep this machine busy
are all good reasons.

- `errand peers` lists the runners and what they have. `errand ps` lists jobs.
- `errand -- COMMAND ARGS` runs on the default runner and waits for the job.
  Use `--on NAME` for a named runner, `--on local` for a scratch copy on this
  machine, or `--where 'os=linux,gpu'` to let Errand choose by requirement.
- The command can't be interactive. Git-ignored files such as `node_modules`
  or `target` don't travel, so install dependencies within the job:
  `errand -- sh -c 'pnpm install && pnpm test'`. For a Rust or Go build,
  `--cache NAME=PATH` keeps the runner's build cache between jobs.
- For anything longer than a couple of minutes, start it detached:
  `errand -d -- COMMAND` prints a handle (`peer/ULID`) and returns. Check it
  with `errand status --json HANDLE`, and read its output with
  `errand attach --no-follow --tail 200 HANDLE`. Stop it with `errand kill HANDLE`.
- Files the job changed stay on the runner. `errand fetch HANDLE` stages them
  for a look, and `errand fetch --apply HANDLE` merges them into this folder,
  checking for conflicts first. `--apply` on the run itself applies them
  after a clean success. Results that git ignores come back with
  `--artifact PATH`.
- Exit codes are the command's own. 120 means Errand itself failed, or the
  runner stopped answering, so the job's state is unknown: run
  `errand status HANDLE` before running the job again.

Say which runner a result came from when you report it.
