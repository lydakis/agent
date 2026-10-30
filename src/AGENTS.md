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
- Known exceptions, chosen: the sender line (`hide_senders`), provider-key
  stripping and redaction, Codex's `[shell_environment_policy]`.
