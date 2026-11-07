Feature: Automations
An automation's `if` command that an agent added now runs in that agent's own sandbox, with its variables and its own daemon socket, as its other commands do. One added before this upgrade still runs as the person until it is added again.

List your automations. Add each one whose `if` you or an agent of yours wrote again, with `replace` and the same fields, from your own automations tool, so its `if` runs in your sandbox. Leave one the person wrote themselves as it is. When an `if` needs something your sandbox does not reach, such as a folder outside it, tell the person what it needs.
