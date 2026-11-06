import base64
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from nightly_formula import prepare_update, render_formula
from test_release_workflow import workflow_step, WORKFLOWS


COMMIT = "a" * 40


class NightlyFormulaTest(unittest.TestCase):
    def test_pins_both_binaries_to_one_checkout(self):
        formula = render_formula(COMMIT, "12.1")
        self.assertIn(f'revision: "{COMMIT}"', formula)
        self.assertIn('version "12.1"', formula)
        self.assertIn('head "https://github.com/lydakis/agent.git", branch: "main"', formula)
        self.assertIn('std_cargo_args(root: libexec)', formula)
        self.assertIn('libexec.install libexec/"bin/agent"', formula)
        self.assertIn('std_cargo_args(path: "sonny")', formula)
        self.assertNotIn('depends_on "lydakis/agent/agent"', formula)
        for dependency in ["rust", "ripgrep", "agent-browser", "bubblewrap"]:
            self.assertIn(f'depends_on "{dependency}"', formula)
        self.assertIn('ENV["AGENT_BWRAP"]', formula)
        self.assertIn('conflicts_with "sonny"', formula)
        # Both the client and its bundled daemon are exercised by brew test.
        self.assertIn('#{bin}/sonny --version', formula)
        self.assertIn('#{bin}/sonny agent --version', formula)
        self.assertIn(COMMIT[:9], formula)

    def test_rejects_invalid_or_executable_inputs(self):
        for commit, version in [("main", "1.1"), ("a" * 39, "1.1"),
                                (COMMIT, "1"), (COMMIT, "01.1"),
                                (COMMIT, '1.1"; system "false"')]:
            with self.subTest(commit=commit, version=version), self.assertRaises(ValueError):
                render_formula(commit, version)

    def test_create_update_and_concurrent_edit_guard(self):
        with tempfile.TemporaryDirectory() as directory:
            current = Path(directory) / "Formula/sonny-nightly.rb"
            request = prepare_update(COMMIT, "12.1", current)
            self.assertNotIn("sha", request)
            previous = current.read_bytes()
            self.assertEqual(base64.b64decode(request["content"]), previous)
            request = prepare_update("b" * 40, "13.1", current)
            expected = hashlib.sha1(b"blob " + str(len(previous)).encode() + b"\0" + previous).hexdigest()
            self.assertEqual(request["sha"], expected)
            self.assertEqual(base64.b64decode(request["content"]), current.read_bytes())

    def test_reruns_unchanged_commits_and_older_runs_do_not_publish(self):
        with tempfile.TemporaryDirectory() as directory:
            current = Path(directory) / "sonny-nightly.rb"
            prepare_update(COMMIT, "12.2", current)
            previous = current.read_bytes()
            for commit, version in [(COMMIT, "12.2"), (COMMIT, "13.1"),
                                    ("b" * 40, "11.9"), ("b" * 40, "12.1")]:
                self.assertIsNone(prepare_update(commit, version, current))
                self.assertEqual(current.read_bytes(), previous)
            with self.assertRaisesRegex(ValueError, "same version"):
                prepare_update("b" * 40, "12.2", current)
            current.write_text("unexpected tap contents")
            with self.assertRaisesRegex(ValueError, "current nightly"):
                prepare_update(COMMIT, "13.1", current)


class NightlyWorkflowTest(unittest.TestCase):
    def test_publication_is_guarded_and_uses_the_validated_formula(self):
        workflow = (WORKFLOWS / "nightly.yml").read_text()
        self.assertIn('cron: "17 7 * * *"', workflow)
        self.assertIn("workflow_dispatch:", workflow)
        # A failed publish job can be retried without rebuilding its artifact.
        self.assertNotIn("github.run_attempt", workflow)
        self.assertIn("github.repository == 'lydakis/agent' && github.ref == 'refs/heads/main'", workflow)
        publish = workflow[workflow.index("\n  publish:\n"):]
        self.assertIn("needs: validate", publish)
        self.assertNotIn("secrets.HOMEBREW_TAP_GITHUB_TOKEN", workflow[:workflow.index("\n  publish:\n")])
        self.assertIn('cmp tap/Formula/sonny-nightly.rb "$RUNNER_TEMP/validated/sonny-nightly.rb"', publish)
        self.assertIn("persist-credentials: false", publish)

    def test_prepare_checks_validation_and_stale_runs_skip_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            temp = root / "temp"
            validated = temp / "validated/sonny-nightly.rb"
            validated.parent.mkdir(parents=True)
            validated.write_text(render_formula(COMMIT, "12.1"))
            env = {**os.environ, "NIGHTLY_COMMIT": COMMIT, "NIGHTLY_VERSION": "12.1",
                   "RUNNER_TEMP": str(temp), "GITHUB_OUTPUT": str(root / "outputs")}
            # Use the real workflow shell against a temporary tap and the actual generator.
            (root / "release").symlink_to(Path(__file__).resolve().parent, target_is_directory=True)
            script = workflow_step("Prepare validated tap update", "nightly.yml")

            def run():
                (root / "outputs").unlink(missing_ok=True)
                return subprocess.run(["bash", "-euo", "pipefail", "-c", script],
                                      cwd=root, env=env, capture_output=True, text=True)

            result = run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((root / "outputs").read_text(), "changed=true\n")
            # A rerun clears a previously prepared payload rather than reusing it.
            self.assertEqual(run().returncode, 0)
            self.assertFalse((temp / "tap-nightly.json").exists())
            self.assertFalse((root / "outputs").exists())
            # A changed commit must match exactly what the build job validated.
            env.update(NIGHTLY_COMMIT="b" * 40, NIGHTLY_VERSION="13.1")
            self.assertNotEqual(run().returncode, 0)
            self.assertFalse((root / "outputs").exists())


if __name__ == "__main__":
    unittest.main()
