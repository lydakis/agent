"""Write a release's version into the workspace's manifests and Cargo.lock,
in the build tree only. Main always says 0.0.0, as Codex's does; the
release workflow stamps the tag's version before it tests and builds, so
`agent --version` and `sonny --version` name the release, and a build from
main never passes for one."""

import argparse
from pathlib import Path
import re


# Main's version: every workspace package carries it until a release
# stamps its own.
MAIN = "0.0.0"

VERSION = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?"

# Each workspace package of ours, by its manifest.
PACKAGES = {
    "Cargo.toml": "agent-runtime",
    "client/Cargo.toml": "agent-client",
    "sonny/Cargo.toml": "sonny",
}


def stamp_manifest(text: str, name: str, version: str) -> str:
    """The manifest with its `[package]` version `version`; it must say
    main's version now."""
    package = re.search(r"^\[package\]\n(.*?)(?=^\[|\Z)", text, re.MULTILINE | re.DOTALL)
    if package is None:
        raise ValueError(f"{name}: no [package] table")
    named = re.search(r'^name = "([^"]*)"$', package[1], re.MULTILINE)
    if named is None or named[1] != name:
        raise ValueError(f"{name}: [package] names another package")
    line = re.search(r'^version = "([^"]*)"$', package[1], re.MULTILINE)
    if line is None:
        raise ValueError(f"{name}: [package] has no version")
    if line[1] != MAIN:
        raise ValueError(f"{name} has version {line[1]}, not main's {MAIN}")
    at = package.start(1) + line.start(1)
    return text[:at] + version + text[at + len(line[1]):]


def stamp_lock(text: str, version: str) -> str:
    """Cargo.lock with each of our packages' entries at `version`."""
    for name in PACKAGES.values():
        entry = f'[[package]]\nname = "{name}"\nversion = "{MAIN}"\n'
        if text.count(entry) != 1:
            raise ValueError(f"Cargo.lock: expected one {name} {MAIN}")
        text = text.replace(entry, f'[[package]]\nname = "{name}"\nversion = "{version}"\n')
    return text


def stamp(root: Path, version: str):
    if not re.fullmatch(VERSION, version) or version == MAIN:
        raise ValueError("expected a release version such as 0.1.0 or 0.1.0-rc.1")
    # Everything is read and checked before anything is written.
    stamped = {}
    for manifest, name in PACKAGES.items():
        stamped[manifest] = stamp_manifest((root / manifest).read_text(), name, version)
    stamped["Cargo.lock"] = stamp_lock((root / "Cargo.lock").read_text(), version)
    for path, text in stamped.items():
        (root / path).write_text(text)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="the release's version, its tag without the v")
    parser.add_argument("--root", type=Path, default=Path("."))
    args = parser.parse_args()
    try:
        stamp(args.root, args.version)
    except (ValueError, OSError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
