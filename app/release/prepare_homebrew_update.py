"""Verify a published stable release and prepare a Homebrew Contents API update."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

from homebrew_cask import archive_name, render_cask


STABLE_VERSION = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"


def prepare_update(tag: str, metadata: Path, assets: Path, current: Path,
                   *, commit: str, generator: Path | None = None):
    match = re.fullmatch("v" + STABLE_VERSION, tag)
    if not match:
        raise ValueError("expected a stable tag such as v0.1.0")
    version = tag[1:]
    release = json.loads(metadata.read_text())
    if (release.get("tag_name") != tag or release.get("draft") is not False
            or release.get("prerelease") is not False or not release.get("published_at")):
        raise ValueError("release must match the tag and be published, stable, and not a draft")

    checksums = {}
    for line in (assets / "checksums.txt").read_text().splitlines():
        entry = re.fullmatch(r"([0-9a-f]{64}) [ *](\S+)", line)
        if not entry or entry[2] in checksums:
            raise ValueError("malformed or duplicate checksum entry")
        checksums[entry[2]] = entry[1]
    archive = assets / archive_name(version)
    cask_path = assets / "agent.rb"
    source = assets / "source-commit.txt"
    for path in [archive, cask_path, source]:
        if checksums.get(path.name) != hashlib.sha256(path.read_bytes()).hexdigest():
            raise ValueError(f"checksum mismatch or missing checksum for {path.name}")
    # A draft's tag can move after its assets were built: publish only what
    # the tag's current commit built.
    if not re.fullmatch(r"[0-9a-f]{40}", commit) or source.read_text() != commit + "\n":
        raise ValueError("release assets were not built from the tag's commit")
    cask = render_cask(version, archive).encode()
    expected_release_cask = cask
    if generator is not None:
        with tempfile.TemporaryDirectory() as directory:
            expected = Path(directory) / "agent.rb"
            subprocess.run([sys.executable, str(generator), version,
                            str(archive), str(expected)], check=True)
            expected_release_cask = expected.read_bytes()
    if cask_path.read_bytes() != expected_release_cask:
        raise ValueError("release cask does not match the generated cask for this archive")

    request = {
        "message": f"Update agent to {version}",
        "branch": "main",
        "content": base64.b64encode(cask).decode(),
    }
    if current.exists():
        previous = current.read_bytes()
        versions = re.findall(r'^  version "(' + STABLE_VERSION + r')"$',
                              previous.decode(), re.MULTILINE)
        if len(versions) != 1:
            raise ValueError("current cask must have one stable version")
        previous_version = tuple(map(int, versions[0][1:]))
        next_version = tuple(map(int, match.groups()))
        if previous_version > next_version or previous == cask:
            return None
        if previous_version == next_version:
            raise ValueError("refusing to replace a different cask with the same version")
        # GitHub requires the previous Git blob ID to guard against concurrent edits.
        request["sha"] = hashlib.sha1(
            b"blob " + str(len(previous)).encode() + b"\0" + previous
        ).hexdigest()
    current.parent.mkdir(parents=True, exist_ok=True)
    current.write_bytes(cask)
    return request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("metadata", type=Path)
    parser.add_argument("assets", type=Path)
    parser.add_argument("current", type=Path)
    parser.add_argument("request", type=Path)
    parser.add_argument("--commit", required=True,
                        help="the commit the release tag resolves to now")
    parser.add_argument("--generator", type=Path,
                        help="release tag's generator, used to verify immutable assets")
    args = parser.parse_args()
    args.request.unlink(missing_ok=True)
    try:
        request = prepare_update(args.tag, args.metadata, args.assets, args.current,
                                 commit=args.commit, generator=args.generator)
        if request is not None:
            args.request.write_text(json.dumps(request))
        else:
            print("Tap already contains this version or a newer release; no update needed.")
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
