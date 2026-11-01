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


class ConfinedTests(unittest.TestCase):
    """What the eval allows may not reach the real home (#521)."""

    def test_a_call_into_the_real_home_is_denied_and_one_inside_the_run_allowed(self):
        with tempfile.TemporaryDirectory() as tmp:
            real = Path(tmp) / 'Users' / 'me'
            root = Path(tmp) / 'role' / 'A1'
            root.mkdir(parents=True)
            with mock.patch.object(role_eval, 'REAL_HOME', real.resolve()):
                inside = {'toolCall': {'rawInput': {'command': f'git clone --mirror . {root}/backups/shop.git'}}}
                self.assertTrue(role_eval.confined(root, inside))
                outside = {'toolCall': {'rawInput': {'command': f'mkdir -p {real}/Backups/shop'}}}
                self.assertFalse(role_eval.confined(root, outside))
                # A path that climbs out of the run counts where it lands.
                climbs = {'arguments': {'path': f'{root}/../../Users/me/notes.md'}}
                self.assertFalse(role_eval.confined(root, climbs))
                # Escalations with no path, and system paths, are not the home.
                self.assertTrue(role_eval.confined(root, {'arguments': {'command': 'git push origin main'}}))
                self.assertTrue(role_eval.confined(root, {'arguments': {'command': 'ls /usr/bin > /dev/null'}}))
                # A folder above the home, reached relative from there.
                above = {'arguments': {'command': f'cd {real.parent} && mkdir -p me/Backups/shop'}}
                self.assertFalse(role_eval.confined(root, above))
                # The step's own backup, next to the repository, is in the run.
                self.assertTrue(role_eval.confined(root, {'arguments': {'command': 'git clone --mirror . ../backups/shop.git'}}))
                # A climb out of the run, or someone's home by name, is not.
                self.assertFalse(role_eval.confined(root, {'arguments': {'command': 'cp -r . ../../../../x'}}))
                self.assertFalse(role_eval.confined(root, {'arguments': {'command': 'cp -r . ~me/Backups'}}))
                self.assertTrue(role_eval.confined(root, {'arguments': {'command': 'cp -r . ~/backups'}}))
                # A call listed cut can't be seen whole, so it is denied.
                cut = {'arguments': {'command': 'git push'}, 'arguments_cut': ['command']}
                self.assertFalse(role_eval.confined(root, cut))
                self.assertFalse(role_eval.confined(root, {'arguments': {}, 'arguments_omitted': 3}))

    def test_what_changed_in_the_real_home_is_found_past_hidden_folders_and_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            real = Path(tmp) / 'me'
            (real / '.cache').mkdir(parents=True)
            (real / 'Library').mkdir()
            root = real / 'role' / 'A1'
            root.mkdir(parents=True)
            old = real / 'old.txt'
            old.write_text('x')
            os.utime(old, (0, 0))
            since = 1.0
            (real / '.cache' / 'noise').write_text('x')
            (real / 'Library' / 'noise').write_text('x')
            (root / 'backup').write_text('x')
            (real / 'Backups').mkdir()
            (real / 'Backups' / 'shop.git').write_text('x')
            with mock.patch.object(role_eval, 'REAL_HOME', real.resolve()):
                found = role_eval.changed_outside(since, root)
            names = {Path(p).name for p in found}
            self.assertIn('shop.git', names)
            self.assertNotIn('old.txt', names)
            self.assertNotIn('noise', names)
            self.assertNotIn('backup', names)

