"""What agent-browser or agent-desktop documents: every command and flag
its help and skill name, one per line, so that a release which adds or
drops one shows as a line added or taken away.

    python3 sonny/upstream/inventory.py list agent-desktop PROGRAM > list.txt
    python3 sonny/upstream/inventory.py news agent-desktop PROGRAM list.txt

PROGRAM is the native program, the npm package's bin/agent-*-linux-x64:
the lists in this folder are made on Linux, since a help can differ by
operating system. A line is `command NAME` (or `command NAME SUB` for
agent-desktop's subcommands), `NAME FLAG` for a flag of that command, or
`global FLAG` for one every command takes. What the help and skill name is
what is listed, since a flag that neither names is one no agent is taught.
`news` prints, as an issue's body, what PROGRAM documents that the list
does not and what the list has that it no longer documents; nothing when
they agree.
"""

import re
import subprocess
import sys

LONG = re.compile(r"(?<![\w-])(--[a-zA-Z][\w-]*)")
SHORT = re.compile(r"(?:^\s+|,\s*)(-[a-zA-Z])(?=[\s,]|$)")
# An option's own line: its flags first, as `-i, --interactive <x>`.
OPTION = re.compile(r"^\s+(-{1,2}[a-zA-Z][\w-]*(?:,\s*-{1,2}[a-zA-Z][\w-]*)*)")
QUOTED = re.compile(r"\"[^\"]*\"|'[^']*'")


def run(program, *args):
    return subprocess.run(
        [program, *args],
        capture_output=True,
        text=True,
        timeout=60,
        stdin=subprocess.DEVNULL,
        cwd="/",
    ).stdout


def sections(text, heads=None):
    """`Name:` headed blocks of a help text, as {name: [line, ...]}, with
    what follows each name on its own line put in `heads`."""
    found, name = {}, ""
    for line in text.splitlines():
        head = re.match(r"^([A-Z][\w ()`/-]*):(.*)$", line)
        if head:
            name = head.group(1)
            found.setdefault(name, [])
            if head.group(2).strip():
                found[name].append(head.group(2))
                if heads is not None:
                    heads[name] = head.group(2)
            continue
        found.setdefault(name, []).append(line)
    return found


def options(lines):
    """The flags options' own lines start with, not those their
    descriptions mention."""
    out = set()
    for line in lines:
        own = OPTION.match(line)
        if own:
            out.update(re.split(r",\s*", own.group(1)))
    return out


def flags(lines):
    """Every flag lines name, but in quotes, where they are values."""
    out = set()
    for line in lines:
        line = QUOTED.sub("", line)
        out.update(LONG.findall(line))
        out.update(SHORT.findall(line))
    return out


def desktop(program):
    top = sections(run(program, "--help"))
    globals_ = options(top.get("Options", []))
    lines = {f"global {flag}" for flag in globals_}
    for line in top.get("Commands", []):
        name = line.split()[0] if line.startswith("  ") and line.split() else "help"
        if name == "help":
            continue
        lines.add(f"command {name}")
        own = sections(run(program, name, "--help"))
        for flag in options(own.get("Options", [])) - globals_:
            lines.add(f"{name} {flag}")
        for sub in own.get("Commands", []):
            words = sub.split()
            if sub.startswith("  ") and words and words[0] != "help":
                lines.add(f"command {name} {words[0]}")
    return lines


# Sections of agent-browser's help that name no command or flag of the
# command it is for.
BROWSER_ASIDE = ("Global Options", "Examples", "Environment", "Install")
# Sections of its whole help whose lines are no commands.
BROWSER_NOT_COMMANDS = BROWSER_ASIDE + (
    "Options",
    "Authentication",
    "Snapshot Options",
    "Configuration",
    "Command Chaining",
    "iOS Simulator",
)
# A line that runs it: global flags, each with its value if it has one,
# then the command and its words.
RUNS = re.compile(
    r"^\s*(?:\$ )?agent-browser(?: +-{1,2}[\w-]+(?: +(?:\"[^\"]*\"|[^\s-]\S*))?)* +([a-z][a-z0-9]*)(.*)"
)


def browser_help(program, name):
    """The help agent-browser gives for NAME when it is that of a command,
    or of the command NAME is another name for: for any other word, it
    gives its whole help again."""
    text = run(program, name, "--help").strip()
    first = text.splitlines()[0] if text else ""
    whole = " - fast " in first
    return text if first.startswith("agent-browser ") and not whole else None


