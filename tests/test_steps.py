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

from bench.runtime_client import node_item
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
    def script(self, tools='echo,shell', tool=EXEC, settings=None, client=None, bot_tools=None,
               bot_settings=None, **create):
        client = client or self.client(tools, settings=settings)
        if bot_tools is not None:
            create['tools'] = bot_tools
        if bot_settings is not None:
            create['settings'] = bot_settings
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

    def test_a_picture_too_large_for_one_reply_is_kept_whole_under_the_step(self):
        import base64
        from tests.test_images import noise
        heavy = noise(512)
        self.assertLess(len(heavy), 1024 * 1024)
        self.assertGreater(len(heavy) * 4 // 3, 1024 * 1024)
        (self.path / 'noise.png').write_bytes(heavy)
        client, turn = self.script(tools='echo,read', bot_tools=['read'])
        ran = self.step(client, turn, name='read', arguments={'path': 'noise.png'})['result']
        self.assertIn(f'too large for the reply kept as {turn}/script-0/1/picture-1', ran['output'])
        self.assertNotIn('parts', ran)
        data, offset = '', 0
        while True:
            page = client.request('artifact', bot='Lead', turn=turn, call_id='script-0/1',
                                  stream='picture-1', offset=offset)['result']
            data += page['text']
            offset = page['next_offset']
            if page['done']:
                break
        # The very bytes the direct call reads: nothing encoded again.
        self.assertEqual(base64.b64decode(data), heavy)

    def test_a_picture_that_fits_only_beside_a_short_request_id_is_kept_for_a_long_one(self):
        # A picture sized to fit a reply with a short id, not one beside a
        # request id of 128 control characters (six JSON bytes each): its
        # step still answers, the picture kept under the step.
        import struct
        import zlib
        from tests.test_images import png

        def chunk(kind, data):
            return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
        small = png(4, 4)
        encoded = 1024 * 1024 - 900
        size = encoded // 4 * 3
        filler = size - len(small) - 12
        picture = small[:-12] + chunk(b'tEXt', b'k\x00' + b'x' * (filler - 2)) + small[-12:]
        self.assertEqual(len(picture), size)
        (self.path / 'wide.png').write_bytes(picture)
        client, turn = self.script(tools='echo,read', bot_tools=['read'])
        request = '\u0001' * 128
        line = dict(id=request, op='run_step', bot='Lead', turn=turn, call_id='script-0', step='1',
                    name='read', arguments={'path': 'wide.png'})
        client.process.stdin.write(json.dumps(line) + '\n')
        client.process.stdin.flush()
        replied = self.reply(client, request, timeout=10)
        self.assertIn('result', replied, replied)
        ran = replied['result']
        self.assertIn(f'too large for the reply kept as {turn}/script-0/1/picture-1', ran['output'])
        self.assertNotIn('parts', ran)

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

    def test_a_step_naming_no_valid_bot_is_refused_before_it_is_held(self):
        client, turn = self.script()
        refused = client.request('run_step', bot='x' * 200, turn=turn, call_id='script-0', step='1',
                                 name='echo', arguments={'text': 'hi'})
        self.assertEqual(refused['error'], 'invalid_name')
        self.assertEqual(client.request('stats')['result']['steps']['in_flight'], 0)

    def test_an_interrupt_stops_the_step_with_its_call(self):
        client, turn = self.script()
        request = self.ask(client, turn)
        self.running(client)
        client.request('interrupt', bot='Lead', turn=turn)
        self.assertEqual(self.reply(client, request, timeout=10)['error'], 'step_stopped')
        self.ended(client, turn, 'script-0/1')
        self.assertEqual(client.request('stats')['result']['steps']['in_flight'], 0)

    def test_an_interrupt_of_a_parked_turn_stops_its_steps(self):
        client, turn = self.script(settings={'approval_hold_ms': 0})
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        deadline = time.monotonic() + 5
        while client.request('stats')['result']['active_turns']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        request = self.ask(client, turn)
        self.running(client)
        self.assertIn('result', client.request('interrupt', bot='Lead', turn=turn))
        self.assertEqual(self.reply(client, request, timeout=10)['error'], 'step_stopped')
        self.ended(client, turn, 'script-0/1')
        self.assertEqual(client.request('stats')['result']['steps']['in_flight'], 0)

    def test_a_pause_stops_a_running_step_and_refuses_new_ones(self):
        client, turn = self.script()
        request = self.ask(client, turn)
        self.running(client)
        client.request('pause', reason='look first')
        stopped = self.reply(client, request, timeout=10)
        self.assertEqual((stopped['error'], stopped['detail']), ('step_stopped', 'the pause stopped it'))
        done = self.ended(client, turn, 'script-0/1')
        self.assertEqual((done['failed'], done['error']), (True, 'step_stopped'))
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
        replied = self.reply(client, request, timeout=10)
        self.assertIn('result', replied, replied)
        ran = replied['result']
        self.assertEqual(json.loads(ran['output'])['stdout'], 'ran\n')
        self.assertEqual(client.finished(held)['data']['status'], 'completed')

    def test_a_step_that_takes_no_process_never_waits_behind_a_command_in_line(self):
        client = self.client('echo,shell,read', extra=('--max-processes', '1'))
        self.model.call_script = [('exec', {'code': 'run()'})]
        turns = {}
        for bot in ('Lead', 'Second'):
            client.request('create', bot=bot, workspace=str(self.path), declare=[EXEC])
            turns[bot] = client.request('submit', bot=bot, request_id='t', prompt='script')['result']['message']
        deadline = time.monotonic() + 5
        while len(client.request('tool_calls', tag='sonny')['result']['calls']) < 2:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        # Lead's command holds the one slot; Second's waits in line for it.
        slow = self.ask(client, turns['Lead'], arguments={'command': 'sleep 30'})
        self.running(client)
        lined = self.ask(client, turns['Second'], arguments={'command': 'echo later'}, bot='Second')
        while client.request('stats')['result']['steps']['in_line'] == 0:
            self.assertLess(time.monotonic(), deadline, 'the command never lined up')
            time.sleep(.01)
        # A read and an echo of a third call run at once regardless.
        client.request('create', bot='Third', workspace=str(self.path), declare=[EXEC])
        third = client.request('submit', bot='Third', request_id='t', prompt='script')['result']['message']
        while len(client.request('tool_calls', tag='sonny')['result']['calls']) < 3:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        (self.path / 'note.txt').write_text('hello\n')
        for step, (name, arguments) in enumerate([('echo', {'text': 'hi'}), ('read', {'path': 'note.txt'})], 1):
            started = time.monotonic()
            ran = client.request('run_step', bot='Third', turn=third, call_id='script-0', step=str(step),
                                 name=name, arguments=arguments)
            self.assertIn('result', ran, ran)
            self.assertLess(time.monotonic() - started, 2)
        self.assertEqual(client.request('stats')['result']['steps']['in_line'], 1)
        client.request('interrupt', bot='Lead', turn=turns['Lead'])
        self.assertEqual(self.reply(client, slow, timeout=10)['error'], 'step_stopped')
        ran = self.reply(client, lined, timeout=10)['result']
        self.assertEqual(json.loads(ran['output'])['stdout'], 'later\n')

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

    def test_an_interrupted_call_s_steps_leave_no_rows_or_outputs(self):
        client, turn = self.script()
        self.assertIn('result', self.step(client, turn))
        long = self.step(client, turn, step='2', name='shell',
                         arguments={'command': 'head -c 200000 /dev/zero | tr "\\0" a'})
        self.assertEqual(json.loads(long['result']['output'])['artifacts'], [f'{turn}/script-0/2/stdout'])
        client.request('interrupt', bot='Lead', turn=turn)
        self.assertEqual(client.finished(turn, timeout=10)['data']['status'], 'interrupted')
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            self.assertEqual(db.execute('SELECT count(*) FROM steps').fetchone()[0], 0)
            self.assertEqual(db.execute('SELECT count(*) FROM artifacts').fetchone()[0], 0)
        gone = client.request('artifact', bot='Lead', turn=turn, call_id='script-0/2', stream='stdout')
        self.assertIn('error', gone)

    def test_a_step_asked_once_its_call_s_answer_is_held_is_refused(self):
        client, turn = self.script(settings={'approval_hold_ms': 250})
        # Sent together: the answer is held for the running turn, or taken
        # by it, before anything stores it, when the step is decided.
        client.next_id += 1
        answer = dict(id=client.next_id, op='tool_result', bot='Lead', turn=turn, call_id='script-0',
                      output='done')
        client.next_id += 1
        step = dict(id=client.next_id, op='run_step', bot='Lead', turn=turn, call_id='script-0', step='1',
                    name='echo', arguments={'text': 'hi'})
        client.process.stdin.write(json.dumps(answer) + '\n' + json.dumps(step) + '\n')
        client.process.stdin.flush()
        self.assertIn('result', self.reply(client, answer['id']))
        refused = self.reply(client, step['id'])
        self.assertIn(refused.get('error'), ('call_already_answered', 'no_pending_call'), refused)
        self.assertEqual(self.events(client, turn, 'step_decided'), [])
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

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

    def offered(self):
        return [t['name'] for t in self.model.requests.get(timeout=3)['tools']]

    def drain(self):
        while not self.model.requests.empty():
            self.model.requests.get_nowait()

    def test_steps_only_star_shows_the_model_the_script_tool_and_keeps_the_rest_for_steps(self):
        client, turn = self.script(tools='echo,shell,read', bot_tools=['echo', 'shell', 'read'],
                                   bot_settings={'steps_only': ['*']})
        self.assertEqual(self.offered(), ['exec'])
        # Hidden from the model, still a step.
        self.assertEqual(self.step(client, turn)['result']['output'], 'hi')
        ran = self.step(client, turn, step='2', name='shell', arguments={'command': 'echo ok'})['result']
        self.assertEqual(json.loads(ran['output'])['stdout'], 'ok\n')
        self.answer(client, turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        # A direct call of a hidden tool is refused, with no one asked.
        self.drain()
        prompt = 'calls:' + json.dumps([['shell', {'command': 'echo direct > direct'}]])
        direct = client.request('submit', bot='Lead', request_id='d', prompt=prompt)['result']['message']
        self.assertEqual(client.finished(direct)['data']['status'], 'completed')
        self.assertFalse((self.path / 'direct').exists())
        [done] = self.events(client, direct, 'tool_completed')
        output = node_item(client, 'Lead', done['node'])['result']['output']
        self.assertEqual(json.loads(output)['error'], 'tool_not_available')
        record = client.request('inspect', bot='Lead')['result']
        self.assertEqual(record['settings']['steps_only'], ['*'])

    def test_a_direct_call_of_a_gated_hidden_tool_asks_no_one(self):
        client = self.client('echo,shell')
        client.request('create', bot='Kept', workspace=str(self.path), tools=['echo', 'shell'],
                       declare=[EXEC], approve=['shell'], approver='person',
                       settings={'steps_only': ['shell']})
        prompt = 'calls:' + json.dumps([['shell', {'command': 'echo asked > asked'}]])
        turn = client.request('submit', bot='Kept', request_id='k', prompt=prompt)['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        events = client.request('events', bot='Kept', after=0, limit=256)['result']['events']
        self.assertFalse([e for e in events if e['event'] == 'approval_requested'])
        self.assertFalse((self.path / 'asked').exists())

    def test_steps_only_star_hides_nothing_without_a_script_tool(self):
        client = self.client('echo,shell')
        client.request('create', bot='Plain', workspace=str(self.path), tools=['echo', 'shell'],
                       settings={'steps_only': ['*']})
        turn = client.request('submit', bot='Plain', request_id='p', prompt='hello')['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.offered(), ['echo', 'shell'])

    def test_steps_only_by_name_hides_a_daemon_tool_and_a_client_tool(self):
        lookup = {'name': 'lookup', 'description': 'Look up.', 'answered_by': 'sonny',
                  'parameters': {'type': 'object', 'properties': {}, 'additionalProperties': False}}
        client = self.client('echo,shell')
        client.request('create', bot='Named', workspace=str(self.path), tools=['echo', 'shell'],
                       declare=[EXEC, lookup], settings={'steps_only': ['shell', 'lookup']})
        turn = client.request('submit', bot='Named', request_id='n', prompt='hello')['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.offered(), ['echo', 'exec'])
        # Shown again from the next turn once the setting is cleared.
        self.assertIn('result', client.request('configure', bot='Named', settings={'steps_only': []}))
        self.drain()
        turn = client.request('submit', bot='Named', request_id='m', prompt='hello')['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.offered(), ['echo', 'shell', 'exec', 'lookup'])

    def test_a_bot_made_by_another_takes_its_steps_only_unless_its_client_sets_one(self):
        client = self.client('echo,shell')
        maker = client.request('create', bot='Maker', workspace=str(self.path), tools=['echo', 'shell'],
                               declare=[EXEC], settings={'steps_only': ['*']})['result']['bot_id']
        made = client.request('create', bot='Made', workspace=str(self.path), tools=['echo'],
                              created_by='Maker', created_by_id=maker)['result']
        self.assertEqual(made['settings']['steps_only'], ['*'])
        own = client.request('create', bot='Own', workspace=str(self.path), tools=['echo'],
                             created_by='Maker', created_by_id=maker, settings={'steps_only': []})['result']
        self.assertEqual(own['settings']['steps_only'], [])
        fork = client.request('fork', source='Maker', bot='Copy')['result']
        self.assertEqual(fork['settings']['steps_only'], ['*'])
        refused = client.request('configure', bot='Maker', settings={'steps_only': ['shell', 'shell']})
        self.assertEqual(refused['error'], 'invalid_setting')

    def test_a_direct_call_of_a_hidden_client_tool_never_reaches_its_client(self):
        lookup = {'name': 'lookup', 'description': 'Look up.', 'answered_by': 'sonny',
                  'parameters': {'type': 'object', 'properties': {}, 'additionalProperties': False}}
        client = self.client('echo')
        client.request('create', bot='Lead', workspace=str(self.path), tools=['echo'],
                       declare=[EXEC, lookup], settings={'steps_only': ['lookup']})
        prompt = 'calls:' + json.dumps([['lookup', {}]])
        turn = client.request('submit', bot='Lead', request_id='h', prompt=prompt)['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        [started] = self.events(client, turn, 'tool_started')
        self.assertNotIn('answered_by', started)
        self.assertEqual(client.request('tool_calls', tag='sonny')['result']['calls'], [])
        [done] = self.events(client, turn, 'tool_completed')
        output = node_item(client, 'Lead', done['node'])['result']['output']
        self.assertEqual(json.loads(output)['error'], 'tool_not_available')

    def test_tools_lists_each_tool_s_schema_and_whether_it_runs_as_a_step(self):
        client = self.client('echo,shell,read,history')
        tools = {t['name']: t for t in client.request('tools')['result']['tools']}
        self.assertLessEqual({'echo', 'history', 'read', 'shell'}, set(tools))
        self.assertTrue(tools['shell']['step'] and tools['read']['step'] and tools['echo']['step'])
        self.assertFalse(tools['history']['step'])
        self.assertIn('command', tools['shell']['parameters']['properties'])
        self.assertTrue(tools['echo']['description'])


if __name__ == '__main__':
    unittest.main()
