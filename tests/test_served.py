"""The tools Sonny answers, end to end: an agent's call waits in the daemon,
the lookout runs it in the agent's folder, and its result or error is the
call's output."""
import json
import os
import subprocess
import time
import unittest
from pathlib import Path

from bench.runtime_client import node_item
from bench.socket_client import SocketClient
from bench.targets import clean_env
from tests.test_runtime import ModelFixture


def declared(name, inherit=True):
    """One of Sonny's answered tools, as small as the daemon takes it."""
    tool = {'name': name, 'description': name, 'answered_by': 'sonny',
            'parameters': {'type': 'object'}}
    if not inherit:
        tool['inherit'] = False
    return tool


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ServedToolTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.sonny = self.binary.parent / 'sonny'
        self.store = self.path / 'state.sqlite'
        self.daemon = SocketClient(self.binary, self.store, self.url, 'echo')
        self.addCleanup(self.daemon.close)
        self.script = []

    def lookout(self):
        env = {**clean_env(), 'HOME': str(self.path), 'AGENT_BIN': str(self.binary),
               'GIT_AUTHOR_NAME': 't', 'GIT_AUTHOR_EMAIL': 't@example.com',
               'GIT_COMMITTER_NAME': 't', 'GIT_COMMITTER_EMAIL': 't@example.com'}
        service = subprocess.Popen([str(self.sonny), 'lookout', '--judge', 'off', '--store', str(self.store),
                                    '--socket', str(self.daemon.socket_path)],
                                   env=env, stdout=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (service.kill(), service.wait(timeout=5), service.stdout.close()))
        self.assertEqual(json.loads(service.stdout.readline())['event'], 'watching')
        return service

    def call(self, bot, name, arguments):
        """`bot` calls `name` once; the call's output. The script runs on
        from the calls a bot's earlier turns made."""
        self.script.append((name, arguments))
        self.model.call_script = self.script
        turn = self.daemon.request('submit', bot=bot, request_id=f'{name}-{time.monotonic()}',
                                   prompt='script')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        return self.output(bot, turn)

    def output(self, bot, turn):
        events, after = [], 0
        while page := self.daemon.request('events', bot=bot, after=after, limit=256)['result']['events']:
            events, after = events + page, page[-1]['cursor']
        done = [e for e in events if e['turn'] == turn and e['event'] == 'tool_completed']
        self.assertTrue(done, [e['event'] for e in events if e['turn'] == turn])
        return node_item(self.daemon, bot, done[-1]['data']['node'])['result']['output']

    def git(self, folder, *args):
        subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@example.com', *args], cwd=folder,
                       check=True, capture_output=True)

    def test_a_lead_starts_a_task_in_a_worktree_of_its_own(self):
        repo = self.path / 'calc'
        repo.mkdir()
        self.git(repo, 'init', '-q', '-b', 'main')
        (repo / 'calc.py').write_text('one\n')
        self.git(repo, 'add', '.')
        self.git(repo, 'commit', '-qm', 'one')
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('start_task', inherit=False),
                                                           declared('show_agent')])
        self.lookout()
        output = self.call('calc.lead', 'start_task', {'name': 'fix', 'brief': f'Fix {repo}/calc.py.'})
        self.assertTrue(output.startswith('Started calc.fix: '), output)
        worktree = self.path / '.agent/worktrees/calc.fix'
        [task] = self.daemon.request('bots', name='calc.fix')['result']['bots']
        self.assertEqual((task['created_by'], task['workspace']), ('calc.lead', str(worktree)))
        self.assertEqual(task['model'], 'synthetic-model')
        # It has the tools every agent has, not its lead's own.
        record = self.daemon.request('inspect', bot='calc.fix')['result']
        self.assertEqual([tool['name'] for tool in record['declared']], ['show_agent'])
        # The brief names the task's copy, not the lead's folder.
        turns = self.daemon.request('turns', bot='calc.fix')['result']['turns']
        prompts = self.daemon.request('prompts', bot='calc.fix', turn=turns[0]['turn'])['result']['prompts']
        self.assertEqual(prompts[0]['text'], f'Fix {worktree}/calc.py.')
        # Its name is taken now, so a second start is refused, and nothing
        # is left behind.
        refused = json.loads(self.call('calc.lead', 'start_task', {'name': 'fix', 'brief': 'Again.'}))
        self.assertEqual(refused['error'], 'task_not_started')
        self.assertTrue(worktree.is_dir())

    def test_a_turn_interrupted_mid_setup_stops_it_and_starts_no_task(self):
        repo = self.path / 'calc'
        (repo / '.agents').mkdir(parents=True)
        setup = repo / '.agents/setup'
        setup.write_text(f'#!/bin/sh\necho $$ > {self.path}/setup.pid\nsleep 30\ntouch {self.path}/setup.done\n')
        setup.chmod(0o755)
        self.git(repo, 'init', '-q', '-b', 'main')
        self.git(repo, 'add', '.')
        self.git(repo, 'commit', '-qm', 'one')
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[dict(declared('start_task', inherit=False),
                                                                expire_ms=120_000)])
        self.lookout()
        self.model.call_script = [('start_task', {'name': 'fix', 'brief': 'Fix it.', 'setup': True})]
        turn = self.daemon.request('submit', bot='calc.lead', request_id='slow', prompt='script')['result']['turn']
        pid = self.path / 'setup.pid'
        deadline = time.monotonic() + 30
        while not pid.exists() or not pid.read_text().strip():
            self.assertLess(time.monotonic(), deadline, 'the setup never ran')
            time.sleep(.05)
        setup_pid = int(pid.read_text())
        self.daemon.request('interrupt', bot='calc.lead', turn=turn)
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'interrupted')
        deadline = time.monotonic() + 10
        stat = Path(f'/proc/{setup_pid}/stat')
        while stat.exists() and stat.read_text().split()[2] != 'Z':
            self.assertLess(time.monotonic(), deadline, 'the setup still runs')
            time.sleep(.05)
        time.sleep(1)
        self.assertFalse((self.path / 'setup.done').exists())
        self.assertEqual(self.daemon.request('bots', name='calc.fix')['result']['bots'], [])

    def test_a_call_that_fails_is_answered_with_its_error(self):
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('show_agent')])
        self.lookout()
        # No Rex or Herdr here to show it in.
        failed = json.loads(self.call('calc.lead', 'show_agent', {'agent': 'calc.fix', 'split': 'sideways'}))
        self.assertEqual(failed['error'], 'not_shown')
        self.assertIn('not a side', failed['detail'])

    def test_a_call_made_before_the_lookout_started_is_answered_once_it_does(self):
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('subscriptions', inherit=False)])
        self.model.call_script = [('subscriptions', {})]
        turn = self.daemon.request('submit', bot='calc.lead', request_id='early', prompt='script')['result']['turn']
        deadline = time.monotonic() + 10
        while not self.daemon.request('tool_calls', tag='sonny')['result']['calls']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.05)
        self.lookout()
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.output('calc.lead', turn)), {'subscriptions': []})

    def test_a_lookout_another_build_started_gives_way(self):
        old = self.lookout()
        (self.path / 'state.sqlite.lookout.sonny').write_text('0.0.1 1')
        new = self.lookout()
        old.wait(timeout=10)
        self.assertIsNone(new.poll())
        self.assertNotEqual((self.path / 'state.sqlite.lookout.sonny').read_text(), '0.0.1 1')


if __name__ == '__main__':
    unittest.main()
