"""The tools Sonny answers, end to end: an agent's call waits in the daemon,
the tool host runs it in the agent's folder, and its result or error is the
call's output."""
import json
import os
import queue
import subprocess
import sys
import time
import unittest
from pathlib import Path

from bench.runtime_client import node_item
from bench.socket_client import SocketClient
from bench.targets import clean_env
from tests.test_images import image
from tests.test_runtime import ModelFixture


def declared(name, inherit=True):
    """One of Sonny's answered tools, as small as the daemon takes it."""
    tool = {'name': name, 'description': name, 'answered_by': 'sonny',
            'parameters': {'type': 'object'}}
    if not inherit:
        tool['inherit'] = False
    return tool


def running(pid):
    """Whether `pid` runs: not gone and not a zombie. It can go between any
    two looks at /proc, so a stat that vanishes means gone."""
    try:
        return Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0] != 'Z'
    except FileNotFoundError:
        return False


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ServedToolTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.sonny = self.binary.parent / 'sonny'
        self.store = self.path / 'state.sqlite'
        self.daemon = SocketClient(self.binary, self.store, self.url, 'echo')
        self.addCleanup(self.daemon.close)
        self.script = []

    def companion(self, command, started, **extra):
        env = {**clean_env(), 'HOME': str(self.path), 'AGENT_BIN': str(self.binary),
               'GIT_AUTHOR_NAME': 't', 'GIT_AUTHOR_EMAIL': 't@example.com',
               'GIT_COMMITTER_NAME': 't', 'GIT_COMMITTER_EMAIL': 't@example.com', **extra}
        service = subprocess.Popen([str(self.sonny), command, '--store', str(self.store),
                                    '--socket', str(self.daemon.socket_path)],
                                   env=env, stdout=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (service.kill(), service.wait(timeout=5), service.stdout.close()))
        self.assertEqual(json.loads(service.stdout.readline())['event'], started)
        return service

    def host(self, **extra):
        """The tool host, which answers the calls."""
        return self.companion('tool-host', 'serving', **extra)

    def lookout(self, **extra):
        """The lookout, which tells leads and checks goals."""
        return self.companion('lookout', 'watching', **extra)

    def call(self, bot, name, arguments, **submit):
        """`bot` calls `name` once; the call's output. The script runs on
        from the calls a bot's earlier turns made."""
        self.script.append((name, arguments))
        self.model.call_script = self.script
        turn = self.daemon.request('submit', bot=bot, request_id=f'{name}-{time.monotonic()}',
                                   prompt='script', **submit)['result']['turn']
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
        # A lead in `auto`.
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('start_task', inherit=False),
                                                           declared('show_agent')],
                            approve=['show_agent'], approver='auto')
        self.host()
        output = self.call('calc.lead', 'start_task', {'name': 'fix', 'brief': f'Fix {repo}/calc.py.'})
        self.assertTrue(output.startswith('Started calc.fix: '), output)
        worktree = self.path / '.agent/worktrees/calc.fix'
        [task] = self.daemon.request('bots', name='calc.fix')['result']['bots']
        self.assertEqual((task['created_by'], task['workspace']), ('calc.lead', str(worktree)))
        self.assertEqual(task['model'], 'synthetic-model')
        # It has the tools every agent has, not its lead's own.
        record = self.daemon.request('inspect', bot='calc.fix')['result']
        self.assertEqual([tool['name'] for tool in record['declared']], ['show_agent'])
        # It works in a sandbox, reaching the network, and may commit to the
        # repository its worktree shares.
        self.assertEqual(record['settings']['sandbox'], 'workspace-write')
        self.assertEqual(record['settings']['writable'], [str((repo / '.git').resolve())])
        self.assertTrue(record['settings']['network'])
        # The brief names the task's copy, not the lead's folder.
        turns = self.daemon.request('turns', bot='calc.fix')['result']['turns']
        prompts = self.daemon.request('prompts', bot='calc.fix', turn=turns[0]['turn'])['result']['prompts']
        self.assertEqual(prompts[0]['text'], f'Fix {worktree}/calc.py.')
        # With a task still there, another start says why it is separate
        # work, or is refused with the tasks it could go to.
        refused = json.loads(self.call('calc.lead', 'start_task', {'name': 'docs', 'brief': 'Docs.'}))
        self.assertEqual(refused['error'], 'tasks_exist')
        self.assertIn('calc.fix (', refused['detail'])
        self.assertEqual(self.daemon.request('bots', name='calc.docs')['result']['bots'], [])
        # Its name is taken now, so a second start is refused, and nothing
        # is left behind.
        refused = json.loads(self.call('calc.lead', 'start_task',
                                       {'name': 'fix', 'brief': 'Again.', 'reason': 'Another fix.'}))
        self.assertEqual(refused['error'], 'task_not_started')
        self.assertTrue(worktree.is_dir())
        # In `full`, with no gate to ask, a task runs unconfined, as its
        # lead does.
        self.daemon.request('configure', bot='calc.lead', approve=[])
        output = self.call('calc.lead', 'start_task', {'name': 'free', 'brief': 'Go.', 'reason': 'Separate.'})
        self.assertTrue(output.startswith('Started calc.free: '), output)
        free = self.daemon.request('inspect', bot='calc.free')['result']['settings']
        self.assertEqual((free['sandbox'], free['writable'], free['network']), ('danger-full-access', [], False))

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
        self.host()
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
        while running(setup_pid):
            self.assertLess(time.monotonic(), deadline, 'the setup still runs')
            time.sleep(.05)
        time.sleep(1)
        self.assertFalse((self.path / 'setup.done').exists())
        self.assertEqual(self.daemon.request('bots', name='calc.fix')['result']['bots'], [])

    def test_a_tool_host_that_gives_way_mid_setup_stops_it_and_the_next_serves_the_call(self):
        repo = self.path / 'calc'
        (repo / '.agents').mkdir(parents=True)
        setup = repo / '.agents/setup'
        # The first setup is slow; the one the next host runs is not.
        pids = self.path / 'setup.pids'
        setup.write_text(f'#!/bin/sh\nfirst=$(test -e {pids} || echo 1)\necho $$ >> {pids}\n'
                         f'test -n "$first" && sleep 30\nexit 0\n')
        setup.chmod(0o755)
        self.git(repo, 'init', '-q', '-b', 'main')
        self.git(repo, 'add', '.')
        self.git(repo, 'commit', '-qm', 'one')
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[dict(declared('start_task', inherit=False),
                                                                expire_ms=120_000)])
        old = self.host()
        self.model.call_script = [('start_task', {'name': 'fix', 'brief': 'Fix it.', 'setup': True})]
        turn = self.daemon.request('submit', bot='calc.lead', request_id='slow', prompt='script')['result']['turn']
        deadline = time.monotonic() + 30
        while not pids.exists() or not pids.read_text().strip():
            self.assertLess(time.monotonic(), deadline, 'the setup never ran')
            time.sleep(.05)
        first = int(pids.read_text().split()[0])
        (self.path / 'state.sqlite.tool-host.sonny').write_text('0.0.1 1')
        self.host()
        old.wait(timeout=10)
        deadline = time.monotonic() + 10
        while running(first):
            self.assertLess(time.monotonic(), deadline, 'the first setup still runs')
            time.sleep(.05)
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        self.assertTrue(self.output('calc.lead', turn).startswith('Started calc.fix: '))
        self.assertEqual(len(pids.read_text().split()), 2)

    def test_a_start_the_person_denies_runs_no_setup(self):
        repo = self.path / 'calc'
        (repo / '.agents').mkdir(parents=True)
        setup = repo / '.agents/setup'
        setup.write_text(f'#!/bin/sh\ntouch {self.path}/setup.ran\n')
        setup.chmod(0o755)
        self.git(repo, 'init', '-q', '-b', 'main')
        self.git(repo, 'add', '.')
        self.git(repo, 'commit', '-qm', 'one')
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('start_task', inherit=False)],
                            approve=['start_task'], approver='manual')
        self.host()
        self.model.call_script = [('start_task', {'name': 'fix', 'brief': 'Fix it.', 'setup': True})]
        turn = self.daemon.request('submit', bot='calc.lead', request_id='deny', prompt='script')['result']['turn']
        deadline = time.monotonic() + 10
        while not (asked := self.daemon.request('approvals', bot='calc.lead')['result']['approvals']):
            self.assertLess(time.monotonic(), deadline, 'no approval was asked')
            time.sleep(.05)
        self.assertEqual(asked[0]['name'], 'start_task')
        self.daemon.request('answer', bot='calc.lead', turn=turn, call_id='script-0',
                            request=asked[0]['request'], decision='deny', by='test')
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        self.assertFalse((self.path / 'setup.ran').exists())
        self.assertEqual(self.daemon.request('bots', name='calc.fix')['result']['bots'], [])

    def test_a_call_that_fails_is_answered_with_its_error(self):
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('show_agent')])
        self.host()
        # No Rex or Herdr here to show it in.
        failed = json.loads(self.call('calc.lead', 'show_agent', {'agent': 'calc.fix', 'split': 'sideways'}))
        self.assertEqual(failed['error'], 'not_shown')
        self.assertIn('not a side', failed['detail'])

    def test_a_call_made_before_the_host_started_is_answered_once_it_does(self):
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('subscriptions', inherit=False)])
        self.model.call_script = [('subscriptions', {})]
        turn = self.daemon.request('submit', bot='calc.lead', request_id='early', prompt='script')['result']['turn']
        deadline = time.monotonic() + 10
        while not self.daemon.request('tool_calls', tag='sonny')['result']['calls']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.05)
        self.host()
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.output('calc.lead', turn)), {'subscriptions': [], 'sources': []})

    def test_an_agent_s_script_calls_an_mcp_server_the_host_keeps_up(self):
        server = Path(__file__).with_name('mcp_server.py')
        (self.path / '.agent').mkdir(exist_ok=True)
        (self.path / '.agent/config.toml').write_text(
            f'[sonny.mcp_servers.test]\ncommand = {json.dumps(sys.executable)}\nargs = [{json.dumps(str(server))}]\n'
            f'env = {{ MCP_STARTS = {json.dumps(str(self.path / "starts"))} }}\n')
        # The tool as Sonny gives it to the agents it opens.
        script = json.loads(subprocess.run([str(self.sonny), 'mcp', 'tool'], env={**clean_env(), 'HOME': str(self.path)},
                                           check=True, capture_output=True, text=True).stdout)
        self.assertEqual((script['name'], script['answered_by']), ('exec', 'sonny'))
        self.assertIn('## test', script['description'])
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[script])
        self.host()
        said = self.call('calc.lead', 'exec', 'const r = await tools.mcp__test__echo({text: "hi"});\ntext(r.content[0].text);')
        self.assertTrue(said.startswith('Script completed\n'), said)
        self.assertTrue(said.endswith('MCP calls: test 1\nOutput:\nhi'), said)
        summed = self.call('calc.lead', 'exec', 'text((await tools.mcp__test__add({a: 1, b: 2})).structuredContent.sum);')
        self.assertTrue(summed.endswith('Output:\n3'), summed)
        # One server, started by the first script and kept for the next.
        self.assertEqual(len((self.path / 'starts').read_text().splitlines()), 1)
        refused = json.loads(self.call('calc.lead', 'exec', '// @exec: {"wait": 1}\ntext(1);'))
        self.assertEqual(refused['error'], 'exec_refused')
        self.assertIn('`wait`', refused['detail'])

    def test_a_script_s_pictures_reach_the_model_in_order_with_its_text(self):
        server = Path(__file__).with_name('mcp_server.py')
        (self.path / '.agent').mkdir(exist_ok=True)
        (self.path / '.agent/config.toml').write_text(
            f'[sonny.mcp_servers.test]\ncommand = {json.dumps(sys.executable)}\nargs = [{json.dumps(str(server))}]\n')
        script = json.loads(subprocess.run([str(self.sonny), 'mcp', 'tool'], env={**clean_env(), 'HOME': str(self.path)},
                                           check=True, capture_output=True, text=True).stdout)
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[script])
        self.host()
        shot = image()
        self.model.requests = queue.Queue()
        self.call('calc.lead', 'exec', f"text('before'); image('data:image/png;base64,{shot['data']}'); text('after');")
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        [answered] = [i for i in requests[-1]['input'] if i.get('call_id') == 'script-0' and 'output' in i]
        kinds = [part['type'] for part in answered['output']]
        self.assertEqual(kinds, ['input_text', 'input_image', 'input_text'], answered)
        self.assertTrue(answered['output'][0]['text'].endswith('Output:\nbefore'), answered)
        self.assertEqual(answered['output'][1]['image_url'], f"data:image/png;base64,{shot['data']}")
        self.assertEqual(answered['output'][2]['text'], 'after')

    def test_an_agent_saves_memory_through_the_host(self):
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('memory')])
        self.host()
        fact = {'name': 'prefers-short', 'type': 'feedback', 'description': 'Short replies.',
                'source': 'the person, 2026-10-28', 'text': '-'}
        saved = json.loads(self.call('calc.lead', 'memory', {'save': fact, 'user': True}))
        self.assertTrue(saved, saved)
        memory = self.path / '.agents/memory'
        self.assertIn('\n-\n', (memory / 'prefers-short.md').read_text())
        self.assertIn('prefers-short (feedback', (memory / 'MEMORY.md').read_text())
        shown = json.loads(self.call('calc.lead', 'memory', {'user': True}))
        self.assertIn('Short replies.', shown['user']['index'])
        refused = json.loads(self.call('calc.lead', 'memory', {'save': dict(fact, name='x', type='mood')}))
        self.assertEqual(refused['error'], 'memory_refused')

    def test_agents_share_a_thread_in_the_commons_and_hear_of_replies(self):
        for bot in ('calc.a', 'shop.b'):
            self.daemon.request('create', bot=bot, workspace=str(self.path), model='openai/synthetic-model',
                                instructions='Agent.', declare=[declared('commons')])
        self.host()
        scripts = {}

        def turn(bot, arguments):
            # The script runs on from the calls this bot's earlier turns made.
            self.script = scripts.setdefault(bot, [])
            return json.loads(self.call(bot, 'commons', arguments))

        post = {'name': 'fake-home', 'title': 'Live tests want a fake HOME', 'text': 'For both binaries.'}
        self.assertEqual(turn('calc.a', {'post': post})['posts'], 1)
        replied = turn('shop.b', {'reply': {'thread': 'fake-home', 'text': 'Confirmed here too.'}})
        self.assertEqual((replied['posts'], replied['told']), (2, ['calc.a']), replied)
        thread = (self.path / '.agents/commons/fake-home.jsonl').read_text().splitlines()
        self.assertEqual([json.loads(line)['by'] for line in thread], ['calc.a', 'shop.b'])
        # The reply joins calc.a's next model call, and starts no turn.
        turns = self.daemon.request('turns', bot='calc.a')['result']['turns']
        self.assertEqual([t['status'] for t in turns], ['completed', 'attached'], turns)
        self.model.bodies = []
        turn_id = self.daemon.request('submit', bot='calc.a', request_id='next', prompt='hello')['result']['turn']
        self.assertEqual(self.daemon.finished(turn_id)['data']['status'], 'completed')
        sent = b''.join(self.model.bodies)
        self.assertIn(b'shop.b replied in the commons thread fake-home', sent)
        listed = turn('shop.b', {})
        self.assertEqual([t['thread'] for t in listed['threads']], ['fake-home'])

    def test_a_new_thread_invites_an_agent_whose_turns_touch_it(self):
        for bot in ('calc.a', 'shop.b', 'mail.c'):
            self.daemon.request('create', bot=bot, workspace=str(self.path), model='openai/synthetic-model',
                                instructions='Agent.', declare=[declared('commons')])
        # An old agent that once worked on it, and one that did not.
        for bot, prompt in [('shop.b', 'the WAL checkpoint stalls'), ('mail.c', 'hello')]:
            turn = self.daemon.request('submit', bot=bot, request_id=bot, prompt=prompt)['result']['turn']
            self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        self.host()
        post = {'name': 'stalls', 'title': 'Checkpoint stalls under load', 'text': 'Seen this?'}
        started = json.loads(self.call('calc.a', 'commons', {'post': post}))
        self.assertEqual(started['invited'], 'shop.b', started)
        # The note starts its turn.
        deadline = time.monotonic() + 5
        while len(turns := self.daemon.request('turns', bot='shop.b')['result']['turns']) < 2:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.05)
        prompts = self.daemon.request('prompts', bot='shop.b', turn=turns[-1]['turn'])['result']['prompts']
        self.assertEqual(prompts[0]['text'],
                         '[calc.a started the commons thread stalls, "Checkpoint stalls under load", '
                         'which your turns touch on. The invite pays for 1.0M tokens in the thread: while any '
                         'are left, a reply there starts your turn.]')
        self.assertEqual(turns[-1]['budget_tokens'], 1_000_000)
        listed = json.loads(self.call('calc.a', 'commons', {}))
        self.assertEqual(listed['invites'], {'today': 1, 'a_day': 3, 'answered': 0, 'this_week': 1})
        # Its reply earns the commons another invite a day.
        self.call('shop.b', 'commons', {'reply': {'thread': 'stalls', 'text': 'Same here.'}})
        # A thread started in a turn the commons began brings no invite.
        post = {'name': 'more', 'title': 'More checkpoint stalls', 'text': 'And this?'}
        started = json.loads(self.call('shop.b', 'commons', {'post': post}, origin='commons'))
        self.assertIsNone(started['invited'], started)
        listed = json.loads(self.call('mail.c', 'commons', {}))
        self.assertEqual(listed['invites'], {'today': 1, 'a_day': 4, 'answered': 1, 'this_week': 1})
        # mail.c's turn ran the script from its start, so it replied in the
        # thread too: that wakes the invited shop.b while its invite has
        # tokens left, its turn capped at what is left.
        deadline = time.monotonic() + 5
        while not (woken := [t for t in self.daemon.request('turns', bot='shop.b')['result']['turns']
                             if t['request_id'].endswith('.woken')]):
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.05)
        self.assertLess(woken[0]['budget_tokens'], 1_000_000, woken)

    def test_a_lead_made_for_a_subfolder_reads_its_own_memory_and_follows_its_folder(self):
        ws = self.path / 'ws'
        (ws / '.agents').mkdir(parents=True)
        (ws / '.agents/project.toml').write_text('name = "ws"\n')
        (ws / 'api').mkdir()
        memory = self.path / '.agents/memory/projects'
        for name in ['ws', 'api']:
            (memory / name).mkdir(parents=True)
            (memory / name / 'MEMORY.md').write_text(f'# Memory of {name}\n')
        self.daemon.request('create', bot='ws.lead', workspace=str(ws), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('new_project')])
        self.host()
        made = json.loads(self.call('ws.lead', 'new_project', {'folder': str(ws / 'api')}))
        self.assertEqual((made['coordinator'], made['created']), ('api.lead', True), made)
        # Its own project's memory, though the folder above is a project
        # and the new one had no file when its lead was made.
        lead = self.daemon.request('inspect', bot='api.lead')['result']
        self.assertIn('# Memory of api', lead['instructions'])
        self.assertNotIn('# Memory of ws', lead['instructions'])
        # Sonny composed them, so the lead takes its folder's changes when
        # it opens, as one made by opening Sonny there does.
        [kept] = (self.path / '.agent/sonny/instructions').iterdir()
        self.assertTrue(kept.name.endswith(f'-{lead["bot_id"]}'), kept.name)

    def note_for(self, bot, having, timeout=30):
        """The first note the lookout gave `bot` that says `having`."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for turn in self.daemon.request('turns', bot=bot)['result']['turns']:
                text = self.daemon.request('prompts', bot=bot, turn=turn['turn'])['result']['prompts']
                if text and text[0]['text'].startswith('Task updates: ') and having in text[0]['text']:
                    return text[0]['text']
            time.sleep(.2)
        self.fail(f'{bot} heard nothing saying {having!r}')

    def goal_watch(self):
        """calc.fix, calc.lead's task with the goal tool, watched by its
        lead, which hears its turns; the host serving them, and the lookout
        checking the goal: the lookout's process."""
        lead = self.daemon.request('create', bot='calc.lead', workspace=str(self.path),
                                   model='openai/synthetic-model', instructions='Lead.',
                                   declare=[declared('goal')])['result']
        self.daemon.request('create', bot='calc.fix', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Task.', declare=[declared('goal')], created_by='calc.lead',
                            created_by_id=lead['bot_id'])
        folder = self.path / 'state.sqlite.lookout'
        folder.mkdir(exist_ok=True)
        watch = {'name': 'tasks', 'tasks_of': [], 'bots': ['calc.fix'], 'on': ['said', 'ended'], 'updates': True}
        (folder / 'calc.lead.json').write_text(json.dumps({'subscriptions': [watch]}))
        self.host()
        return self.lookout()

    def lines_of(self, path, at_least, timeout=30):
        """The lines of `path` once it has `at_least` of them."""
        deadline = time.monotonic() + timeout
        while not path.exists() or len(path.read_text().splitlines()) < at_least:
            self.assertLess(time.monotonic(), deadline, f'{path.name} has fewer than {at_least} lines')
            time.sleep(.05)
        return path.read_text().splitlines()

    def goal_run(self, check):
        """calc.fix sets itself a goal with `check`: the lead's note once
        the goal ends, and the prompts of calc.fix's turns."""
        self.goal_watch()
        set_ = json.loads(self.call('calc.fix', 'goal', {'set': {'done_when': 'the flag exists', 'check': check}}))
        self.assertIn('goal', set_, set_)
        self.assertEqual(set_['goal']['check'], check)
        note = self.note_for('calc.lead', 'its goal')
        prompts = [self.daemon.request('prompts', bot='calc.fix', turn=t['turn'])['result']['prompts'][0]['text']
                   for t in self.daemon.request('turns', bot='calc.fix')['result']['turns']]
        return note, prompts

    def test_a_goal_keeps_its_agent_working_until_its_check_passes(self):
        # Fails once, leaving the flag that passes it the next time.
        note, prompts = self.goal_run('test -f flag || { touch flag; echo not yet; exit 1; }')
        self.assertEqual(len(prompts), 2, prompts)
        self.assertTrue(prompts[1].startswith('Your goal is not met yet: `test -f flag'), prompts[1])
        self.assertIn('exits 1.\n```\nnot yet\n```\nKeep working until the flag exists.', prompts[1])
        # The lead hears the turn that met it, and nothing of the one before.
        self.assertIn('- calc.fix: turn:calc.fix/2 completed, asked by goal, and met its goal:', note)
        self.assertNotIn('calc.fix/1', note)
        self.assertFalse((self.path / 'state.sqlite.lookout/goals/calc.fix.json').exists())

    def test_a_goal_whose_agent_only_answers_stops(self):
        note, prompts = self.goal_run('false')
        # The turn that set it called the tool; two that only answered end it.
        self.assertEqual(len(prompts), 3, prompts)
        self.assertIn('turn:calc.fix/3 completed, asked by goal, and stopped pursuing its goal: 2 turns in a row',
                      note)
        self.assertIn('its check still exits 1', note)

    def test_a_goal_its_lead_sets_counts_from_the_task_turn_that_ends_next(self):
        self.goal_watch()
        ran = self.path / 'ran'
        set_ = json.loads(self.call('calc.lead', 'goal', {'set': {'bot': 'calc.fix', 'done_when': 'it ran',
                                                                    'check': f'echo . >> {ran}'}}))
        self.assertEqual(set_['goal']['bot'], 'calc.fix', set_)
        # Ends at once, well before the lookout reads its goals again.
        turn = self.daemon.request('submit', bot='calc.fix', request_id='work', prompt='Work.')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        note = self.note_for('calc.lead', 'its goal')
        self.assertIn(f'- calc.fix: turn:calc.fix/{turn} completed, asked by the person, and met its goal:', note)
        self.assertEqual(len(ran.read_text().splitlines()), 1)

    def test_a_turn_that_ends_while_its_goal_is_checked_starts_no_other_check(self):
        self.goal_watch()
        runs = self.path / 'runs'
        # Slow, and passing the second time it runs.
        check = f'echo . >> {runs}; sleep 2; test $(wc -l < {runs}) -ge 2'
        self.call('calc.fix', 'goal', {'set': {'done_when': 'it ran twice', 'check': check}})
        self.lines_of(runs, 1)
        # The person writes in while it runs; the turn ends before it does.
        turn = self.daemon.request('submit', bot='calc.fix', request_id='more', prompt='More.')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        note = self.note_for('calc.lead', 'its goal')
        self.assertIn('asked by goal, and met its goal:', note)
        # One check for the turn that set it, one for the goal's own turn.
        self.assertEqual(len(runs.read_text().splitlines()), 2)

    def test_a_goal_checked_when_its_lookout_gives_way_is_checked_by_the_next(self):
        old = self.goal_watch()
        runs = self.path / 'runs'
        check = f'echo . >> {runs}; sleep 30; exit 1'
        self.call('calc.fix', 'goal', {'set': {'done_when': 'it ran again', 'check': check}})
        self.lines_of(runs, 1)
        # The cursor it keeps stays before the turn's end, which waits on
        # the check.
        [ended] = [e['cursor'] for e in self.daemon.request('events', bot='calc.fix', after=0, limit=256)
                   ['result']['events'] if e['event'] == 'turn_finished']
        saved = self.lines_of(self.path / 'state.sqlite.lookout.cursor', 1)
        self.assertLess(int(saved[0].split()[0]), ended)
        # Another build's lookout takes over, stopping this one's check.
        (self.path / 'state.sqlite.lookout.sonny').write_text('0.0.1 1')
        (self.path / 'state.sqlite.lookout/goals/calc.fix.json').write_text(json.dumps(dict(
            json.loads((self.path / 'state.sqlite.lookout/goals/calc.fix.json').read_text()),
            check=f'echo . >> {runs}')))
        self.lookout()
        old.wait(timeout=10)
        # The turn that set it is checked again, and now passes.
        note = self.note_for('calc.lead', 'its goal')
        self.assertIn('- calc.fix: turn:calc.fix/1 completed, asked by the person, and met its goal:', note)
        self.assertEqual(len(self.lines_of(runs, 2)), 2)

    def test_a_goal_no_one_watches_is_checked_again_after_a_restart(self):
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('goal')])
        self.host()
        old = self.lookout()
        runs = self.path / 'runs'
        self.call('calc.lead', 'goal', {'set': {'done_when': 'it ran again', 'check': f'echo . >> {runs}; sleep 30'}})
        self.lines_of(runs, 1)
        [ended] = [e['cursor'] for e in self.daemon.request('events', bot='calc.lead', after=0, limit=256)
                   ['result']['events'] if e['event'] == 'turn_finished']
        # Past a save of the cursor, which stays before the turn's end
        # though no subscriber holds it.
        time.sleep(1.5)
        saved = self.lines_of(self.path / 'state.sqlite.lookout.cursor', 1)
        self.assertLess(int(saved[0].split()[0]), ended)
        old.kill()
        old.wait(timeout=10)
        self.lookout()
        self.assertEqual(len(self.lines_of(runs, 2)), 2)

    def goal_down(self, **goal):
        """calc.fix and its lead with no lookout running, `goal` on disk as
        the goal tool writes it, and the cursor a lookout that went down
        just now left: what happens next is news to the one that starts."""
        old = self.goal_watch()
        old.kill()
        old.wait(timeout=10)
        fix = self.daemon.request('inspect', bot='calc.fix')['result']
        folder = self.path / 'state.sqlite.lookout/goals'
        folder.mkdir(parents=True, exist_ok=True)
        if 'check' in goal:
            (folder / 'calc.fix.json').write_text(json.dumps(dict(
                bot='calc.fix', bot_id=fix['bot_id'], done_when='it ran twice', workspace=str(self.path),
                set_by='calc.lead', set_ms=1, timeout_s=60, **goal)))
        at = max(e['cursor'] for bot in ('calc.lead', 'calc.fix')
                 for e in self.daemon.request('events', bot=bot, after=0, limit=256)['result']['events'])
        (self.path / 'state.sqlite.lookout.cursor').write_text(f'{at} {at}')
        return fix

    def test_two_goal_turns_that_ended_while_the_lookout_was_down_are_both_checked(self):
        runs = self.path / 'runs'
        self.goal_down(check=f'echo . >> {runs}; test $(wc -l < {runs}) -ge 2')
        first = self.daemon.request('submit', bot='calc.fix', request_id='work', prompt='Work.')['result']['turn']
        self.assertEqual(self.daemon.finished(first)['data']['status'], 'completed')
        # The turn the first check goes on in, made before the lookout went
        # down: its request ID is the one the check asks for again.
        second = self.daemon.request('submit', bot='calc.fix', request_id=f'goal-calc.fix-{first}', prompt='More.',
                                     origin='goal')['result']['turn']
        self.assertEqual(self.daemon.finished(second)['data']['status'], 'completed')
        self.lookout()
        note = self.note_for('calc.lead', 'its goal')
        self.assertIn(f'- calc.fix: turn:calc.fix/{second} completed, asked by goal, and met its goal:', note)
        self.assertEqual(len(self.lines_of(runs, 2)), 2)

    def test_how_a_goal_ended_reaches_the_lead_after_a_restart(self):
        fix = self.goal_down()
        turn = self.daemon.request('submit', bot='calc.fix', request_id='work', prompt='Work.')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        # The goal ended with that turn, and the lookout went down before
        # its lead heard.
        ended = self.path / 'state.sqlite.lookout/goals/ended'
        ended.mkdir(parents=True)
        (ended / 'calc.fix.json').write_text(json.dumps(dict(
            turn=turn, bot_id=fix['bot_id'], check='false', met=False,
            outcome=', and gave up its goal, saying why in that turn')))
        self.lookout()
        note = self.note_for('calc.lead', 'its goal')
        self.assertIn(f'- calc.fix: turn:calc.fix/{turn} completed, asked by the person, and gave up its goal', note)

    @unittest.skipIf(sys.platform == 'darwin', 'launchd keeps triggers on a Mac')
    def test_a_trigger_with_no_systemd_running_is_refused_and_says_why(self):
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('triggers')])
        # A runtime folder no user manager serves.
        runtime = self.path / 'runtime'
        runtime.mkdir()
        self.host(XDG_RUNTIME_DIR=str(runtime))
        refused = json.loads(self.call('calc.lead', 'triggers', {'add': {'every': '30m', 'message': 'Check.'}}))
        self.assertEqual(refused['error'], 'trigger_refused')
        self.assertIn('triggers_unsupported', refused['detail'])
        self.assertEqual(list((self.path / '.config/systemd/user').glob('*.service')), [])

    def test_a_lookout_another_build_started_gives_way(self):
        old = self.lookout()
        (self.path / 'state.sqlite.lookout.sonny').write_text('0.0.1 1')
        new = self.lookout()
        old.wait(timeout=10)
        self.assertIsNone(new.poll())
        self.assertNotEqual((self.path / 'state.sqlite.lookout.sonny').read_text(), '0.0.1 1')

    def test_one_tool_host_a_store_and_the_next_starts_where_it_stopped(self):
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('memory')])
        old = self.host()
        second = subprocess.run([str(self.sonny), 'tool-host', '--store', str(self.store),
                                 '--socket', str(self.daemon.socket_path)],
                                env={**clean_env(), 'HOME': str(self.path)}, capture_output=True, text=True,
                                timeout=10)
        self.assertEqual((second.returncode, json.loads(second.stdout)['event']), (0, 'already_serving'))
        self.call('calc.lead', 'memory', {'user': True})
        saved = self.lines_of(self.path / 'state.sqlite.tool-host.cursor', 1)
        self.assertGreater(int(saved[0]), 0)
        old.kill()
        old.wait(timeout=10)
        self.host()
        self.assertTrue(json.loads(self.call('calc.lead', 'memory', {'user': True})))


if __name__ == '__main__':
    unittest.main()