def browser(program):
    text = run(program, "--help")
    skill = run(program, "skills", "get", "core", "--full")
    heads = {}
    top = sections(text, heads)
    globals_ = set()
    for name in ("Options", "Authentication"):
        globals_ |= options(top.get(name, []))
    lines = {f"global {flag}" for flag in globals_}
    # Its commands are the first words of its whole help's lines, but where
    # a section is one command's (`Mouse:  agent-browser mouse <action>`,
    # whose lines are that command's words), and words with a help of their
    # own: those a line runs it with in its help and skill (`agent-browser
    # -p ios tap @e1`), and those in backticks (`goto`, another name).
    names, shown = set(), {}
    for section, body in top.items():
        if section in BROWSER_NOT_COMMANDS or section.startswith("Start here"):
            continue
        owner = re.search(r"agent-browser ([a-z][a-z0-9]*)", heads.get(section, ""))
        if owner:
            names.add(owner.group(1))
            shown.setdefault(owner.group(1), set()).update(flags(body))
            continue
        for line in body:
            word = re.match(r"^  ([a-z][a-z0-9]*)(\s.*|$)", line)
            if word:
                names.add(word.group(1))
                shown.setdefault(word.group(1), set()).update(flags([word.group(2)]))
    # The flags a line runs a command with are that command's too, since a
    # command's own help can leave some out.
    for line in (text + skill).splitlines():
        runs = RUNS.match(line)
        if runs:
            shown.setdefault(runs.group(1), set()).update(flags([runs.group(2)]))
    words = names | set(shown) | set(re.findall(r"`([a-z][a-z0-9]*)`", text + skill))
    seen = set()
    while words - seen:
        name = min(words - seen)
        seen.add(name)
        help_ = browser_help(program, name)
        if help_ is None and name not in names:
            continue
        lines.add(f"command {name}")
        words |= set(re.findall(r"`([a-z][a-z0-9]*)`", help_ or ""))
        # Another name for a command (`goto` for `open`) has its flags.
        if help_ and help_.split()[1] != name:
            continue
        # Of its own help: its options, its usage, and its operations'
        # lines, which are indented where what it says of them is not.
        mine = set(shown.get(name, ()))
        own_heads = {}
        for section, body in sections(help_ or "", own_heads).items():
            if section in BROWSER_ASIDE:
                continue
            if "Options" in section:
                mine |= options(body)
            else:
                mine |= flags([own_heads.get(section, "")])
                mine |= flags(line for line in body if line.startswith(" "))
        for flag in mine - globals_:
            lines.add(f"{name} {flag}")
    return lines


LISTS = {"agent-desktop": desktop, "agent-browser": browser}


def listed(path):
    with open(path) as f:
        return {line for line in f.read().splitlines() if line and not line.startswith("#")}


def said(program, line):
    """The line of its help that says what a line of a list is."""
    words = line.split()
    if words[0] == "command":
        own = browser_help(program, words[1]) if len(words) == 2 else None
        own = own or run(program, *words[1:], "--help")
        if own.lstrip().startswith("agent-browser - fast"):
            own = ""
        for text in own.splitlines() + run(program, "--help").splitlines():
            if text.strip() and (own or text.startswith(f"  {words[-1]} ")):
                return text.strip()
        return ""
    command = [] if words[0] == "global" else words[:1]
    flag = re.escape(words[-1])
    for text in run(program, *command, "--help").splitlines() + run(program, "--help").splitlines():
        if re.search(rf"(?<![\w-]){flag}(?![\w-])", text):
            return text.strip()
    return ""


def news(program, path, new):
    old = listed(path)
    added, dropped = sorted(new - old), sorted(old - new)
    if not added and not dropped:
        return ""
    version = run(program, "--version").strip()
    with open(path) as f:
        checked = f.readline().lstrip("# ").strip()
    out = []
    if added:
        out.append(f"New in {version} since {checked}, which Sonny's tool was checked against:\n")
        out.extend(f"- `{line}`: {said(program, line)}" for line in added)
    if dropped:
        out.append(f"\nIn {checked} and no longer documented in {version}:\n")
        out.extend(f"- `{line}`" for line in dropped)
    return "\n".join(out) + "\n"


def main():
    what, which, program = sys.argv[1:4]
    lines = LISTS[which](program)
    if what == "list":
        print(f"# {run(program, '--version').strip()}")
        print("\n".join(sorted(lines)))
    elif what == "news":
        print(news(program, sys.argv[4], lines), end="")
    else:
        sys.exit(f"{what}: list or news")


if __name__ == "__main__":
    main()
