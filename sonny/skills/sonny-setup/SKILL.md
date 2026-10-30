---
name: sonny-setup
description: Set up or change how Sonny and its agent daemon reach a model, bring in MCP servers from other agents, and check the result. Use when asked to set up Sonny, or when Sonny's setup needs finishing.
---

# Setting up Sonny

Sonny keeps its settings in `~/.agent/config.toml` (the provider, the
default model, efforts, MCP servers) and keys in `~/.agent/env`, one
`NAME=value` a line, which only the person can read; Sonny applies it to
the daemon and everything it starts. `sonny setup` with flags changes both
and asks nothing; with none, it asks the person in a terminal.

## 1. See what the machine has

`sonny setup --json` prints:

- `provider`, `model`: what is set now. A provider set means agents can
  reach a model.
- `ways`: each way to reach one. ChatGPT (`signed_in`, and
  `codex_signed_in` when Codex uses a ChatGPT plan here), Bedrock with the
  AWS `profiles` found, and each provider's key `set` or not.
- `agents`: the other coding agents on `PATH`.
- `mcp_to_import`: MCP servers other agents have that Sonny does not.
- `missing`: tools agents need, with the command that installs each.
- `steps`: setup an upgrade offers, each `done` or not.

## 2. Reach a model

Ask the person which way they want when more than one is ready. Then:

- ChatGPT plan: `sonny agent login chatgpt` signs in through the browser,
  then `sonny setup --provider chatgpt`.
- A key: `sonny setup --provider anthropic` (or `openai`, `openrouter`)
  uses the key set in the shell or `~/.agent/env`. A new key goes in on
  stdin, never as an argument: the person runs
  `sonny setup --provider openai --key-stdin` and pastes it. Never read a
  key out of another agent's files, or write one anywhere else.
- Bedrock: `sonny setup --provider bedrock --profile NAME`. An SSO profile
  whose session expired needs `aws sso login --profile NAME` first.

If the daemon was running, it keeps its old provider until
`sonny agent shutdown` stops it; Sonny starts it again.

## 3. The model

`sonny setup --model PROVIDER/MODEL` sets the model new agents start with.
Left unset, Sonny lists the provider's models, best first, the next time
it starts. `sonny agent models` lists them now.

## 4. MCP servers

`sonny mcp import --json` shows what other agents have.
`sonny setup --import-mcp` brings them all, or
`--import-mcp codex,claude` those agents' alone; a secret in one moves to
`~/.agent/env`. Servers from files in the folder are left out: the person
ticks those in `sonny mcp import` if they trust them. `sonny mcp list`
starts each and says which work.

## 5. Check

`sonny setup --json` shows the provider and model, `missing` is empty or
the person knows what to install, and `sonny mcp list` lists the servers.
Tell the person what you set and where; if you have a memory, note what
they chose and why (which AWS profile is for what, say), so the next setup
need not ask.
