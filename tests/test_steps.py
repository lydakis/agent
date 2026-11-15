"""Daemon steps: a script's calls of its bot's own tools, which the daemon
decides as the same call from the model, runs, records and answers with
what the model would have read (`run_step`)."""
import json
import os
import queue
import shutil
import sqlite3
import sys
import time
import unittest

from bench.socket_client import Connection, SocketClient
from tests.test_runtime import ModelFixture

# A script tool whose daemon steps the bot's own gates judge, each by its
# own tool.
EXEC = {'name': 'exec', 'description': 'Run a script.',
        'parameters': {'type': 'object', 'properties': {'code': {'type': 'string'}},
                       'required': ['code'], 'additionalProperties': False},
        'answered_by': 'sonny', 'steps': True}


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class DaemonStepTests(ModelFixture):
    def script(self, tools='echo,shell', tool=EXEC, settings=None, client=None, bot_tools=None, **create):
        client = client or self.client(tools, settings=settings)
        if bot_tools is not None:
            create['tools'] = bot_tools
        created = client.request('create', bot='Lead', workspace=str(self.path), declare=[tool], **create)
        self.assertIn('result', created, created)
        self.model.call_script = [('exec', {'code': 'run()'})]
        turn = client.request('submit', bot='Lead', request_id='t', prompt='script')['result']['message']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn)
        return client, turn

    def step(self, client, turn, step='1', name='echo', arguments=None, call_id='script-0'):
        return client.request('run_step', bot='Lead', turn=turn, call_id=call_id, step=step, name=name,
                              arguments={'text': 'hi'} if arguments is None else arguments)

    def ask(self, client, turn, step='1', name='shell', arguments=None, bot='Lead'):
        """Send a run_step without waiting for it; its id, to receive the reply by."""
        client.next_id += 1
        request = dict(id=client.next_id, op='run_step', bot=bot, turn=turn, call_id='script-0',
                       step=step, name=name, arguments=arguments or {'command': 'sleep 30'})
        client.process.stdin.write(json.dumps(request) + '\n')
        client.process.stdin.flush()
        return client.next_id

    def reply(self, client, request, timeout=5):
        return client.receive(lambda m: m.get('id') == request, timeout)

    def answer(self, client, turn, output='done'):
        return client.request('tool_result', bot='Lead', turn=turn, call_id='script-0', output=output)

    def events(self, client, turn, kind):
        events = client.request('events', bot='Lead', after=0, limit=256)['result']['events']
        return [e['data'] for e in events if e.get('turn') == turn and e['event'] == kind]

    def ended(self, client, turn, call_id, timeout=5):
        deadline = time.monotonic() + timeout
        while True:
            done = [d for d in self.events(client, turn, 'step_completed') if d['call_id'] == call_id]
            if done:
                return done[0]
            self.assertLess(time.monotonic(), deadline, f'{call_id} never ended')
            time.sleep(.02)

    def running(self, client, timeout=5):
        deadline = time.monotonic() + timeout
        while client.request('stats')['result']['steps']['commands'] == 0:
            self.assertLess(time.monotonic(), deadline, 'no step command started')
            time.sleep(.01)

    def test_an_echo_step_runs_at_once_and_answers_what_the_model_would_read(self):
        client, turn = self.script()
        step = self.step(client, turn)
        self.assertEqual(step['result'], {'call_id': 'script-0/1', 'output': 'hi', 'failed': False})
        # Decided and ended as events a judge reads, and a row in the trail.
        [decided] = self.events(client, turn, 'step_decided')
        self.assertEqual({k: decided[k] for k in ('call_id', 'step_of', 'name', 'decision', 'daemon', 'arguments')},
                         {'call_id': 'script-0/1', 'step_of': 'script-0', 'name': 'echo', 'decision': 'allow',
                          'daemon': True, 'arguments': '{"text":"hi"}'})
        [done] = self.events(client, turn, 'step_completed')
        self.assertEqual((done['call_id'], done['step_of'], done.get('failed')), ('script-0/1', 'script-0', None))
        rows = client.request('audit', bot='Lead')['result']['rows']
        [row] = [r for r in rows if r['step'] == '1']
        self.assertEqual((row['tool'], row['approved'], row['outcome'], row['call_id']),
                         ('echo', 'nobody', 'completed', 'script-0'))
        # Steps one after another, each its own.
        self.assertEqual(self.step(client, turn, step='2', arguments={'text': 'again'})['result']['output'], 'again')
        self.assertEqual(self.step(client, turn, step='1')['error'], 'step_exists')
        self.assertEqual(client.request('stats')['result']['steps']['in_flight'], 0)
        self.answer(client, turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_a_shell_step_runs_in_the_turn_s_folder_with_its_environment(self):
        client, turn = self.script()
        step = self.step(client, turn, name='shell',
                         arguments={'command': 'pwd; echo "$AGENT_BOT $AGENT_TURN"; exit 3'})['result']
        self.assertEqual(step['call_id'], 'script-0/1')
        self.assertTrue(step['failed'])
        output = json.loads(step['output'])
        self.assertEqual(output['stdout'].split('\n')[:2], [str(self.path.resolve()), f'Lead {turn}'])
        self.assertEqual((output['exit_code'], output['success']), (3, False))
        self.assertTrue(self.ended(client, turn, 'script-0/1')['failed'])
        self.answer(client, turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_file_steps_read_write_edit_and_patch_in_the_turn_s_folder(self):
        client, turn = self.script(tools='echo,shell,read,write,edit,apply_patch')
        wrote = self.step(client, turn, step='w', name='write',
                          arguments={'path': 'notes.md', 'content': 'one\ntwo\n'})['result']
        self.assertEqual((wrote['output'], wrote['failed']), ('wrote 8 bytes to notes.md', False))
        self.assertEqual((self.path / 'notes.md').read_text(), 'one\ntwo\n')
        edited = self.step(client, turn, step='e', name='edit',
                           arguments={'path': 'notes.md', 'old': 'two', 'new': 'three'})['result']
        self.assertFalse(edited['failed'], edited)
        patch = '*** Begin Patch\n*** Add File: more.md\n+four\n*** End Patch'
        patched = self.step(client, turn, step='p', name='apply_patch', arguments={'input': patch})['result']
        self.assertFalse(patched['failed'], patched)
        self.assertEqual((self.path / 'more.md').read_text(), 'four\n')
        read = self.step(client, turn, step='r', name='read', arguments={'path': 'notes.md'})['result']
        self.assertFalse(read['failed'])
        self.assertIn('three', read['output'])
        self.assertNotIn('two', read['output'])
        # A file the edit cannot find fails as the direct call does.
        missed = self.step(client, turn, step='m', name='edit',
                           arguments={'path': 'notes.md', 'old': 'nowhere', 'new': 'x'})['result']
        self.assertTrue(missed['failed'])
        # Each is decided and ended as its direct call, by its own name.
        decided = [(d['call_id'], d['name']) for d in self.events(client, turn, 'step_decided')]
        self.assertEqual(decided, [('script-0/w', 'write'), ('script-0/e', 'edit'), ('script-0/p', 'apply_patch'),
                                   ('script-0/r', 'read'), ('script-0/m', 'edit')])
        self.assertEqual(len(self.events(client, turn, 'step_completed')), 5)
        # A step reads files; a retained output is read with `artifact`.
        self.assertEqual(self.step(client, turn, step='a', name='read',
                                   arguments={'artifact': f'{turn}/script-0/w/stdout'})['error'], 'invalid_step')
        self.answer(client, turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_a_read_step_of_a_picture_answers_with_the_picture(self):
        from tests.test_images import png
        (self.path / 'shot.png').write_bytes(png(4, 4))
        client, turn = self.script(tools='echo,read', bot_tools=['read'])
        ran = self.step(client, turn, name='read', arguments={'path': 'shot.png'})['result']
        self.assertTrue(ran['output'].startswith('shot.png: a PNG image, 4×4'), ran)
        [picture] = ran['parts']
        self.assertEqual((picture['type'], picture['media_type']), ('image', 'image/png'))

    @unittest.skipUnless(shutil.which('bwrap') or sys.platform == 'darwin', 'sandboxes need bubblewrap')
    def test_a_sandboxed_bot_s_file_steps_stay_in_its_sandbox(self):
        client, turn = self.script(tools='echo,shell,read,write,web_fetch',
                                   settings={'sandbox': 'workspace-write'})
        inside = self.step(client, turn, step='1', name='write',
                           arguments={'path': 'in.txt', 'content': 'ok'})['result']
        self.assertFalse(inside['failed'], inside)
        outside = self.path.parent / f'{self.path.name}-outside.txt'
        escaped = self.step(client, turn, step='2', name='write',
                            arguments={'path': str(outside), 'content': 'no'})['result']
        self.assertTrue(escaped['failed'], escaped)
        self.assertFalse(outside.exists())
        # Off the network, its web tool is not one it may call.
        fetched = self.step(client, turn, step='3', name='web_fetch', arguments={'url': 'https://example.test/'})
        self.assertEqual(fetched['error'], 'tool_not_available')
        self.answer(client, turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    @unittest.skipUnless(shutil.which('bwrap') or sys.platform == 'darwin', 'sandboxes need bubblewrap')
    def test_a_file_step_through_a_link_into_the_workspace_is_judged_where_it_leads(self):
        outside = self.path.parent / f'{self.path.name}-links'
        outside.mkdir()
        self.addCleanup(shutil.rmtree, outside, True)
        (outside / 'ws').symlink_to(self.path)
        client, turn = self.script(tools='echo,write', bot_tools=['write'], approve=['write'],
                                   approver='manual', settings={'sandbox': 'workspace-write'})
        # Spelled outside, it leads into the workspace: confined, no gate asked.
        ran = self.step(client, turn, name='write',
                        arguments={'path': str(outside / 'ws' / 'via.txt'), 'content': 'ok'})['result']
        self.assertFalse(ran['failed'], ran)
        self.assertEqual((self.path / 'via.txt').read_text(), 'ok')
        self.answer(client, turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_a_tool_the_bot_lacks_is_not_a_step_it_may_run(self):
        client, turn = self.script(tools='echo,shell,write', bot_tools=['echo', 'shell'])
        refused = self.step(client, turn, name='write', arguments={'path': 'x', 'content': 'y'})
        self.assertEqual(refused['error'], 'tool_not_available')
        self.assertFalse((self.path / 'x').exists())
        self.answer(client, turn)

    def test_a_step_is_refused_at_once_for_what_it_cannot_be(self):
        client, turn = self.script()
        ask = dict(bot='Lead', turn=turn, call_id='script-0', step='1', name='echo', arguments={'text': 'hi'})
        for change, error in [({'step': 'a b'}, 'invalid_step'), ({'name': 'wait'}, 'invalid_step'), ({'name': 'read'}, 'invalid_tool_arguments'),
                              ({'name': 'mcp__x__y'}, 'invalid_step'), ({'arguments': []}, 'invalid_step'),
                              ({'arguments': {'nope': 1}}, 'invalid_tool_arguments'),
                              ({'name': 'shell', 'arguments': {'command': 'x', 'background': True}}, 'invalid_step'),
                              ({'arguments': {'text': 'x' * 65537}}, 'step_arguments_limit'),
                              ({'call_id': 'nope'}, 'no_pending_call'), ({'bot': 'Other'}, 'turn_not_found'),
                              ({'call_id': ''}, 'invalid_call_id')]:
            self.assertEqual(client.request('run_step', **(ask | change))['error'], error, change)
        # One daemon step of a call at a time.
        request = self.ask(client, turn)
        self.running(client)
        self.assertEqual(client.request('run_step', **(ask | {'step': '2'}))['error'], 'step_in_flight')
        self.answer(client, turn)
        self.assertEqual(self.reply(client, request)['error'], 'step_stopped')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(client.request('run_step', **ask)['error'], 'stale_turn')
        self.assertEqual(client.request('stats')['result']['steps']['in_flight'], 0)
        # Nothing was recorded for a step refused at once.
        self.assertEqual([d['call_id'] for d in self.events(client, turn, 'step_decided')], ['script-0/1'])

    def test_a_step_needs_a_tool_the_bot_may_call_and_a_call_that_takes_steps(self):
        client, turn = self.script(tools='echo')
        self.assertEqual(self.step(client, turn, name='shell', arguments={'command': 'true'})['error'],
                         'tool_not_available')
        self.answer(client, turn)
        client.finished(turn)
        self.setUp()
        client, turn = self.script(tool={k: v for k, v in EXEC.items() if k != 'steps'})
        self.assertEqual(self.step(client, turn)['error'], 'invalid_step')

    def test_a_step_is_decided_as_its_direct_call_would_be(self):
        rules = {'prefix_rules': [{'pattern': ['rm'], 'decision': 'forbidden', 'source': 'default.rules:1'}]}
        client, turn = self.script(settings={'rules': rules})
        refused = self.step(client, turn, name='shell', arguments={'command': 'rm -rf x'})['result']
        self.assertEqual((refused['decision'], refused['failed']), ('deny', True))
        self.assertEqual(json.loads(refused['output'])['error'], 'command_forbidden')
        [decided] = self.events(client, turn, 'step_decided')
        self.assertEqual((decided['decision'], decided['error'], decided.get('denied')),
                         ('deny', 'command_forbidden', None))
        self.assertEqual(self.events(client, turn, 'step_completed'), [])
        # A refused step's name is spent too: one name, one step, while its call lives.
        self.assertEqual(self.step(client, turn, name='shell', arguments={'command': 'ls'})['error'], 'step_exists')
        # A gate that names shell holds a shell step, as it holds the call.
        self.answer(client, turn)
        client.finished(turn)
        self.setUp()
        client, turn = self.script(approve=['shell'], approver='manual')
        request = self.ask(client, turn, arguments={'command': 'echo allowed'})
        [call] = client.receive(lambda m: m.get('event') == 'approval_requested')['data']['calls']
        self.assertEqual((call['call_id'], call['step_of'], call['name'], call['gates']),
                         ('script-0/1', 'script-0', 'shell', ['manual']))
        [listed] = client.request('approvals', bot='Lead')['result']['approvals']
        self.assertTrue(listed['daemon'])
        client.request('answer', bot='Lead', turn=turn, call_id='script-0/1', request=1, decision='allow', by='test')
        ran = self.reply(client, request)['result']
        self.assertEqual(json.loads(ran['output'])['stdout'], 'allowed\n')
        request = self.ask(client, turn, step='2', arguments={'command': 'echo denied'})
        client.receive(lambda m: m.get('event') == 'approval_requested')
        client.request('answer', bot='Lead', turn=turn, call_id='script-0/2', request=1, decision='deny',
                       reason='not now', by='test')
        denied = self.reply(client, request)['result']
        self.assertEqual((denied['decision'], denied['reason'], denied['failed']), ('deny', 'not now', True))
        self.assertEqual(json.loads(denied['output'])['error'], 'approval_denied')
        self.assertEqual([(d['call_id'], d['decision'], d.get('daemon')) for d in self.events(client, turn, 'step_decided')],
                         [('script-0/1', 'allow', True), ('script-0/2', 'deny', True)])
        self.assertEqual([d['call_id'] for d in self.events(client, turn, 'step_completed')], ['script-0/1'])
        self.assertEqual(self.step(client, turn, step='2')['error'], 'step_exists')
        # An echo step no gate names runs at once beside them.
        self.assertEqual(self.step(client, turn, step='3')['result']['output'], 'hi')
        self.answer(client, turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_a_bot_that_never_asks_has_a_held_step_refused(self):
        client, turn = self.script(approve=['shell'], approver='manual', settings={'approval_policy': 'never'})
        refused = self.step(client, turn, name='shell', arguments={'command': 'true'})['result']
        self.assertEqual(json.loads(refused['output'])['error'], 'approval_refused')
        [decided] = self.events(client, turn, 'step_decided')
        self.assertTrue(decided['denied'])
        self.answer(client, turn)
        client.finished(turn)

    def test_the_script_tool_s_gate_judges_its_daemon_steps_only_when_it_says_so(self):
        client, turn = self.script(approve=['exec'], approver='manual')
        self.assertEqual(self.step(client, turn)['result']['output'], 'hi')
        self.answer(client, turn)
        client.finished(turn)
        self.setUp()
        client, turn = self.script(tool=dict(EXEC, gate_daemon_steps=True), approve=['exec'], approver='manual')
        request = self.ask(client, turn, name='echo', arguments={'text': 'judged'})
        [call] = client.receive(lambda m: m.get('event') == 'approval_requested')['data']['calls']
        self.assertEqual((call['call_id'], call['name'], call['gates']), ('script-0/1', 'echo', ['manual']))
        client.request('answer', bot='Lead', turn=turn, call_id='script-0/1', request=1, decision='allow', by='test')
        self.assertEqual(self.reply(client, request)['result']['output'], 'judged')
        self.answer(client, turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_the_call_answered_stops_its_step_first(self):
        client, turn = self.script()
        request = self.ask(client, turn)
        self.running(client)
        self.assertIn('result', self.answer(client, turn))
        stopped = self.reply(client, request)
        self.assertEqual((stopped['error'], stopped['detail']), ('step_stopped', 'the call was answered'))
        done = self.ended(client, turn, 'script-0/1')
        self.assertEqual((done['failed'], done['error']), (True, 'step_stopped'))
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        rows = client.request('audit', bot='Lead')['result']['rows']
        [row] = [r for r in rows if r['step'] == '1']
        self.assertEqual(row['outcome'], 'failed')

    def test_the_call_running_out_of_time_stops_its_step(self):
        client, turn = self.script(tool=dict(EXEC, expire_ms=1500))
        request = self.ask(client, turn)
        self.running(client)
        stopped = self.reply(client, request, timeout=10)
        self.assertEqual(stopped['error'], 'step_stopped')
        self.ended(client, turn, 'script-0/1')
        self.assertEqual(client.finished(turn, timeout=10)['data']['status'], 'completed')

    def test_an_interrupt_stops_the_step_with_its_call(self):
        client, turn = self.script()
        request = self.ask(client, turn)
        self.running(client)
        client.request('interrupt', bot='Lead', turn=turn)
        self.assertEqual(self.reply(client, request, timeout=10)['error'], 'step_stopped')
        self.ended(client, turn, 'script-0/1')
        self.assertEqual(client.request('stats')['result']['steps']['in_flight'], 0)

    def test_a_pause_stops_a_running_step_and_refuses_new_ones(self):
        client, turn = self.script()
        request = self.ask(client, turn)
        self.running(client)
        client.request('pause', reason='look first')
        stopped = self.reply(client, request, timeout=10)
        self.assertEqual(stopped['result']['failed'], True)
        self.assertEqual(json.loads(stopped['result']['output'])['error'], 'daemon_paused')
        self.assertEqual(self.step(client, turn, step='2')['error'], 'daemon_paused')
        client.request('resume')

    def test_a_step_goes_with_the_session_that_asked(self):
        daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'echo,shell')
        self.addCleanup(daemon.close)
        daemon.request('create', bot='Lead', workspace=str(self.path), declare=[EXEC])
        self.model.call_script = [('exec', {'code': 'run()'})]
        turn = daemon.request('submit', bot='Lead', request_id='t', prompt='script')['result']['message']
        deadline = time.monotonic() + 5
        while not daemon.request('tool_calls', tag='sonny')['result']['calls']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        asker = Connection(daemon.socket_path)
        asker.socket.sendall((json.dumps(dict(id=1, op='run_step', bot='Lead', turn=turn, call_id='script-0',
                                              step='1', name='shell', arguments={'command': 'sleep 30'})) + '\n').encode())
        while daemon.request('stats')['result']['steps']['commands'] == 0:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        asker.close()
        done = self.ended(daemon, turn, 'script-0/1')
        self.assertEqual(done['error'], 'step_stopped')
        self.assertEqual(daemon.request('stats')['result']['steps']['in_flight'], 0)
        self.assertIn('result', daemon.request('tool_result', bot='Lead', turn=turn, call_id='script-0', output='ok'))
        self.assertEqual(daemon.finished(turn)['data']['status'], 'completed')

    def test_a_step_command_waits_in_line_while_a_turn_s_command_holds_the_slot(self):
        client = self.client('echo,shell', extra=('--max-processes', '1'))

        def respond(request, user):
            if user != 'hold':
                return None
            answered = any(i.get('type') == 'function_call_output' for i in request['input'])
            return 'held' if answered else [['shell', {'command': 'sleep 2'}]]
        self.model.respond = respond
        client.request('create', bot='Other', workspace=str(self.path), tools=['shell'])
        held = client.request('submit', bot='Other', request_id='h', prompt='hold')['result']['message']
        deadline = time.monotonic() + 5
        while client.request('stats')['result']['running_processes'] == 0:
            self.assertLess(time.monotonic(), deadline, "the turn's command never started")
            time.sleep(.01)
        client, turn = self.script(client=client)
        request = self.ask(client, turn, arguments={'command': 'echo ran'})
        while client.request('stats')['result']['steps']['in_line'] == 0:
            self.assertLess(time.monotonic(), deadline, 'the step never lined up')
            time.sleep(.01)
        # It waits as an entry in the line, not as a started command.
        self.assertEqual(client.request('stats')['result']['steps']['commands'], 0)
        ran = self.reply(client, request, timeout=10)['result']
        self.assertEqual(json.loads(ran['output'])['stdout'], 'ran\n')
        self.assertEqual(client.finished(held)['data']['status'], 'completed')

    def test_no_process_bound_lets_every_call_s_command_step_run_at_once(self):
        client = self.client('echo,shell', extra=('--max-processes', '0'))
        self.model.call_script = [('exec', {'code': 'run()'})]
        turns = {}
        for bot in ('Lead', 'Second'):
            client.request('create', bot=bot, workspace=str(self.path), declare=[EXEC])
            turns[bot] = client.request('submit', bot=bot, request_id='t', prompt='script')['result']['message']
        deadline = time.monotonic() + 5
        while len(client.request('tool_calls', tag='sonny')['result']['calls']) < 2:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        slow = self.ask(client, turns['Lead'], arguments={'command': 'sleep 30'})
        self.running(client)
        quick = self.ask(client, turns['Second'], arguments={'command': 'echo quick'}, bot='Second')
        ran = self.reply(client, quick, timeout=5)['result']
        self.assertEqual(json.loads(ran['output'])['stdout'], 'quick\n')
        client.request('interrupt', bot='Lead', turn=turns['Lead'])
        self.assertEqual(self.reply(client, slow, timeout=10)['error'], 'step_stopped')

    def test_another_bot_s_answer_to_the_call_stops_nothing(self):
        client, turn = self.script()
        client.request('create', bot='Other', workspace=str(self.path))
        request = self.ask(client, turn)
        self.running(client)
        wrong = client.request('tool_result', bot='Other', turn=turn, call_id='script-0', output='no')
        self.assertEqual(wrong['error'], 'turn_not_found')
        self.assertEqual(client.request('stats')['result']['steps']['commands'], 1)
        self.assertEqual(self.events(client, turn, 'step_completed'), [])
        self.answer(client, turn)
        self.assertEqual(self.reply(client, request, timeout=10)['error'], 'step_stopped')

    def test_an_interrupted_call_s_steps_leave_no_rows(self):
        client, turn = self.script()
        self.assertIn('result', self.step(client, turn))
        client.request('interrupt', bot='Lead', turn=turn)
        self.assertEqual(client.finished(turn, timeout=10)['data']['status'], 'interrupted')
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            self.assertEqual(db.execute('SELECT count(*) FROM steps').fetchone()[0], 0)

    def test_a_step_cut_off_by_a_restart_ends_of_unknown_outcome(self):
        client, turn = self.script()
        self.ask(client, turn)
        self.running(client)
        client.close(kill=True)
        client = self.client('echo,shell')
        done = self.ended(client, turn, 'script-0/1')
        self.assertEqual((done['failed'], done['error']), (True, 'tool_outcome_unknown'))

    def test_a_step_s_long_output_is_kept_under_its_id_until_its_call_is_answered(self):
        client, turn = self.script()
        step = self.step(client, turn, name='shell', arguments={'command': 'head -c 200000 /dev/zero | tr "\\0" a'})
        output = json.loads(step['result']['output'])
        self.assertEqual(output['artifacts'], [f'{turn}/script-0/1/stdout'])
        page = client.request('artifact', bot='Lead', turn=turn, call_id='script-0/1', stream='stdout', limit=10)
        self.assertEqual(page['result']['text'], 'a' * 10)
        self.answer(client, turn)
        client.finished(turn)
        gone = client.request('artifact', bot='Lead', turn=turn, call_id='script-0/1', stream='stdout')
        self.assertIn('error', gone)

    def test_a_lifted_call_keeps_its_step_past_its_turn(self):
        client, turn = self.script(tool=dict(EXEC, lift=True), settings={'approval_hold_ms': 0})
        request = self.ask(client, turn, arguments={'command': 'sleep 1; echo lifted'})
        self.running(client)
        steered = client.request('submit', bot='Lead', request_id='s', prompt='go on', delivery='steer')
        self.assertIn('result', steered)
        ran = self.reply(client, request, timeout=15)['result']
        self.assertEqual(json.loads(ran['output'])['stdout'], 'lifted\n')


if __name__ == '__main__':
    unittest.main()
