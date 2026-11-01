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
    """What the eval allows has effects only in the run (#521)."""

    def test_only_a_plain_call_of_sonnys_scripts_is_allowed(self):
        home = role_eval.ROOT / 'A1/h'
        shell = lambda command, **more: {'name': 'shell', 'arguments': {'command': command}, **more}
        for allowed in ['~/.agent/trigger add --bot shop.lead --every 10m -- "Check the tasks"',
                        '"$HOME/.agent/trigger" rm watch',
                        f'{home}/.agent/memory save flow --type project -- \'text\'',
                        "~/.agent/trigger add --bot shop.lead --cron '*/5 * * * *' -- 'Is it {done}? Yes!'"]:
            self.assertTrue(role_eval.determinable(home, shell(allowed)), allowed)
        # The ACP shape of the same call.
        acp = {'toolCall': {'name': 'shell', 'rawInput': {'command': '~/.agent/trigger ls'}}}
        self.assertTrue(role_eval.determinable(home, acp))
        for denied in ['mkdir -p ~/Backups/shop',
                       'cd /Users && mkdir -p me/Backups/shop',
                       'python3 -c "import pwd,os; os.makedirs(pwd.getpwuid(os.getuid()).pw_dir + \'/x\')"',
                       '~/.agent/trigger ls; rm -rf ~/x',
                       '~/.agent/trigger ls && touch /x',
                       '~/.agent/trigger add -- "$(touch /x)"',
                       '~/.agent/trigger add -- `touch /x`',
                       '~/.agent/trigger ls > /Users/me/x',
                       '~/.agent/trigger ls\ntouch /x',
                       'FOO=1 ~/.agent/trigger ls',
                       # What runs a command or starts an agent outside the eval's row.
                       "~/.agent/trigger add --bot shop.lead --every 10m --if 'touch /x' -- ping",
                       '~/.agent/trigger add --if=true --bot shop.lead -- ping',
                       '~/.agent/trigger add --start helper --model chatgpt/x --in 5m -- go',
                       '~/.agent/trigger add --bot shop.lead --every 10m',
                       '~/.agent/trigger fire watch --if true',
                       '~/.agent/trigger rm',
                       '~/.agent/trigger edit watch',
                       '~/.agent/memory schedule --model chatgpt/x',
                       '~/.agent/memory cleanup start',
                       '~/.agent/memory save flow --type -- text',
                       '~/.agent/memory',
                       # What Bash would expand into other words than the ones checked.
                       "~/.agent/trigger add --name {watch,--if,'touch /x'} --bot shop.lead --in 1m -- ping",
                       '~/.agent/trigger rm w*',
                       '~/.agent/trigger rm w?',
                       '~/.agent/trigger rm [w]',
                       '~/.agent/trigger rm ~/x',
                       '~/.agent/trigger rm \\-\\-if',
                       '~/.agent/trigger add --bot shop.lead --in 1m -- "!!"',
                       '~/.agent/trigger add --bot shop.lead --in 1m -- "a\\"b"',
                       '~/.agent/trigger rm watch # x',
                       "~/.agent/trigger rm 'unclosed",
                       'git push origin main']:
            self.assertFalse(role_eval.determinable(home, shell(denied)), denied)
        # Sonny's own tools the day needs, with what stays in the run.
        native = lambda name, arguments: {'toolCall': {'name': name, 'rawInput': arguments}}
        for name, arguments in [('start_task', {'name': 'export', 'brief': 'b', 'setup': True}),
                                ('start_task', {'name': 'x', 'brief': 'b', 'own_folder': True}),
                                ('remove_task', {'task': 'export'}),
                                ('subscriptions', {'add': {'name': 'news', 'sources': ['chat']}}),
                                ('triggers', {'add': {'every': '10m', 'message': 'check'}}),
                                ('triggers', {'remove': 'watch'})]:
            self.assertTrue(role_eval.determinable(home, native(name, arguments)), (name, arguments))
        for name, arguments in [('start_task', {'name': 'x', 'brief': 'b', 'folder': 'nested'}),
                                ('start_task', {'name': 'x', 'brief': 'b', 'folder': '/Users/me'}),
                                ('start_task', {'name': 'x', 'brief': 'b', 'folder': '../..'}),
                                ('start_task', {'name': 'x', 'brief': 'b', 'folder': '~/x'}),
                                ('triggers', {'add': {'every': '10m', 'if': 'touch /x', 'message': 'm'}}),
                                ('triggers', {'add': {'start': {'name': 'n', 'model': 'm'}, 'message': 'm'}}),
                                ('new_project', {'folder': '/Users/me/x'}),
                                ('run_in_pane', {'command': 'ls'}),
                                ('goal', {}), ('browser', {})]:
            self.assertFalse(role_eval.determinable(home, native(name, arguments)), (name, arguments))
        # Not a shell call, or not seen whole.
        self.assertFalse(role_eval.determinable(home, {'name': 'write', 'arguments': {'path': '/x', 'content': ''}}))
        self.assertFalse(role_eval.determinable(home, shell('~/.agent/trigger ls', arguments_cut=['command'])))
        self.assertFalse(role_eval.determinable(home, shell('~/.agent/trigger ls', arguments_omitted=1)))

    def test_tasks_calls_are_answered_while_the_leads_prompt_is_out(self):
        # A lead waiting on a task before it answers would otherwise wait
        # out the prompt while the task waits on the eval.
        with tempfile.TemporaryDirectory() as tmp:
            sonny = Path(tmp) / 'sonny'
            sonny.write_text('#!/bin/sh\nexec cat >/dev/null\n')
            sonny.chmod(0o755)
            served = []
            with mock.patch.object(role_eval.Acp, 'SERVE', 0.1):
                acp = role_eval.Acp(sonny, dict(os.environ), tmp, Path(tmp) / 'acp.jsonl', tmp,
                                    lambda: served.append(1))
                try:
                    with self.assertRaises(RuntimeError):
                        acp.ask('session/prompt', {}, timeout=1)
                finally:
                    acp.close()
            self.assertTrue(served)

    def test_runs_live_outside_what_the_sandbox_always_writes(self):
        # /tmp and $TMPDIR are writable in every sandbox: a run there could
        # have its copy of the build replaced.
        for temporary in {Path('/tmp'), Path(tempfile.gettempdir())}:
            self.assertFalse(role_eval.ROOT.resolve().is_relative_to(temporary.resolve()))

    def test_the_run_writes_its_folder_without_network_and_the_eval_approves(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'A1'
            home, _, _, _ = role_eval.setup(root, build(tmp), 'chatgpt/gpt-6.1-sol')
            config = (home / '.agent/config.toml').read_text()
            self.assertIn('default_permissions = "eval"', config)
            self.assertIn('extends = "ask"', config)
            self.assertIn('network = false', config)
            self.assertIn(f'writable = ["{root}/backups", "{root}/origin.git"]', config)
            self.assertTrue((root / 'backups').is_dir())

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

