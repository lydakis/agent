Feature: Automations
An automation's `if` command that an agent added now runs in that agent's own sandbox, with its variables and its own daemon socket, as its other commands do. One added before this upgrade does not run at all, since Sonny did not record who wrote it: until it is added again, each of its checks fails, and the automation sends nothing.

List your automations. Add each one that has an `if` again, with `replace` and the same fields, from your own automations tool, so its `if` runs in your sandbox. When the person wrote an `if` themselves, or it needs something your sandbox does not reach, such as a folder outside it, tell the person which automation it is: they can add it again from their own terminal with `sonny automation add --replace`, which runs its `if` as them.
