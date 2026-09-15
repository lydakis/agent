# OpenAI adapter live check

Observed 2026-09-14 America/New_York. The existing Rust Responses adapter passed
a paid, bounded check against OpenAI using `gpt-5-mini-2025-08-07`. No SDK was
added. The provider continues to share its HTTP transport and stream native
history items by reference.

The [probe](../examples/openai_smoke.rs) permits at most four requests, each with
`max_output_tokens: 2048`. It uses a synthetic marker and only the native echo
tool. It emits safe status and token counts, not prompts, outputs, or credentials.
It is an explicitly invoked example, never part of automatic test execution.

Result: three requests, one tool call, 427 input tokens, 195 output tokens, and
two retained encrypted reasoning items. The streamed answer included the tool's
result. After reconstructing the native history from serialized items, a further
request recalled that result without another tool call. Every call returned usage.
The safe local capture is `.local/openai-smoke-result.json`.

This tests the real provider adapter and tool executor. It does not test a live
daemon crash, SQLite recovery, historical forks, arbitrary tools, large contexts,
or provider capacity. Those lifecycle behaviors currently have synthetic coverage.

## Adapter changes

- A validated, optional `Provider::with_max_output_tokens` bounds Responses
  generation, including reasoning. Zero and unsupported provider families are
  rejected. The daemon's default remains unchanged; this is not a daemon-wide
  spend limit.
- Requests explicitly include `reasoning.encrypted_content` for compatibility
  with older Responses implementations. Native output items remain intact.
  Current OpenAI documentation says stateless calls return encrypted reasoning
  by default, so omission of `include` is not itself a demonstrated current API bug.

Sources checked on the observation date: [reasoning and stateless continuation](https://developers.openai.com/api/docs/guides/reasoning),
[Responses request parameters](https://developers.openai.com/api/reference/resources/responses/methods/create),
and [GPT-5 mini capabilities and snapshot](https://developers.openai.com/api/docs/models/gpt-5-mini).

## Run deliberately

With `OPENAI_API_KEY` already supplied through the caller's environment:

```sh
CARGO_HOME=.local/cargo cargo run --release --locked --example openai_smoke
```

The runtime does not automatically load env files. Keep credentials out of
tracked files, benchmark captures, tool environments, and remote snapshots.
