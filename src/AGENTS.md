# The daemon and the CLI

Both are mechanism; clients such as Sonny hold every opinion. Before a change
here, check it against these, and discuss any daemon change with George first:

- Offer a mechanism the client fills in, never a policy. A default is fine
  only where a caller can change it per bot or per call.
- Name no client, terminal or tool it serves (Sonny, Herdr, cmux, git, `.agents`).
  A client passes such knowledge in. `tests/daemon_knows_no_client.rs` checks names.
- Write nothing to a model beyond facts and structure: no advice, no style,
  no CLI flags in tool descriptions or notes. Model-facing text is the client's.
- No fixed limit that decides how a bot works (rounds, what counts as a
  secret, what to keep); capacity bounds that protect the daemon are fine.
- Known exceptions, chosen: the line after a prompt (`trailer`), provider-key
  stripping and redaction, Codex's `[shell_environment_policy]`.

## Words

The daemon is a runtime others build on, with a vocabulary of its own; a
client such as Sonny keeps its own words for its people. Each word here
names one mechanism. Check a new protocol name, error code or doc line
against this list: use the word that is here, add one only for a new
mechanism, and borrow a client's word only where it means the same thing.

- **bot**: a named, durable identity the daemon runs: instructions, model,
  tools, workspace and history. Never "agent" in the protocol (Sonny calls
  its bots agents).
- **turn**: one prompt's run, from accepted to finished, in model **rounds**
  (one provider call and the tool calls it plans).
- **prompt**: what starts or joins a turn. Its **delivery** is `reject`,
  `queue`, `steer` or `attach`. Its **origin** is what sent it when no
  bot's turn did, as the client names itself; `from` is the bot turn that
  wrote it; `person` is who wrote it, as the client labels them.
- **fork**: a bot made as a copy of another's history. One with
  `expire_ms` is removed once idle that long.
- **gate**: tools (`approve`) whose calls wait on an answer before they
  run, and the **tag** that names who answers (`approver`).
- **approver**: whatever answers a tag: a person through a client (the
  CLI's `manual`), or a program that serves it (the CLI's `auto`, which
  Sonny serves with its reviewer model). The daemon knows tags, never what
  is behind one.
- **answer**: an approver's **decision** on one call: `allow`, `deny` or
  `ask` (leave it open for a person), with a reason and `by`, a label.
  Prose may say verdict; protocol names say decision.
- **serve** / **lease**: a session takes a tag's calls with
  `serve_approvals` and keeps them while it renews the lease.
- **park**: a turn that waits (a gate, `wait`, a closed pool, a backoff)
  held as a store row, with no task, slot or timer of its own.
- **pool** / **pacer**: a provider's shared capacity, and what learns its
  limits from what the provider says.
- **companion**: a command the daemon keeps running for as long as it
  runs, a client's helper; the daemon knows nothing of what it does.
- **event**: what the daemon reports of a bot as it happens, read with
  `events` or `follow`.
