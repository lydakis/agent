import json
import os
import tempfile
import time
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


    def test_every_planted_burst_ask_is_scored(self):
        rubric = (Path(__file__).resolve().parents[1] / 'docs/ROLE_EVAL.md').read_text()
        plants = [s['plant'] for s in role_eval.BURST if s.get('plant')]
        plants += [e[3] for s in role_eval.BURST for e in s.get('events', []) if len(e) > 3]
        for plant in plants:
            self.assertIn(f'| {plant.split()[0]} |', rubric)

    def test_the_burst_is_three_hundred_messages_with_twenty_asks_none_lost_by_the_source(self):
        events = [e for s in role_eval.BURST for e in s.get('events', [])]
        self.assertEqual(len(events), 300)
        self.assertEqual(sum(len(e) > 3 for e in events), 20)
        self.assertEqual({e[0] for e in events}, {'chat', 'ci'})
        # A wave fits in what a subscriber holds (64), and every ID stays in
        # what a source remembers (1024), so only a busy lead loses news.
        self.assertLessEqual(max(len(s.get('events', [])) for s in role_eval.BURST), 64)
        self.assertEqual(role_eval.BURST, role_eval.burst(), 'the same burst every run')

    def test_a_lead_is_subscribed_once_it_hears_every_source_of_the_day(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(role_eval, 'DAY', role_eval.BURST):
            home = Path(tmp)
            folder = home / '.agent/state.sqlite.lookout'
            folder.mkdir(parents=True)
            subs = [{'name': 'chat', 'sources': ['chat']}]
            (folder / 'shop.lead.json').write_text(json.dumps({'subscriptions': subs}))
            self.assertFalse(role_eval.subscribed(home))
            subs.append({'name': 'ci', 'sources': ['ci:main']})
            (folder / 'shop.lead.json').write_text(json.dumps({'subscriptions': subs}))
            self.assertTrue(role_eval.subscribed(home))


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
        for allowed in ['~/.agent/automation add --bot shop.lead --every 10m -- "Check the tasks"',
                        '"$HOME/.agent/automation" rm watch',
                        f'{home}/.agent/memory save flow --type project -- \'text\'',
                        "~/.agent/automation add --bot shop.lead --cron '*/5 * * * *' -- 'Is it {done}? Yes!'"]:
            self.assertTrue(role_eval.determinable(home, shell(allowed)), allowed)
        # The ACP shape of the same call.
        acp = {'toolCall': {'name': 'shell', 'rawInput': {'command': '~/.agent/automation ls'}}}
        self.assertTrue(role_eval.determinable(home, acp))
        for denied in ['mkdir -p ~/Backups/shop',
                       'cd /Users && mkdir -p me/Backups/shop',
                       'python3 -c "import pwd,os; os.makedirs(pwd.getpwuid(os.getuid()).pw_dir + \'/x\')"',
                       '~/.agent/automation ls; rm -rf ~/x',
                       '~/.agent/automation ls && touch /x',
                       '~/.agent/automation add -- "$(touch /x)"',
                       '~/.agent/automation add -- `touch /x`',
                       '~/.agent/automation ls > /Users/me/x',
                       '~/.agent/automation ls\ntouch /x',
                       'FOO=1 ~/.agent/automation ls',
                       # What runs a command or starts an agent outside the eval's row.
                       "~/.agent/automation add --bot shop.lead --every 10m --if 'touch /x' -- ping",
                       '~/.agent/automation add --if=true --bot shop.lead -- ping',
                       '~/.agent/automation add --start helper --model chatgpt/x --in 5m -- go',
                       '~/.agent/automation add --bot shop.lead --every 10m',
                       '~/.agent/automation run watch --if true',
                       '~/.agent/automation rm',
                       '~/.agent/automation edit watch',
                       '~/.agent/memory schedule --model chatgpt/x',
                       '~/.agent/memory cleanup start',
                       '~/.agent/memory save flow --type -- text',
                       '~/.agent/memory',
                       # What Bash would expand into other words than the ones checked.
                       "~/.agent/automation add --name {watch,--if,'touch /x'} --bot shop.lead --in 1m -- ping",
                       '~/.agent/automation rm w*',
                       '~/.agent/automation rm w?',
                       '~/.agent/automation rm [w]',
                       '~/.agent/automation rm ~/x',
                       '~/.agent/automation rm \\-\\-if',
                       '~/.agent/automation add --bot shop.lead --in 1m -- "!!"',
                       '~/.agent/automation add --bot shop.lead --in 1m -- "a\\"b"',
                       '~/.agent/automation rm watch # x',
                       "~/.agent/automation rm 'unclosed",
                       'git push origin main']:
            self.assertFalse(role_eval.determinable(home, shell(denied)), denied)
        # Sonny's own tools the day needs, with what stays in the run.
        native = lambda name, arguments: {'toolCall': {'name': name, 'rawInput': arguments}}
        for name, arguments in [('start_task', {'name': 'export', 'brief': 'b', 'setup': True}),
                                ('start_task', {'name': 'x', 'brief': 'b', 'own_folder': True}),
                                ('remove_task', {'task': 'export'}),
                                ('subscriptions', {'add': {'name': 'news', 'sources': ['chat']}}),
                                ('automations', {'add': {'every': '10m', 'message': 'check'}}),
                                ('automations', {'remove': 'watch'})]:
            self.assertTrue(role_eval.determinable(home, native(name, arguments)), (name, arguments))
        for name, arguments in [('start_task', {'name': 'x', 'brief': 'b', 'folder': 'nested'}),
                                ('start_task', {'name': 'x', 'brief': 'b', 'folder': '/Users/me'}),
                                ('start_task', {'name': 'x', 'brief': 'b', 'folder': '../..'}),
                                ('start_task', {'name': 'x', 'brief': 'b', 'folder': '~/x'}),
                                ('automations', {'add': {'every': '10m', 'if': 'touch /x', 'message': 'm'}}),
                                ('automations', {'add': {'start': {'name': 'n', 'model': 'm'}, 'message': 'm'}}),
                                ('new_project', {'folder': '/Users/me/x'}),
                                ('run_in_pane', {'command': 'ls'}),
                                ('goal', {}), ('browser', {})]:
            self.assertFalse(role_eval.determinable(home, native(name, arguments)), (name, arguments))
        # Not a shell call, or not seen whole.
        self.assertFalse(role_eval.determinable(home, {'name': 'write', 'arguments': {'path': '/x', 'content': ''}}))
        self.assertFalse(role_eval.determinable(home, shell('~/.agent/automation ls', arguments_cut=['command'])))
        self.assertFalse(role_eval.determinable(home, shell('~/.agent/automation ls', arguments_omitted=1)))

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

    def test_a_run_lets_go_of_its_homes_scheduler_job_and_only_that(self):
        # Sonny loads a job per HOME in the person's own scheduler; one a run
        # left would stay loaded after it.
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            agents = home / 'Library/LaunchAgents'
            agents.mkdir(parents=True)
            (agents / f'{role_eval.SCHEDULER}8a8d61ae.plist').write_text('')
            (agents / 'com.other.plist').write_text('')
            units = home / '.config/systemd/user'
            units.mkdir(parents=True)
            (units / f'{role_eval.SCHEDULER}8a8d61ae.service').write_text('')
            for platform, want in [('darwin', ['/bin/launchctl', 'bootout', f'gui/{os.getuid()}',
                                               str(agents / f'{role_eval.SCHEDULER}8a8d61ae.plist')]),
                                   ('linux', ['systemctl', '--user', 'disable', '--now',
                                              f'{role_eval.SCHEDULER}8a8d61ae.service'])]:
                with mock.patch.object(role_eval.sys, 'platform', platform), \
                        mock.patch.object(role_eval.subprocess, 'run') as run:
                    role_eval.let_go_scheduler(home)
                self.assertEqual([c.args[0] for c in run.call_args_list], [want])
            self.assertTrue((agents / f'{role_eval.SCHEDULER}8a8d61ae.plist').exists())

    def test_a_prompt_left_out_holds_up_nothing_and_its_answer_keeps_its_time(self):
        # The burst's mid-burst question: the waves go on while it is out.
        with tempfile.TemporaryDirectory() as tmp:
            sonny = Path(tmp) / 'sonny'
            sonny.write_text('#!/usr/bin/env python3\nimport json, sys, time\n'
                             'for line in sys.stdin:\n'
                             '    time.sleep(1)\n'
                             '    print(json.dumps({"id": json.loads(line)["id"], "result": {}}), flush=True)\n')
            sonny.chmod(0o755)
            acp = role_eval.Acp(sonny, dict(os.environ), tmp, Path(tmp) / 'acp.jsonl', tmp)
            try:
                sent = time.time()
                rid = acp.ask('session/prompt', {}, wait=False)
                self.assertLess(time.time() - sent, 0.5)
                self.assertNotIn(rid, acp.answers)
                self.assertEqual(acp.collect(rid, timeout=10), {'id': rid, 'result': {}})
                self.assertGreaterEqual(acp.at[rid] - sent, 1)
            finally:
                acp.close()

    def test_the_mid_burst_question_is_about_code_no_ask_changes(self):
        asked = next(s for s in role_eval.BURST if s.get('say') and s.get('gap'))
        self.assertIn('sample order 1', asked['say'])
        self.assertIn("Order(1, Decimal('19.99'), 'EUR'", role_eval.FILES['shop/orders.py'])
        # Asks add to Order (a __str__, is_refund), never change a sample.
        for ask in role_eval.BURST_ASKS:
            self.assertNotIn('order 1', ask[2])

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



class BurstLedgerTests(unittest.TestCase):
    from bench import burst_ledger as ledger

    def repo(self, tmp, files):
        repo = Path(tmp) / 'shop'
        for path, text in files.items():
            (repo / path).parent.mkdir(parents=True, exist_ok=True)
            (repo / path).write_text(text)
        env = {**os.environ, 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_AUTHOR_NAME': 'e', 'GIT_AUTHOR_EMAIL': 'e@x',
               'GIT_COMMITTER_NAME': 'e', 'GIT_COMMITTER_EMAIL': 'e@x'}
        for args in (['init', '-q', '-b', 'main'], ['add', '-A'], ['commit', '-qm', 'shop']):
            role_eval.subprocess.run(['git', *args], cwd=repo, env=env, check=True, capture_output=True)
        return repo, env

    def agent(self, tmp):
        """A stand-in for `agent sandbox` that runs the command as it is."""
        agent = Path(tmp) / 'agent'
        agent.write_text('#!/bin/sh\nwhile [ "$1" != "--" ]; do shift; done\nshift\nexec "$@"\n')
        agent.chmod(0o755)
        return agent

    def test_every_planted_burst_ask_has_a_way_to_tell_how_it_ended(self):
        plants = {e[3] for e in role_eval.BURST_ASKS}
        self.assertEqual(plants, set(self.ledger.LEDGER))
        for plant, spec in self.ledger.LEDGER.items():
            self.assertEqual('check' in spec, plant.startswith('B1'), plant)

    def test_no_code_ask_passes_on_the_repo_as_it_starts_and_a_done_one_does(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, env = self.repo(tmp, role_eval.FILES)
            agent = self.agent(tmp)
            for plant, spec in self.ledger.LEDGER.items():
                if 'check' in spec:
                    self.assertFalse(self.ledger.checked(agent, repo, env, 'main', spec['check']), plant)
            done = dict(role_eval.FILES)
            done['.gitignore'] = '__pycache__/\n'
            done['shop/session.py'] = done['shop/session.py'].replace('5 * 60', '5 * 60 * 60')
            done['shop/orders.py'] = done['shop/orders.py'].replace(
                '    created_at: datetime  # always timezone-aware\n',
                "    created_at: datetime  # always timezone-aware\n\n"
                "    def __str__(self):\n        return f'#{self.id} {self.total} {self.currency}'\n")
        with tempfile.TemporaryDirectory() as tmp:
            repo, env = self.repo(tmp, done)
            agent = self.agent(tmp)
            for plant in ('B1 code ask: gitignore', 'B1 code ask: session timeout', 'B1 code ask: Order __str__'):
                self.assertTrue(self.ledger.checked(agent, repo, env, 'main', self.ledger.LEDGER[plant]['check']),
                                plant)

    def test_the_ledger_follows_each_ask_from_its_note_to_how_it_ended(self):
        asks = {e[3]: e for e in role_eval.BURST_ASKS}
        code, look = asks['B1 code ask: gitignore'], asks['B3 quick look: USD']
        events = [{'step': 2, 'id': 's2e0', 'source': code[0], 'from': code[1], 'text': code[2], 'plant': code[3]},
                  {'step': 2, 'id': 's2e1', 'source': look[0], 'from': look[1], 'text': look[2], 'plant': look[3]},
                  {'step': 3, 'id': 's3e0', 'source': 'chat', 'from': 'sam', 'text': 'lol', 'plant': None}]
        with tempfile.TemporaryDirectory() as tmp:
            repo, env = self.repo(tmp, role_eval.FILES)
            out = Path(tmp) / 'out'
            out.mkdir()
            t0 = 1000.0
            update = lambda t, u: json.dumps({'t': t, 'm': {'method': 'session/update', 'params': {'update': u}}})
            note = 'From your sources: ...\n' + '\n'.join(f'- {e["source"]}: {json.dumps(e["text"])}' for e in events[:2])
            (out / 'acp.jsonl').write_text('\n'.join([
                update(t0 + 20, {'sessionUpdate': 'user_message', 'content': [{'type': 'text', 'text': note}]}),
                update(t0 + 25, {'sessionUpdate': 'tool_call', 'name': 'start_task',
                                 'rawInput': {'name': 'ignore', 'brief': 'add a .gitignore for __pycache__'}}),
                update(t0 + 30, {'sessionUpdate': 'agent_message_chunk', 'messageId': 'm1',
                                 'content': {'type': 'text', 'text': 'Sample order 2 is in '}}),
                update(t0 + 31, {'sessionUpdate': 'agent_message_chunk', 'messageId': 'm1',
                                 'content': {'type': 'text', 'text': 'USD.'}}),
            ]) + '\n')
            (out / 'events.jsonl').write_text('\n'.join(json.dumps(e) for e in [
                {'t': t0, 'event': 'usage', 'bot': 'shop.ignore', 'data': {'input_tokens': 10, 'output_tokens': 2}},
                {'t': t0, 'event': 'turn_finished', 'bot': 'shop.ignore', 'data': {'status': 'failed', 'error': 'x'}},
                {'t': t0, 'event': 'deleted', 'bot': 'shop.ignore'},
            ]) + '\n')
            lookout = Path(tmp) / 'lookout.jsonl'
            lookout.write_text(json.dumps({'lead': 'shop.lead', 'source': 'chat', 'skipped': 'full'}) + '\n'
                               + json.dumps({'lead': 'shop.lead', 'woke': [], 'waiting': 40}) + '\n')
            timeline = [{'step': 1, 't': t0 - 100}, {'step': 2, 't': t0}, {'step': 3, 't': t0 + 120}]
            agent = self.agent(tmp)
            summary = self.ledger.ledger(out, agent, repo, env, events, timeline, lookout, {'peak_rss_mb': 1.0},
                                         ended=t0 + 600)
            rows = {r['ask']: r for r in json.loads((out / 'asks.json').read_text())['asks']}
            # Written again later, an ask still waits only until the run ended.
            again = self.ledger.ledger(out, agent, repo, env, events, timeline, lookout)
        self.assertEqual(set(rows), {code[3], look[3]})
        self.assertEqual((rows[code[3]]['reached_s'], rows[code[3]]['delegated_s']), (20, 25))
        self.assertIs(rows[code[3]]['on_main'], False)
        self.assertEqual(rows[look[3]]['said_s'], 31)
        self.assertEqual((summary['reached'], summary['handled'], summary['code_on_main']), (2, 2, 0))
        self.assertEqual((summary['turns_failed'], summary['deleted'], summary['input_tokens']), (1, 1, 10))
        self.assertEqual((summary['not_kept'], summary['most_waiting'], summary['peak_rss_mb']), (1, 40, 1.0))
        self.assertTrue(summary['main_tests_pass'])
        self.assertEqual((again['ended'], again['oldest_pending_s']), (t0 + 600, summary['oldest_pending_s']))

    def test_a_code_ask_the_lead_did_itself_counts_as_handled(self):
        code = next(e for e in role_eval.BURST_ASKS if e[3] == 'B1 code ask: gitignore')
        events = [{'step': 1, 'id': 's1e0', 'source': code[0], 'from': code[1], 'text': code[2], 'plant': code[3]}]
        with tempfile.TemporaryDirectory() as tmp:
            repo, env = self.repo(tmp, role_eval.FILES)
            out = Path(tmp) / 'out'
            out.mkdir()
            edit = {'sessionUpdate': 'tool_call', 'name': next(iter(self.ledger.EDITS)),
                    'rawInput': {'path': '.gitignore', 'content': '__pycache__/'}}
            (out / 'acp.jsonl').write_text(json.dumps({'t': 50.0, 'm': {'method': 'session/update',
                                                                        'params': {'update': edit}}}) + '\n')
            summary = self.ledger.ledger(out, self.agent(tmp), repo, env, events, [{'step': 1, 't': 0.0}],
                                         Path(tmp) / 'none.jsonl', ended=500.0)
            row = json.loads((out / 'asks.json').read_text())['asks'][0]
        self.assertEqual((row['edited_itself_s'], row['pending_s'], summary['handled']), (50, 50, 1))

    @unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
    def test_the_ledger_runs_the_agents_code_in_the_sandbox_without_the_key(self):
        agent = Path('.local/target/release/agent').resolve()
        probe = role_eval.subprocess.run([agent, 'sandbox', '--', 'true'], capture_output=True, text=True)
        if 'sandbox_unavailable' in probe.stdout + probe.stderr and os.environ.get('AGENT_TEST_SANDBOX') != '1':
            self.skipTest('no sandbox runs here; AGENT_TEST_SANDBOX=1 makes this fail instead')
        # Outside /tmp, which every sandbox writes, as the eval's runs are.
        Path('.local').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir='.local') as tmp:
            outside = Path(tmp).resolve() / 'outside'
            outside.mkdir()
            repo, env = self.repo(tmp, role_eval.FILES)
            env = {**env, 'HOME': str(outside), 'OPENAI_API_KEY': 'sk-test'}
            wrote = f"open({str(outside / 'x')!r}, 'w').write('x')"
            self.assertFalse(self.ledger.checked(agent, repo, env, 'main', wrote))
            self.assertFalse((outside / 'x').exists())
            self.assertTrue(self.ledger.checked(agent, repo, env, 'main', "open('here', 'w').write('x')"))
            keyless = "import os, sys; sys.exit('OPENAI_API_KEY' in os.environ)"
            self.assertTrue(self.ledger.checked(agent, repo, env, 'main', keyless))
