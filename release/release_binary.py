#!/usr/bin/env python3
"""Fetch a published release's Linux daemon, checked against its checksum.

    release_binary.py TAG [TAG ...] --into DIR

writes DIR/TAG/agent for each tag: the `agent` from that release's static
Linux build of Sonny for this machine's architecture, after its tarball
matches the `.sha256` published beside it. Prints each path.
"""

import hashlib
import io
import platform
import re
import stat
import sys
import tarfile
import time
import urllib.request
from pathlib import Path

REPOSITORY = "https://github.com/lydakis/agent/releases/download"
ARCHITECTURES = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}


def download(url, attempts=4):
    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(url, timeout=60) as response:
                return response.read()
        except OSError as error:
            if attempt == attempts - 1:
                raise SystemExit(f"could not download {url}: {error}")
            time.sleep(2 ** attempt * 5)


def fetch(tag, into):
    """The path of `tag`'s daemon under `into`, downloaded once."""
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise SystemExit(f"expected a release tag such as v0.10.2, not {tag!r}")
    target = Path(into) / tag / "agent"
    if target.is_file():
        return target
    machine = ARCHITECTURES.get(platform.machine().lower())
    if sys.platform != "linux" or machine is None:
        raise SystemExit("releases publish Linux x86_64 and aarch64 builds only; pass --binary")
    name = f"sonny-{tag[1:]}-linux-{machine}"
    tarball = download(f"{REPOSITORY}/{tag}/{name}.tar.gz")
    expected = download(f"{REPOSITORY}/{tag}/{name}.tar.gz.sha256").split()[0].decode()
    if hashlib.sha256(tarball).hexdigest() != expected:
        raise SystemExit(f"{name}.tar.gz does not match its published checksum")
    with tarfile.open(fileobj=io.BytesIO(tarball)) as archive:
        member = archive.getmember(f"{name}/libexec/agent")
        data = archive.extractfile(member).read()
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(data)
    target.chmod(target.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return target


def main(argv):
    if "--into" not in argv or argv.index("--into") != len(argv) - 2 or len(argv) < 3:
        raise SystemExit(__doc__)
    into = argv[-1]
    for tag in argv[:-2]:
        print(fetch(tag, into))


if __name__ == "__main__":
    main(sys.argv[1:])
