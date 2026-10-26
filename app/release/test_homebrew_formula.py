import hashlib
from pathlib import Path
import tempfile
import unittest

from homebrew_formula import render_formula


class FormulaTest(unittest.TestCase):
    def test_formula_pins_the_archive_and_keeps_the_daemon_off_path(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "sonny-0.3.0.tar.gz"
            archive.write_bytes(b"sonny and agent")
            formula = render_formula("0.3.0", archive)
        self.assertIn(f'  sha256 "{hashlib.sha256(b"sonny and agent").hexdigest()}"', formula)
        self.assertIn("/releases/download/v0.3.0/sonny-0.3.0.tar.gz", formula)
        # The version comes from the archive's name; stating it again is
        # what brew audit calls redundant.
        self.assertNotIn('  version "', formula)
        self.assertIn('    bin.install "bin/sonny"\n', formula)
        self.assertIn('    libexec.install "libexec/agent"\n', formula)
        self.assertNotIn('bin.install "libexec/agent"', formula)
        self.assertIn("  depends_on :macos\n", formula)
        self.assertIn('shell_output("#{bin}/sonny --version")', formula)

    def test_rejects_invalid_versions_and_wrong_archive(self):
        for version in ["v0.1.0", "0.1", '0.1.0"', "01.2.3"]:
            with self.subTest(version=version), self.assertRaises(ValueError):
                render_formula(version, Path("missing.tar.gz"))
        with self.assertRaises(ValueError):
            render_formula("0.1.0", Path("sonny-0.2.0.tar.gz"))


if __name__ == "__main__":
    unittest.main()
