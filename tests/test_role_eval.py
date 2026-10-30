import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bench import role_eval


class SetupTests(unittest.TestCase):
    def test_only_the_models_key_reaches_the_run_and_none_is_written(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(
                os.environ, {'ANTHROPIC_API_KEY': 'sk-test-anthropic', 'OPENAI_API_KEY': 'sk-test-openai'}):
            root = Path(tmp) / 'A1'
            home, repo, env = role_eval.setup(root, '/bin', 'anthropic/claude-opus-5-5')
            self.assertEqual(env['ANTHROPIC_API_KEY'], 'sk-test-anthropic')
            self.assertNotIn('OPENAI_API_KEY', env)
            self.assertIn('provider = "anthropic"', (home / '.agent/config.toml').read_text())
            for path in root.rglob('*'):
                if path.is_file():
                    self.assertNotIn(b'sk-test', path.read_bytes(), path)

    def test_the_chatgpt_plan_needs_no_key(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, {'OPENAI_API_KEY': 'sk-test'}):
            _, _, env = role_eval.setup(Path(tmp) / 'A1', '/bin', 'chatgpt/gpt-6.1-sol')
            self.assertNotIn('OPENAI_API_KEY', env)

    def test_every_planted_step_is_scored(self):
        plants = [s['plant'].split()[0] for s in role_eval.STEPS if s.get('plant')]
        plants += [e['plant'].split()[0] for e in role_eval.events() if e['plant']]
        rubric = (Path(__file__).resolve().parents[1] / 'docs/ROLE_EVAL.md').read_text()
        for plant in plants:
            self.assertIn(f'| {plant} |', rubric)


if __name__ == '__main__':
    unittest.main()
