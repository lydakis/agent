Feature: `/permissions` (was `/approvals`)
`~/.agent/env` sets `AGENT_JUDGE`, which nothing has read since 0.8.0: a reviewer model chosen there was never used, and the agent's own model reviewed in its place. That file holds keys only; the reviewer's model is `[judge] model` in `~/.agent/config.toml` (`effort` beside it).

Tell the person what `AGENT_JUDGE` named and ask whether that model should review their Auto calls. If yes, set `[judge] model = "PROVIDER/MODEL"` in `~/.agent/config.toml` (ask before writing it). Either way, remove the `AGENT_JUDGE` line from `~/.agent/env` with their OK; that file is outside your sandbox, so ask them to remove it, or run the edit through a command they approve.
