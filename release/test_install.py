"""Exercise the Linux installer against a local release with no package changes."""
import hashlib
import io
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile
import unittest

INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"


@unittest.skipUnless(sys.platform.startswith("linux"), "exercises Linux rename semantics")
class RepairTests(unittest.TestCase):
    def test_repair_survives_interruption_and_can_be_retried(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            prefix = root / "prefix"
            tools = root / "tools"
            tools.mkdir()
            arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(platform.machine(), platform.machine())
            name = f"sonny-1.2.3-linux-{arch}"
            archive = root / f"{name}.tar.gz"

            def bundle(label, status=0):
                with tarfile.open(archive, "w:gz") as tar:
                    for program in ("bin/sonny", "libexec/agent"):
                        data = f"#!/bin/sh\necho {label}\nexit {status}\n".encode()
                        info = tarfile.TarInfo(f"{name}/{program}")
                        info.size, info.mode = len(data), 0o755
                        tar.addfile(info, io.BytesIO(data))
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                Path(str(archive) + ".sha256").write_text(f"{digest}  {archive.name}\n")

            def stub(name, script):
                path = tools / name
                path.write_text("#!/bin/sh\n" + script)
                path.chmod(0o755)

            stub("curl", 'cp "$RELEASE/${4##*/}" "$3"\n')
            stub("gh", "exit 1\n")
            # One repair forces libc, the later retry uses the host implementation.
            stub("mv", 'case "$1:$2" in --exchange:*) [ -z "$LIBC_SWAP" ] || exit 1;; '
                 '-Tf:*/current) [ -z "$INTERRUPT" ] || exit 77;; esac\nexec /bin/mv "$@"\n')
            env = dict(os.environ, PREFIX=str(prefix), RELEASE=str(root),
                       PATH=f"{tools}:/usr/bin:/bin", SONNY_VERSION="1.2.3", SONNY_PACKAGES="0",
                       INTERRUPT="", LIBC_SWAP="")

            def run(**overrides):
                return subprocess.run(["sh", INSTALLER], env=env | overrides,
                                      stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

            def installed():
                return subprocess.check_output([prefix / "bin/sonny", "--version"], text=True).strip()

            bundle("old")
            fresh = run()
            self.assertEqual(fresh.returncode, 0, fresh.stdout)
            bundle("broken", status=1)
            self.assertNotEqual(run().returncode, 0)
            self.assertEqual(installed(), "old")
            bundle("repaired")
            self.assertNotEqual(run(INTERRUPT="1", LIBC_SWAP="1").returncode, 0)
            self.assertEqual(installed(), "repaired")
            agent = prefix / "opt" / name / "libexec/agent"
            agent.unlink()
            result = run()
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertEqual(subprocess.check_output([agent], text=True).strip(), "repaired")
            self.assertEqual(installed(), "repaired")
