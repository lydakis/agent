"""The Linux installer against a local release, with no package changes."""
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


@unittest.skipUnless(sys.platform.startswith("linux"), "the installer is for Linux")
class InstallTests(unittest.TestCase):
    def test_a_release_that_cannot_run_leaves_the_installed_one_and_a_reinstall_replaces_it(self):
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

            for tool, script in [("curl", 'cp "$RELEASE/${4##*/}" "$3"\n'), ("gh", "exit 1\n")]:
                (tools / tool).write_text("#!/bin/sh\n" + script)
                (tools / tool).chmod(0o755)
            env = dict(os.environ, PREFIX=str(prefix), RELEASE=str(root),
                       PATH=f"{tools}:/usr/bin:/bin", SONNY_VERSION="1.2.3", SONNY_PACKAGES="0")

            def run():
                return subprocess.run(["sh", INSTALLER], env=env,
                                      stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

            def installed():
                return subprocess.check_output([prefix / "bin/sonny", "--version"], text=True).strip()

            bundle("old")
            fresh = run()
            self.assertEqual(fresh.returncode, 0, fresh.stdout)
            self.assertEqual(installed(), "old")
            bundle("broken", status=1)
            refused = run()
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn("installed Sonny is kept", refused.stdout)
            self.assertEqual(installed(), "old")
            bundle("repaired")
            again = run()
            self.assertEqual(again.returncode, 0, again.stdout)
            self.assertEqual(installed(), "repaired")
            agent = prefix / "opt" / name / "libexec/agent"
            self.assertEqual(subprocess.check_output([agent], text=True).strip(), "repaired")
            # Nothing staged is left behind.
            self.assertEqual(sorted(p.name for p in (prefix / "opt").iterdir()), sorted([name, "sonny"]))
            self.assertEqual(sorted(p.name for p in (prefix / "bin").iterdir()), ["sonny"])


if __name__ == "__main__":
    unittest.main()
