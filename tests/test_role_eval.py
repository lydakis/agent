import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bench import role_eval


def build(tmp):
    """A stand-in build: setup copies agent and sonny into the run."""
    bins = Path(tmp) / 'build'
    bins.mkdir()
    for name in ('agent', 'sonny'):
        (bins / name).write_text('#!/bin/sh\n')
    return bins


class SetupTests(unittest.TestCase):
    def test_only_the_models_key_reaches_the_run_and_none_is_written(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(
                os.environ, {'ANTHROPIC_API_KEY': 'sk-test-anthropic', 'OPENAI_API_KEY': 'sk-test-openai'}):
            root = Path(tmp) / 'A1'
            home, repo, env, bins = role_eval.setup(root, build(tmp), 'anthropic/claude-opus-5-5')
            self.assertEqual(bins, root / 'bin')
            self.assertTrue((bins / 'sonny').is_file())
            self.assertEqual(env['ANTHROPIC_API_KEY'], 'sk-test-anthropic')
            self.assertNotIn('OPENAI_API_KEY', env)
            self.assertIn('provider = "anthropic"', (home / '.agent/config.toml').read_text())
            for path in root.rglob('*'):
                if path.is_file():
                    self.assertNotIn(b'sk-test', path.read_bytes(), path)

    def test_the_chatgpt_plan_needs_no_key(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, {'OPENAI_API_KEY': 'sk-test'}):
            _, _, env, _ = role_eval.setup(Path(tmp) / 'A1', build(tmp), 'chatgpt/gpt-6.1-sol')
            self.assertNotIn('OPENAI_API_KEY', env)

    def test_every_planted_step_is_scored(self):
        plants = [s['plant'].split()[0] for s in role_eval.STEPS if s.get('plant')]
        plants += [e['plant'].split()[0] for e in role_eval.events() if e['plant']]
        rubric = (Path(__file__).resolve().parents[1] / 'docs/ROLE_EVAL.md').read_text()
        for plant in plants:
            self.assertIn(f'| {plant} |', rubric)


class BlindTests(unittest.TestCase):
    def run_dir(self, runs, name, build, rows):
        out = runs / name
        out.mkdir(parents=True)
        (out / 'bin.txt').write_text(build)
        (out / 'acp.jsonl').write_text(''.join(json.dumps({'t': 0, 'm': m}) + '\n' for m in rows))
        (out / 'transcript.md').write_text(f'- call exec: "{build}/agent ls"\n')

    def test_failed_turns_stay_out_and_builds_are_hidden(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / 'out'
            runs = out / 'runs'
            ok = {'method': 'session/update', 'params': {'update': {'sessionUpdate': 'state_update', 'state': 'idle'}}}
            bad = {'method': 'session/update', 'params': {'update': {
                'sessionUpdate': 'state_update', 'state': 'idle', 'error': {'message': 'rate limited'}}}}
            self.run_dir(runs, 'A1', '/work/baseline/target/release', [ok])
            self.run_dir(runs, 'B1', '/work/trim/target/release', [ok])
            self.run_dir(runs, 'B2', '/work/trim/target/release', [bad])
            with mock.patch.object(role_eval, 'OUT', out), mock.patch.object(role_eval, 'RUNS', runs):
                role_eval.blind()
            key = json.loads((out / 'blind-key.json').read_text())
            self.assertEqual(sorted(key.values()), ['A1', 'B1'])
            for path in (out / 'blind').iterdir():
                text = path.read_text()
                self.assertNotIn('baseline', text)
                self.assertNotIn('trim', text)


if __name__ == '__main__':
    unittest.main()
