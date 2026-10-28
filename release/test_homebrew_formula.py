import hashlib
from pathlib import Path
import tempfile
import unittest

from homebrew_formula import render_formula


class FormulaTest(unittest.TestCase):
    def render(self, name):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "agent-0.3.0.tar.gz"
            archive.write_bytes(b"source")
            return render_formula(name, "0.3.0", archive)

    def test_both_build_from_the_pinned_source_archive(self):
        for name in ["agent", "sonny"]:
            with self.subTest(name=name):
                formula = self.render(name)
                self.assertTrue(formula.startswith(f"class {name.capitalize()} < Formula\n"))
                self.assertIn(f'  sha256 "{hashlib.sha256(b"source").hexdigest()}"', formula)
                self.assertIn("/releases/download/v0.3.0/agent-0.3.0.tar.gz", formula)
                # The version comes from the archive's name; stating it
                # again is what brew audit calls redundant.
                self.assertNotIn('  version "', formula)
                self.assertIn('  depends_on "rust" => :build\n', formula)
                # Built on the person's machine, on macOS or Linux.
                self.assertNotIn("depends_on :macos", formula)
                self.assertNotIn("releases/download/v0.3.0/sonny", formula)

    def test_agent_gives_just_agent(self):
        formula = self.render("agent")
        self.assertIn('    system "cargo", "install", *std_cargo_args\n', formula)
        self.assertIn('shell_output("#{bin}/agent --version")', formula)
        self.assertNotIn("lydakis/agent/agent", formula)

    def test_sonny_pulls_in_agent(self):
        formula = self.render("sonny")
        self.assertIn('  depends_on "lydakis/agent/agent"\n  depends_on "ripgrep"\n', formula)
        self.assertIn('*std_cargo_args(path: "sonny")', formula)
        self.assertIn('shell_output("#{bin}/sonny --version")', formula)
        # Sonny finds the daemon by the agent formula's opt path itself, so
        # launchd's trigger fires need no wrapper.
        self.assertNotIn("write_env_script", formula)

    def test_rejects_invalid_versions_and_wrong_archive(self):
        for version in ["v0.1.0", "0.1", '0.1.0"', "01.2.3"]:
            with self.subTest(version=version), self.assertRaises(ValueError):
                render_formula("agent", version, Path("missing.tar.gz"))
        with self.assertRaises(ValueError):
            render_formula("agent", "0.1.0", Path("agent-0.2.0.tar.gz"))
        with self.assertRaises(ValueError):
            render_formula("sonny", "0.1.0", Path("sonny-0.1.0.tar.gz"))


if __name__ == "__main__":
    unittest.main()
