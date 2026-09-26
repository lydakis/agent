"""Tool approval: gated calls wait for an answer, run or are denied, and park."""
import json
import os
import shlex
import subprocess
import time
import unittest

from bench.targets import clean_env
from tests.test_runtime import ODD_CALL_ID, ModelFixture


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ApprovalTests(ModelFixture):
    def gated(self, extra=(), bot='Bob', **gate):
        client = self.client('echo,shell,wait', extra=extra)
        gate = {'approve': ['shell'], 'approver': 'manual', **gate}
        created = client.request('create', bot=bot, workspace=str(self.path), **gate)['result']
        self.assertEqual(created['gates'], [{'tag': gate['approver'], 'tools': gate['approve'],
                                             **({'expire_ms': gate['approve_expire_ms']}
                                                if 'approve_expire_ms' in gate else {})}])
        return client

    def announced(self, client, turn, failed=None):
        return client.receive(lambda m: m.get('event') == 'approval_requested' and m.get('turn') == turn
                              and m['data'].get('failed') == failed)['data']['calls']

    def answer(self, client, turn, call_id, decision='allow', request=1, **extra):
        return client.request('answer', bot=extra.pop('bot', 'Bob'), turn=turn, call_id=call_id,
                              request=request, decision=decision, by='test', **extra)

    def tool_output(self, client, call_id, bot='Bob'):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        completed = [e for e in events if e['event'] == 'tool_completed' and e['data']['call_id'] == call_id][-1]
        output = client.request('item', bot=bot, node=completed['data']['node'])['result']['output']
        return completed['data'], json.loads(output)

    def events(self, client, turn, kind, bot='Bob'):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        return [e['data'] for e in events if e.get('turn') == turn and e['event'] == kind]

    def test_an_ungated_bot_announces_nothing(self):
        client = self.client('echo,shell')
        created = client.request('create', bot='Bob', workspace=str(self.path))['result']
        self.assertEqual(created['gates'], [])
        turn = client.request('submit', bot='Bob', request_id='t', prompt='shell:printf hi')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.events(client, turn, 'approval_requested'), [])
        self.assertNotIn('approvals', self.events(client, turn, 'tool_started')[0])
        self.assertEqual(client.request('approvals')['result'], {'approvals': [], 'next_after': None})
        self.assertEqual(client.request('stats')['result']['approval_requests'], 0)
        self.assertIn('approvals', client.ready['capabilities'])
        self.assertEqual(client.ready['limits']['approval_hold_ms'], 2000)

    def test_an_allowed_call_runs_and_records_who_allowed_it(self):
        client = self.gated()
        turn = client.request('submit', bot='Bob', request_id='t', prompt='shell:printf hi')['result']['turn']
        [call] = self.announced(client, turn)
        self.assertEqual({k: call[k] for k in ('call_id', 'request', 'gates', 'name')},
                         {'call_id': 'shell-1', 'request': 1, 'gates': ['manual'], 'name': 'shell'})
        self.assertIsInstance(call['node'], int)
        # Pending calls list their arguments from the planned item.
        [pending] = client.request('approvals', bot='Bob')['result']['approvals']
        self.assertEqual((pending['call_id'], pending['gates'], pending['request']), ('shell-1', ['manual'], 1))
        self.assertEqual(json.loads(pending['arguments'])['command'], 'printf hi')
        self.assertEqual(client.request('stats')['result']['approval_requests'], 1)
        # A wrong request number, tag, or call changes nothing.
        self.assertEqual(self.answer(client, turn, 'shell-1', request=2)['error'], 'no_pending_approval')
        self.assertEqual(self.answer(client, turn, 'shell-1', tag='auto')['error'], 'no_pending_approval')
        self.assertEqual(self.answer(client, turn, 'nope')['error'], 'no_pending_approval')
        self.assertEqual(self.answer(client, turn, 'shell-1', decision='maybe')['error'], 'invalid_decision')
        answered = self.answer(client, turn, 'shell-1')['result']
        self.assertEqual((answered['decision'], answered['pending']), ('allow', []))
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        [started] = self.events(client, turn, 'tool_started')
        [approval] = started['approvals']
        self.assertEqual((approval['tag'], approval['by']), ('manual', 'test'))
        self.assertGreaterEqual(approval['waited_ms'], 0)
        self.assertEqual(self.tool_output(client, 'shell-1')[1]['stdout'], 'hi')
        self.assertEqual(client.request('approvals')['result']['approvals'], [])
        self.assertEqual(self.answer(client, turn, 'shell-1')['error'], 'stale_turn')
        # The live verdict took no park: the turn never waited.
        self.assertEqual(self.events(client, turn, 'turn_waiting'), [])

    def test_a_denial_is_the_result_and_the_turn_goes_on(self):
        client = self.gated()
        turn = client.request('submit', bot='Bob', request_id='t', prompt='shell:touch ran')['result']['turn']
        self.announced(client, turn)
        self.answer(client, turn, 'shell-1', decision='deny', reason='not in this repo')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        completed, output = self.tool_output(client, 'shell-1')
        self.assertEqual(output, {'error': 'approval_denied', 'detail': 'not in this repo'})
        self.assertTrue(completed['denied'])
        self.assertEqual(completed['approvals'][0]['by'], 'test')
        self.assertEqual(self.events(client, turn, 'tool_started'), [])
        self.assertFalse((self.path / 'ran').exists())

    def test_a_slow_verdict_parks_the_turn_and_resumes_it(self):
        client = self.gated(extra=('--approval-hold-ms', '100'))
        turn = client.request('submit', bot='Bob', request_id='t', prompt='shell:printf late')['result']['turn']
        waiting = client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        self.assertEqual(waiting['data'], {'call_id': 'shell-1', 'approval': True, 'deadline_ms': None})
        stats = client.request('stats')['result']
        self.assertEqual((stats['active_turns'], stats['waiting_turns']), (0, 1))
        self.assertEqual(client.request('resume', bot='Bob')['result']['status'], 'waiting')
        self.assertEqual(len(client.request('approvals')['result']['approvals']), 1)
        self.assertEqual(self.answer(client, turn, 'shell-1', request=0)['error'], 'approval_superseded')
        self.answer(client, turn, 'shell-1')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'shell-1')[1]['stdout'], 'late')
        self.assertEqual(len(self.events(client, turn, 'turn_resumed')), 1)

    def test_an_expired_gate_denies_the_call_and_ends_the_turn(self):
        for hold in ('2000', '50'):
            with self.subTest(hold=hold):
                self.setUp()
                client = self.gated(extra=('--approval-hold-ms', hold), approve_expire_ms=300)
                turn = client.request('submit', bot='Bob', request_id='t',
                                      prompt='multi:touch first|touch second')['result']['turn']
                calls = self.announced(client, turn)
                self.assertEqual([c['call_id'] for c in calls], ['shell-1', 'shell-2'])
                finished = client.finished(turn)['data']
                self.assertEqual((finished['status'], finished['error']), ('interrupted', 'approval_expired'))
                completed, output = self.tool_output(client, 'shell-1')
                self.assertEqual(output, {'error': 'approval_denied', 'detail': 'not reviewed: no verdict'})
                self.assertTrue(completed['expired'])
                self.assertIsNone(completed['approvals'][0]['by'])
                # The rest of the round never ran.
                self.assertTrue(self.tool_output(client, 'shell-2')[0]['cancelled'])
                self.assertFalse((self.path / 'first').exists() or (self.path / 'second').exists())
                self.assertEqual(client.request('approvals')['result']['approvals'], [])
                parked = self.events(client, turn, 'turn_waiting')
                self.assertEqual(len(parked), 1 if hold == '50' else 0)
                client.close()

    def test_a_failed_call_voids_verdicts_for_the_rest_of_its_round(self):
        client = self.gated()
        turn = client.request('submit', bot='Bob', request_id='t',
                              prompt='multi:false|printf second')['result']['turn']
        self.assertEqual([c['request'] for c in self.announced(client, turn)], [1, 1])
        # Answered for the round as planned, before the first call runs.
        self.answer(client, turn, 'shell-2')
        self.answer(client, turn, 'shell-1')
        [again] = self.announced(client, turn, failed='shell-1')
        self.assertEqual((again['call_id'], again['request']), ('shell-2', 2))
        self.assertEqual(self.answer(client, turn, 'shell-2')['error'], 'approval_superseded')
        self.answer(client, turn, 'shell-2', request=2)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'shell-2')[1]['stdout'], 'second')

    def test_a_verdict_for_a_later_call_waits_while_the_turn_waits_on_a_handle(self):
        client = self.gated()
        client.request('create', bot='Alice', workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='slow')['result']
        turn = client.request('submit', bot='Bob', request_id='t',
                              prompt=f"waitshell:{alice['handle']}|printf after")['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        # Committed while parked on the wait; it does not wake the turn.
        self.assertEqual(self.answer(client, turn, 'shell-1')['result']['pending'], [])
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'shell-1')[1]['stdout'], 'after')
        [started] = [s for s in self.events(client, turn, 'tool_started') if s['name'] == 'shell']
        self.assertEqual(started['approvals'][0]['by'], 'test')
        self.assertEqual(len(self.events(client, turn, 'turn_resumed')), 1)

    def test_gates_accumulate_and_every_gate_must_allow(self):
        client = self.gated()
        fork = client.request('fork', source='Bob', bot='Carol', workspace=str(self.path), approve=['shell', 'echo'],
                              approver='second', approve_expire_ms=60000)['result']
        self.assertEqual(fork['gates'], [{'tag': 'manual', 'tools': ['shell']},
                                         {'tag': 'second', 'tools': ['echo', 'shell'], 'expire_ms': 60000}])
        # A bot created from a gated bot's shell keeps the gates its tools meet.
        bob = client.request('resume', bot='Bob')['result']
        child = client.request('create', bot='Dan', workspace=str(self.path), tools=['echo', 'shell'],
                               created_by='Bob', created_by_id=bob['id'])['result']
        self.assertEqual(child['gates'], [{'tag': 'manual', 'tools': ['shell']}])
        reader = client.request('create', bot='Eve', workspace=str(self.path), tools=['echo'],
                                created_by='Bob', created_by_id=bob['id'])['result']
        self.assertEqual(reader['gates'], [])
        self.assertEqual(client.request('create', bot='Fay', approve=['read'], approver='manual')['error'],
                         'approve_not_in_tools')
        self.assertEqual(client.request('create', bot='Fay', approve=['shell'])['error'], 'invalid_gate')
        turn = client.request('submit', bot='Carol', request_id='t', prompt='shell:printf both')['result']['turn']
        [call] = self.announced(client, turn)
        self.assertEqual(call['gates'], ['manual', 'second'])
        self.assertEqual(self.answer(client, turn, 'shell-1', bot='Carol')['error'], 'approval_tag_required')
        first = self.answer(client, turn, 'shell-1', bot='Carol', tag='manual')['result']
        self.assertEqual(first['pending'], ['second'])
        self.assertEqual(self.answer(client, turn, 'shell-1', bot='Carol', tag='manual')['error'],
                         'approval_already_answered')
        # Each listed call names only the gates still unanswered.
        self.assertEqual(client.request('approvals', tag='manual')['result']['approvals'], [])
        [pending] = client.request('approvals', tag='second')['result']['approvals']
        self.assertEqual(pending['gates'], ['second'])
        self.assertIsInstance(pending['expires_ms'], int)
        self.answer(client, turn, 'shell-1', bot='Carol', tag='second', decision='deny', reason='no')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'shell-1', bot='Carol')[1]['detail'], 'no')

    def test_a_parked_verdict_survives_restart_and_interrupt_cancels_it(self):
        client = self.gated(extra=('--approval-hold-ms', '0'))
        client.request('create', bot='Dan', workspace=str(self.path), approve=['shell'], approver='manual')
        turn = client.request('submit', bot='Bob', request_id='t', prompt='shell:printf restarted')['result']['turn']
        dan = client.request('submit', bot='Dan', request_id='d', prompt='shell:touch never')['result']['turn']
        for parked in (turn, dan):
            client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == parked)
        client.close(kill=True)
        client = self.client('echo,shell,wait')
        self.assertEqual(len(client.request('approvals')['result']['approvals']), 2)
        self.answer(client, turn, 'shell-1')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'shell-1')[1]['stdout'], 'restarted')
        self.assertTrue(client.request('interrupt', bot='Dan', turn=dan)['result']['parked'])
        self.assertEqual(client.finished(dan)['data']['status'], 'interrupted')
        self.assertTrue(self.tool_output(client, 'shell-1', bot='Dan')[0]['cancelled'])
        self.assertEqual(client.request('approvals')['result']['approvals'], [])
        self.assertEqual(self.answer(client, dan, 'shell-1', bot='Dan')['error'], 'stale_turn')


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ApprovalCliTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.store = self.path / 'state.sqlite'
        self.common = ['--store', str(self.store), '--provider', f'openai=responses,{self.url}',
                       '--model', 'openai/synthetic-model', '--tools', 'echo,shell,wait']
        self.addCleanup(lambda: self.agent('shutdown', '--store', str(self.store), check=False))

    def agent(self, *args, check=True, env=None):
        result = subprocess.run([str(self.binary), *args], env={**clean_env(), **(env or {})},
                                capture_output=True, text=True, timeout=30, cwd=self.path)
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return result

    def test_manual_mode_gates_every_tool_but_the_bots_own_records(self):
        submitted = json.loads(self.agent('run', *self.common, '--new', '--bot', 'Bob', '--detach',
                                          'shell:printf ok', env={'AGENT_APPROVAL': 'manual'}).stdout)
        [bob] = [b for b in json.loads(self.agent('ls', '--store', str(self.store)).stdout) if b['name'] == 'Bob']
        self.assertEqual(bob['gates'], [{'tag': 'manual', 'tools': ['shell']}])
        deadline = time.monotonic() + 10
        while True:
            pending = json.loads(self.agent('approvals', '--store', str(self.store)).stdout)
            if pending or time.monotonic() > deadline:
                break
            time.sleep(.05)
        [call] = pending
        pretty = self.agent('approvals', '--store', str(self.store), '--pretty').stdout
        self.assertIn(f"agent answer --bot Bob --turn {submitted['turn']} --call shell-1 --request 1", pretty)
        self.assertIn('printf ok', pretty)
        inside = self.agent('answer', '--store', str(self.store), '--bot', 'Bob', '--turn', str(call['turn']),
                            '--call', 'shell-1', '--request', '1', 'allow', check=False,
                            env={'AGENT_BOT': 'Bob', 'AGENT_BOT_ID': '1'})
        self.assertEqual(inside.returncode, 1)
        self.assertIn('answer_in_tool_shell', inside.stderr)
        answered = json.loads(self.agent('answer', '--store', str(self.store), '--bot', 'Bob', '--turn',
                                         str(call['turn']), '--call', 'shell-1', '--request', '1',
                                         'allow').stdout)
        self.assertEqual((answered['decision'], answered['pending']), ('allow', []))
        result = self.agent('wait', '--store', str(self.store), submitted['handle'])
        self.assertEqual(json.loads(result.stdout)['results'][submitted['handle']]['status'], 'completed')

    def test_a_printed_answer_keeps_any_call_id_one_word(self):
        submitted = json.loads(self.agent('run', *self.common, '--new', '--bot', 'Bob', '--detach',
                                          'oddshell:printf ok', env={'AGENT_APPROVAL': 'manual'}).stdout)
        deadline = time.monotonic() + 10
        while not (pending := json.loads(self.agent('approvals', '--store', str(self.store)).stdout)):
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.05)
        self.assertEqual([c['call_id'] for c in pending], [ODD_CALL_ID])
        pretty = self.agent('approvals', '--store', str(self.store), '--pretty').stdout
        [line] = [line for line in pretty.splitlines() if ' · agent answer ' in line]
        command = line.split(' · ', 1)[1].replace(
            'agent answer', f'{shlex.quote(str(self.binary))} answer --store {shlex.quote(str(self.store))}', 1)
        pasted = subprocess.run(['bash', '-c', command.replace('allow|deny', 'allow')], cwd=self.path,
                                env=clean_env(), capture_output=True, text=True, timeout=30)
        self.assertEqual(pasted.returncode, 0, pasted.stderr)
        self.assertEqual(json.loads(pasted.stdout)['pending'], [])
        self.assertFalse((self.path / 'pwned').exists())
        result = self.agent('wait', '--store', str(self.store), submitted['handle'])
        self.assertEqual(json.loads(result.stdout)['results'][submitted['handle']]['status'], 'completed')

    def test_modes_are_validated_before_anything_is_created(self):
        auto = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--approval', 'auto', 'hi', check=False)
        self.assertEqual(auto.returncode, 1)
        self.assertIn('approval_mode_unsupported', auto.stderr)
        full = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--approve', 'shell', 'hi', check=False)
        self.assertEqual(full.returncode, 2)
        self.assertIn('--approval manual', full.stderr)
        unknown = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--approval', 'some', 'hi', check=False)
        self.assertEqual(unknown.returncode, 2)
        existing = self.agent('run', *self.common, '--bot', 'Bob', '--approval', 'manual', 'hi', check=False)
        self.assertEqual(existing.returncode, 2)
        self.assertIn('creation options', existing.stderr)
        listed = self.agent('ls', '--store', str(self.store), check=False)
        bots = json.loads(listed.stdout) if listed.returncode == 0 else []
        self.assertFalse(any(b['name'] == 'Bob' for b in bots))


if __name__ == '__main__':
    unittest.main()
