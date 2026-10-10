import hashlib
from pathlib import Path
import tempfile
import unittest

from homebrew_cask import render_cask


class CaskTest(unittest.TestCase):
    def test_cask_pins_the_archive_and_installs_the_app(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "Agent_0.1.0_universal.zip"
            archive.write_bytes(b"signed app")
            cask = render_cask("0.1.0", archive)
        self.assertIn(f'  sha256 "{hashlib.sha256(b"signed app").hexdigest()}"', cask)
        self.assertIn('  version "0.1.0"', cask)
        self.assertIn("/releases/download/v#{version}/Agent_#{version}_universal.zip", cask)
        self.assertIn('  app "Agent.app"', cask)
        self.assertIn('  binary "#{appdir}/Agent.app/Contents/MacOS/agent"\n', cask)
        # The bundled daemon is stopped before its binary goes.
        self.assertIn('Agent.app/Contents/MacOS/agent",', cask)
        self.assertIn('args:         ["shutdown", "--store", "#{Dir.home}/.agent/state.sqlite", "--grace", "30s"]', cask)
        # What a start writes is there at install; skill links go with the app.
        self.assertIn('Agent.app/Contents/MacOS/agent-app",\n                   args:         ["--setup"]', cask)
        self.assertIn('args:         ["--unlink-skills"]', cask)
        self.assertLess(cask.index("postflight do"), cask.index("uninstall_preflight do"))
        # brew style (Homebrew/OSDependsOn) requires it of a macOS-only cask.
        self.assertIn("  depends_on :macos\n", cask)
        # The store is shared with the CLI and is the user's data, not the app's.
        self.assertNotIn("~/.agent", cask)

    def test_rejects_invalid_versions_and_wrong_archive(self):
        for version in ["v0.1.0", "0.1", "../0.1.0", '0.1.0"', "01.2.3"]:
            with self.subTest(version=version), self.assertRaises(ValueError):
                render_cask(version, Path("missing.zip"))
        with self.assertRaises(ValueError):
            render_cask("0.1.0", Path("Agent_0.2.0_universal.zip"))

    def test_prerelease_uses_its_own_tag(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "Agent_0.1.0-rc.1_universal.zip"
            archive.write_bytes(b"release candidate")
            cask = render_cask("0.1.0-rc.1", archive)
        self.assertIn('  version "0.1.0-rc.1"', cask)


if __name__ == "__main__":
    unittest.main()
