"""Tool approval: gated calls wait for an answer, run or are denied, and park."""
import http.server
import json
import os
import queue
import re
import shlex
import subprocess
import threading
import time
import unittest

from bench.runtime_client import Client, node_item
from bench.socket_client import Connection, SocketClient
from bench.targets import clean_env
from tests.test_elision import drain, encoded
from tests.test_runtime import ODD_CALL_ID, ModelFixture, is_summary
from tests.test_turn_compaction import all_events


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ApprovalTests(ModelFixture):
    def gated(self, extra=(), settings=None, bot='Bob', **gate):
        client = self.client('echo,shell,wait', extra=extra, settings=settings)
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
        output = node_item(client, bot, completed['data']['node'])['result']['output']
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
        self.assertEqual(client.request('stats')['result']['approvers'], [])
        self.assertEqual(created['settings']['approval_hold_ms'], 2000)

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
        self.assertEqual((pending['arguments'], pending['arguments_cut'], pending['arguments_omitted']),
                         ({'command': 'printf hi', 'timeout_ms': 2000}, [], 0))
        self.assertEqual(client.request('stats')['result']['approval_requests'], 1)
        # A wrong request number, tag, or call changes nothing.
        self.assertEqual(self.answer(client, turn, 'shell-1', request=2)['error'], 'no_pending_approval')
        self.assertEqual(self.answer(client, turn, 'shell-1', tag='auto')['error'], 'no_pending_approval')
        self.assertEqual(self.answer(client, turn, 'nope')['error'], 'no_pending_approval')
        self.assertEqual(self.answer(client, turn, 'shell-1', decision='maybe')['error'], 'invalid_decision')
        self.assertEqual(self.answer(client, turn, 'shell-1', reason='fine')['error'], 'invalid_reason')
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
        client = self.gated(settings={'approval_hold_ms': 100})
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
        for hold in (2000, 50):
            with self.subTest(hold=hold):
                self.setUp()
                client = self.gated(settings={'approval_hold_ms': hold}, approve_expire_ms=300)
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
                self.assertEqual(len(parked), 1 if hold == 50 else 0)
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

    def test_a_gate_lapses_from_its_announcement_not_when_its_call_comes_up(self):
        # An ungated command outlasts the gate on the call after it, so that
        # call is past its expiry when it comes up and is judged at once.
        client = self.gated(approve=['echo'], approve_expire_ms=300)
        turn = client.request('submit', bot='Bob', request_id='t', prompt='shellecho:sleep 1|hi')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_completed' and m.get('turn') == turn
                       and m['data']['call_id'] == 'shell-1')
        ran = time.monotonic()
        finished = client.finished(turn)
        self.assertEqual((finished['data']['status'], finished['data']['error']), ('interrupted', 'approval_expired'))
        self.assertLess(finished['_received_at'] - ran, 0.2)

    def test_a_successful_call_whose_output_reads_as_a_failure_keeps_the_rounds_verdicts(self):
        # Echoed text is the call's output, not its status.
        client = self.gated(approve=['shell'])
        turn = client.request('submit', bot='Bob', request_id='t',
                              prompt='echoshell:{"success":false,"error":"x"}|printf second')['result']['turn']
        self.announced(client, turn)
        self.answer(client, turn, 'shell-1')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'shell-1')[1]['stdout'], 'second')
        self.assertEqual([e['request'] for c in self.events(client, turn, 'approval_requested') for e in c['calls']], [1])

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
        # A list longer than the daemon's tools is refused before any work on it.
        self.assertEqual(client.request('create', bot='Fay', approve=['shell'] * 20000,
                                        approver='manual')['error'], 'invalid_gate')
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
        completed, output = self.tool_output(client, 'shell-1', bot='Carol')
        self.assertEqual(output['detail'], 'no')
        # The denied call's record keeps the allow that came before the deny.
        self.assertEqual([(a['tag'], a['by'], a['allow']) for a in completed['approvals']],
                         [('manual', 'test', True), ('second', 'test', False)])

    def test_a_partial_verdict_keeps_the_later_gates_lapse(self):
        client = self.gated(settings={'approval_hold_ms': 0}, approve_expire_ms=1000)
        client.request('fork', source='Bob', bot='Carol', workspace=str(self.path), approve=['shell'],
                       approver='second', approve_expire_ms=2500)
        turn = client.request('submit', bot='Carol', request_id='t', prompt='shell:printf x')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        # The earlier gate is allowed while the turn is parked; the later one
        # still lapses on time and ends the turn.
        first = self.answer(client, turn, 'shell-1', bot='Carol', tag='manual')['result']
        self.assertEqual(first['pending'], ['second'])
        finished = client.finished(turn)['data']
        self.assertEqual((finished['status'], finished['error']), ('interrupted', 'approval_expired'))
        completed, _ = self.tool_output(client, 'shell-1', bot='Carol')
        self.assertEqual([(a['tag'], a['by'], a['allow']) for a in completed['approvals']],
                         [('manual', 'test', True), ('second', None, False)])

    def test_only_a_gate_that_lapsed_is_recorded_as_lapsed(self):
        client = self.gated(approve_expire_ms=300)
        client.request('fork', source='Bob', bot='Carol', workspace=str(self.path), approve=['shell'],
                       approver='second')
        turn = client.request('submit', bot='Carol', request_id='t', prompt='shell:printf x')['result']['turn']
        finished = client.finished(turn)['data']
        self.assertEqual((finished['status'], finished['error']), ('interrupted', 'approval_expired'))
        # The gate without an expiry was still open, not lapsed.
        completed, _ = self.tool_output(client, 'shell-1', bot='Carol')
        self.assertEqual([(a['tag'], a['by'], a['allow']) for a in completed['approvals']],
                         [('manual', None, False)])

    def test_a_later_calls_lapse_ends_a_turn_waiting_on_an_earlier_verdict(self):
        for hold in (2000, 50):
            with self.subTest(hold=hold):
                self.setUp()
                # The shell gate never lapses; the echo gate after it does.
                client = self.gated(settings={'approval_hold_ms': hold})
                client.request('fork', source='Bob', bot='Carol', workspace=str(self.path), approve=['echo'],
                               approver='second', approve_expire_ms=300)
                turn = client.request('submit', bot='Carol', request_id='t',
                                      prompt='shellecho:touch first|hi')['result']['turn']
                announced = time.monotonic()
                finished = client.finished(turn)
                self.assertEqual((finished['data']['status'], finished['data']['error']),
                                 ('interrupted', 'approval_expired'))
                self.assertLess(finished['_received_at'] - announced, 1.5)
                # Neither call ran, and the one waiting was not denied.
                for call_id in ('shell-1', 'echo-1'):
                    self.assertTrue(self.tool_output(client, call_id, bot='Carol')[0]['cancelled'])
                self.assertFalse((self.path / 'first').exists())
                self.assertEqual(len(self.events(client, turn, 'turn_waiting', bot='Carol')),
                                 1 if hold == 50 else 0)
                client.close()

    def test_a_gate_lapses_on_time_while_its_turn_waits_on_a_handle(self):
        client = self.gated(approve_expire_ms=300)
        client.request('create', bot='Alice', workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='wait')['result']
        turn = client.request('submit', bot='Bob', request_id='t',
                              prompt=f"waitshell:{alice['handle']}|printf after")['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        parked = time.monotonic()
        finished = client.finished(turn)
        self.assertEqual((finished['data']['status'], finished['data']['error']),
                         ('interrupted', 'approval_expired'))
        # Ended by the lapse, long before the turn it waited on.
        self.assertLess(time.monotonic() - parked, 2)
        self.assertEqual(client.request('resume', bot='Alice')['result']['status'], 'running')
        self.assertTrue(self.tool_output(client, 'shell-1')[0]['expired'])
        # The wait returned with its handle unresolved, not as a success.
        self.assertEqual(self.tool_output(client, 'wait-1')[1],
                         {'results': {alice['handle']: {'pending': True}}, 'pending': [alice['handle']]})

    def test_a_parked_verdict_survives_restart_and_interrupt_cancels_it(self):
        client = self.gated(settings={'approval_hold_ms': 0})
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

    def park_and_allow(self, client, turn, calls, restart_at, tools):
        """Allow each call once its turn parks for it, restarting the daemon
        while the call at `restart_at` waits; the bot keeps its settings.
        Returns the client in use."""
        for n, call_id in enumerate(calls):
            client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn
                           and m['data'].get('approval'), timeout=30)
            if n == restart_at:
                client.close(kill=True)
                client = self.client(tools)
            self.assertEqual(self.answer(client, turn, call_id)['result']['pending'], [])
        return client

    def test_a_turn_parked_on_every_call_compacts_inside_itself_across_a_restart(self):
        # Every shell call parks for its verdict while the turn outgrows its
        # budget: stubs and summaries come between parks, and one park spans
        # a restart. Each round runs once, with its allow.
        client = self.client('shell,read', settings={'approval_hold_ms': 0, 'context_bytes': 24576})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'],
                       approve=['shell'], approver='manual', compaction_instructions='Summarize.')
        rounds = 24
        turn = client.request('submit', bot='Bob', request_id='1', prompt=f'long:{rounds}')['result']['turn']
        calls = [f'long-{n}' for n in range(rounds)]
        client = self.park_and_allow(client, turn, calls, rounds // 2, 'shell,read')
        ended = client.finished(turn, timeout=30)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = node_item(client, 'Bob', ended['data']['checkpoint'])['result']
        self.assertIn(f'done after {rounds} rounds', json.dumps(answer))
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in requests))
        events = all_events(client, 'Bob')
        self.assertEqual([e['data']['call_id'] for e in events if e['event'] == 'tool_completed'],
                         calls + ['long-read'])
        started = [e['data'] for e in events if e['event'] == 'tool_started']
        self.assertTrue(all([a['allow'] for a in s['approvals']] == [True] for s in started[:rounds]))
        self.assertNotIn('approvals', started[-1])
        # Summaries both before and after the restart, every cut inside the
        # turn keeping its prompt, and none failed.
        cursors = [e['cursor'] for e in events if e['event'] == 'compacted']
        restart = next(e['cursor'] for e in events if e['event'] == 'turn_waiting'
                       and e['data']['call_id'] == calls[rounds // 2])
        self.assertTrue(any(c < restart for c in cursors) and any(c > restart for c in cursors), cursors)
        self.assertTrue(all(e['data']['pinned'] for e in events if e['event'] == 'compacted'))
        self.assertFalse([e for e in events if e['event'] == 'compaction_failed'])
        self.assertEqual(sum(is_summary(r) for r in requests), len(cursors))
        self.assertEqual(client.request('approvals')['result']['approvals'], [])

    def test_a_parked_calls_result_that_overflows_forces_a_summary_after_restart(self):
        # Four small rounds, then a call whose result takes the turn past its
        # budget. That call parks, the daemon restarts, and once allowed its
        # result forces a summary of the earlier rounds before the next call.
        client = self.client('shell', settings={'approval_hold_ms': 0, 'context_bytes': 24576, 'compact_at': 99})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       approve=['shell'], approver='manual', compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:4x250,1x600')['result']['turn']
        client = self.park_and_allow(client, turn, [f'long-{n}' for n in range(5)], 4, 'shell')
        ended = client.finished(turn, timeout=30)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = node_item(client, 'Bob', ended['data']['checkpoint'])['result']
        self.assertIn('done after 5 rounds', json.dumps(answer))
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in requests))
        self.assertEqual(['summary' if is_summary(r) else 'work' for r in requests],
                         ['work'] * 5 + ['summary', 'work'])
        compacted = [e['data'] for e in all_events(client, 'Bob') if e['event'] == 'compacted']
        self.assertEqual(len(compacted), 1)
        self.assertTrue(compacted[0]['pinned'])
        calls = [i['call_id'] for i in requests[-1]['input'] if i.get('type') == 'function_call']
        self.assertEqual(calls, ['long-4'])


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ServedApprovalTests(ModelFixture):
    """One session serves a gate tag: it holds the tag under a lease, gets
    the calls waiting on it, then each call announced for it."""

    def daemon(self, settings=None):
        daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'echo,shell,wait', settings=settings)
        self.addCleanup(daemon.close)
        created = daemon.request('create', bot='Bob', workspace=str(self.path), approve=['shell'],
                                 approver='auto')['result']
        self.assertEqual(created['gates'], [{'tag': 'auto', 'tools': ['shell']}])
        return daemon

    def session(self, daemon):
        session = Connection(daemon.socket_path)
        self.addCleanup(session.close)
        return session

    def pushed(self, session, turn, event='approval_requested'):
        return session.receive(lambda m: m.get('event') == event and m.get('turn') == turn)

    def answer(self, session, turn, call_id, request=1, decision='allow', **extra):
        return session.request('answer', bot='Bob', turn=turn, call_id=call_id, request=request,
                               decision=decision, by='test', **extra)

    def test_a_served_tag_gets_its_waiting_calls_then_each_new_one(self):
        daemon = self.daemon(settings={'approval_hold_ms': 60000})
        first = daemon.request('submit', bot='Bob', request_id='a', prompt='shell:printf one')['result']['turn']
        deadline = time.monotonic() + 5
        while not daemon.request('approvals', tag='auto')['result']['approvals']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        approver = self.session(daemon)
        served = approver.request('serve_approvals', tag='auto', lease_ms=5000)['result']
        lease = served['lease']
        self.assertEqual((served['tag'], served['lease_ms'], served['next_after']), ('auto', 5000, None))
        # Later pages end where serving began.
        self.assertEqual(daemon.request('approvals', tag='auto', through=served['through'])['result']['approvals'],
                         served['approvals'])
        # The waiting call comes with the listing, whole, with the counts.
        [waiting] = served['approvals']
        self.assertEqual((waiting['turn'], waiting['call_id'], waiting['request']), (first, 'shell-1', 1))
        self.assertEqual(waiting['arguments']['command'], 'printf one')
        self.assertEqual(waiting['denials'], {'auto': {'in_row': 0, 'in_turn': 0}})
        self.assertEqual(daemon.request('stats')['result']['approvers'], ['auto'])
        # Another session cannot serve a held tag, and an answer under a
        # lease names its tag and holds only on the session that has it.
        other = self.session(daemon)
        self.assertEqual(other.request('serve_approvals', tag='auto', lease_ms=5000)['error'], 'approvals_served')
        self.assertEqual(self.answer(approver, first, 'shell-1', lease=lease)['error'], 'invalid_lease')
        self.assertEqual(self.answer(other, first, 'shell-1', tag='auto', lease=lease)['error'], 'approvals_lost')
        self.assertEqual(approver.request('renew_approvals', tag='auto', lease=lease + 1)['error'], 'approvals_lost')
        self.assertEqual(approver.request('renew_approvals', tag='auto', lease=lease)['result'],
                         {'tag': 'auto', 'lease': lease, 'lease_ms': 5000})
        self.assertEqual(self.answer(approver, first, 'shell-1', tag='auto', lease=lease)['result']['pending'], [])
        self.assertEqual(daemon.finished(first)['data']['status'], 'completed')
        # A call announced after serving began is pushed to the holder only.
        second = daemon.request('submit', bot='Bob', request_id='b', prompt='shell:printf two')['result']['turn']
        pushed = self.pushed(approver, second)
        self.assertEqual((pushed['tag'], pushed['lease'], pushed['durable']), ('auto', lease, False))
        self.assertEqual((pushed['data']['part'], pushed['data']['parts']), (1, 1))
        [call] = pushed['data']['calls']
        self.assertEqual((call['call_id'], call['request'], call['arguments']['command']),
                         ('shell-1', 1, 'printf two'))
        self.assertEqual(self.answer(approver, second, 'shell-1', tag='auto', lease=lease,
                                     decision='deny', reason='not now')['result']['decision'], 'deny')
        self.assertEqual(daemon.finished(second)['data']['status'], 'completed')
        # The daemon counts the denial on the bot, and the next call carries it.
        self.assertEqual(daemon.request('resume', bot='Bob')['result']['denials'],
                         {'auto': {'in_row': 1, 'in_turn': 1, 'turn': second}})
        third = daemon.request('submit', bot='Bob', request_id='c', prompt='shell:printf three')['result']['turn']
        [call] = self.pushed(approver, third)['data']['calls']
        self.assertEqual(call['denials'], {'auto': {'in_row': 1, 'in_turn': 0}})
        # Closing the session frees the tag at once: the next server takes
        # over under a new lease, and gets the call still waiting.
        approver.close()
        deadline = time.monotonic() + 5
        while (taken := other.request('serve_approvals', tag='auto', lease_ms=5000)).get('error') == 'approvals_served':
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        taken = taken['result']
        self.assertNotEqual(taken['lease'], lease)
        self.assertEqual([c['turn'] for c in taken['approvals']], [third])
        self.answer(other, third, 'shell-1', tag='auto', lease=taken['lease'])
        self.assertEqual(daemon.finished(third)['data']['status'], 'completed')
        self.assertEqual(daemon.request('resume', bot='Bob')['result']['denials'],
                         {'auto': {'in_row': 0, 'in_turn': 1, 'turn': second}})

    def test_a_quiet_holder_loses_its_tag_to_the_next_server(self):
        daemon = self.daemon(settings={'approval_hold_ms': 60000})
        quiet, next_ = self.session(daemon), self.session(daemon)
        old = quiet.request('serve_approvals', tag='auto', lease_ms=100)['result']['lease']
        self.assertEqual(next_.request('serve_approvals', tag='auto', lease_ms=5000)['error'], 'approvals_served')
        time.sleep(.2)
        # Past its lease the holder still has the tag until another takes it.
        new = next_.request('serve_approvals', tag='auto', lease_ms=5000)['result']['lease']
        lost = quiet.receive(lambda m: m.get('event') == 'approvals_lost')
        self.assertEqual((lost['tag'], lost['lease']), ('auto', old))
        turn = daemon.request('submit', bot='Bob', request_id='a', prompt='shell:printf hi')['result']['turn']
        self.pushed(next_, turn)
        # A woken holder's answer under its old lease changes nothing.
        self.assertEqual(self.answer(quiet, turn, 'shell-1', tag='auto', lease=old)['error'], 'approvals_lost')
        self.assertEqual(quiet.request('renew_approvals', tag='auto', lease=old)['error'], 'approvals_lost')
        self.assertEqual(daemon.request('approvals', tag='auto')['result']['approvals'][0]['call_id'], 'shell-1')
        self.answer(next_, turn, 'shell-1', tag='auto', lease=new)
        self.assertEqual(daemon.finished(turn)['data']['status'], 'completed')
        # A lease that runs out with no one to take over is ended when its
        # holder next renews.
        short = quiet.request('serve_approvals', tag='short', lease_ms=100)['result']['lease']
        time.sleep(.2)
        self.assertEqual(quiet.request('renew_approvals', tag='short', lease=short)['error'], 'approvals_lost')
        self.assertEqual(quiet.receive(lambda m: m.get('event') == 'approvals_lost')['lease'], short)
        self.assertEqual(daemon.request('stats')['result']['approvers'], ['auto'])
        # An answer without a lease overrides whoever serves the tag.
        override = daemon.request('submit', bot='Bob', request_id='b', prompt='shell:printf over')['result']['turn']
        deadline = time.monotonic() + 5
        while not daemon.request('approvals', tag='auto')['result']['approvals']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        self.assertEqual(self.answer(daemon.control, override, 'shell-1')['result']['pending'], [])
        self.assertEqual(daemon.finished(override)['data']['status'], 'completed')

    def test_serving_is_refused_for_a_bad_tag_or_lease(self):
        daemon = self.daemon()
        session = self.session(daemon)
        for tag, lease_ms, error in (('', 5000, 'invalid_approver'), ('auto', 99, 'invalid_lease'),
                                     ('auto', 600_001, 'invalid_lease')):
            self.assertEqual(session.request('serve_approvals', tag=tag, lease_ms=lease_ms)['error'], error)
        self.assertEqual(daemon.request('stats')['result']['approvers'], [])
        # Duplicate registration and a bad page both preserve the lease.
        lease = session.request('serve_approvals', tag='auto', lease_ms=5000)['result']['lease']
        self.assertEqual(session.request('serve_approvals', tag='auto', lease_ms=5000)['error'],
                         'approvals_served')
        for limit in (0, 257):
            self.assertEqual(session.request('serve_approvals', tag='auto', lease_ms=5000, limit=limit)['error'],
                             'invalid_approval_page')
        self.assertEqual(session.request('renew_approvals', tag='auto', lease=lease)['result']['lease'], lease)
        self.assertEqual(daemon.request('stats')['result']['approvers'], ['auto'])


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class PromptReadTests(ModelFixture):
    """What a program judging a call reads: who wrote each prompt, and what
    the turn already ran."""

    def test_a_turn_reads_its_prompts_authors_and_calls(self):
        client = self.client('echo,shell')
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='a', prompt='shell:exit 3')['result']['turn']
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        read = client.request('prompts', bot='Bob', turn=first)['result']
        self.assertEqual(read['prompts'], [{'turn': first, 'text': 'shell:exit 3'}])
        [call] = read['calls']
        self.assertEqual((call['name'], call['done'], call['failed']), ('shell', True, True))
        self.assertEqual(json.loads(call['arguments'])['command'], 'exit 3')
        # A prompt a bot's model wrote names that bot's turn; the turn must
        # be one of that bot's.
        self.assertEqual(client.request('submit', bot='Bob', request_id='b', prompt='hi',
                                        **{'from': {'bot': 'Bob', 'turn': first + 99}})['error'], 'invalid_from')
        # What sent a prompt when no bot's turn did is a name the client picks.
        self.assertEqual(client.request('submit', bot='Bob', request_id='o', prompt='hi',
                                        origin='not a name')['error'], 'invalid_origin')
        second = client.request('submit', bot='Bob', request_id='c', prompt='done',
                                **{'from': {'bot': 'Bob', 'turn': first}})['result']['turn']
        self.assertEqual(client.finished(second)['data']['status'], 'completed')
        read = client.request('prompts', bot='Bob', turn=second, bytes=1024)['result']
        self.assertEqual(read['prompts'], [{'turn': second, 'text': 'done',
                                            'from': {'bot': 'Bob', 'turn': first}}])
        self.assertEqual(read['earlier'], [{'turn': first, 'text': 'shell:exit 3'}])
        self.assertEqual((read['status'], read['workspace'], read['more']), ('finished', str(self.path), False))
        self.assertEqual(client.request('prompts', bot='Bob', turn=second, bytes=0)['error'], 'invalid_limit')

    def test_the_approvers_key_never_reaches_a_tool(self):
        env = {**clean_env(), 'TYPESAFE_API_KEY': 'synthetic-judge-key'}
        client = Client(self.binary, self.path / 'state.sqlite', self.url, 'shell', env=env)
        self.addCleanup(client.close)
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='a',
                              prompt='shell:printf "${TYPESAFE_API_KEY-unset}/$AGENT_TURN" > seen')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual((self.path / 'seen').read_text(), f'unset/{turn}')


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
        self.assertIn(f"agent answer --store {shlex.quote(str(self.store))} --bot Bob "
                      f"--turn {submitted['turn']} --call=shell-1 --request 1", pretty)
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
        # One whole command per verdict; the allow one runs as printed.
        [line] = [line.strip() for line in pretty.splitlines()
                  if line.strip().startswith('agent answer ') and line.endswith(' allow')]
        # The command names this store, so it reaches this daemon as printed.
        self.assertIn(f'agent answer --store {shlex.quote(str(self.store))} --bot', line)
        command = line.replace('agent answer', f'{shlex.quote(str(self.binary))} answer', 1)
        pasted = subprocess.run(['bash', '-c', command], cwd=self.path,
                                env=clean_env(), capture_output=True, text=True, timeout=30)
        self.assertEqual(pasted.returncode, 0, pasted.stderr)
        self.assertEqual(json.loads(pasted.stdout)['pending'], [])
        self.assertFalse((self.path / 'pwned').exists())
        result = self.agent('wait', '--store', str(self.store), submitted['handle'])
        self.assertEqual(json.loads(result.stdout)['results'][submitted['handle']]['status'], 'completed')

    def test_run_pretty_shows_what_a_call_would_do_before_its_command(self):
        # The call's output would conceal what follows it; it is shown escaped.
        run = subprocess.Popen([str(self.binary), 'run', *self.common, '--new', '--bot', 'Bob', '--pretty',
                                "shell:printf 'shown\\033[8m'"], env={**clean_env(), 'AGENT_APPROVAL': 'manual'},
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, cwd=self.path)
        self.addCleanup(run.kill)
        lines = queue.Queue()
        reader = threading.Thread(target=lambda: [lines.put(line) for line in run.stdout], daemon=True)
        reader.start()
        shown = ''
        while 'manual deny' not in shown:
            shown += lines.get(timeout=10)
        self.assertIn("⏸ shell printf 'shown\\033[8m'", shown)
        self.assertIn('--call=shell-1 --request 1 --tag manual allow\n', shown)
        self.assertIn('--call=shell-1 --request 1 --tag manual deny\n', shown)
        turn = re.search(r'--turn (\d+)', shown)[1]
        self.agent('answer', '--store', str(self.store), '--bot', 'Bob', '--turn', turn, '--call', 'shell-1',
                   '--request', '1', 'allow')
        self.assertEqual(run.wait(timeout=30), 0, run.stderr.read())
        reader.join(timeout=10)
        while not lines.empty():
            shown += lines.get()
        self.assertIn('  shown\\u{1b}[8m\n', shown)
        self.assertNotIn('\x1b', shown)

    def test_modes_are_validated_before_anything_is_created(self):
        # A judge that cannot start leaves no bot waiting on it.
        auto = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--approval', 'auto', 'hi', check=False,
                          env={'AGENT_APPROVER_JUDGE': 'nowhere/judge'})
        self.assertEqual(auto.returncode, 1)
        self.assertIn('approver_start_failed', auto.stderr)
        full = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--approve', 'shell', 'hi', check=False)
        self.assertEqual(full.returncode, 2)
        self.assertIn('--approval manual', full.stderr)
        unknown = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--approval', 'some', 'hi', check=False)
        self.assertEqual(unknown.returncode, 2)
        # Asked to gate nothing is refused, not run ungated.
        for listed in ('', ',,'):
            empty = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--approval', 'manual',
                               '--approve', listed, 'hi', check=False)
            self.assertEqual(empty.returncode, 2)
            self.assertIn('--approve names no tools', empty.stderr)
        existing = self.agent('run', *self.common, '--bot', 'Bob', '--approval', 'manual', 'hi', check=False)
        self.assertEqual(existing.returncode, 2)
        self.assertIn('creation options', existing.stderr)
        listed = self.agent('ls', '--store', str(self.store), check=False)
        bots = json.loads(listed.stdout) if listed.returncode == 0 else []
        self.assertFalse(any(b['name'] == 'Bob' for b in bots))



class Judge(http.server.BaseHTTPRequestHandler):
    """A stand-in for the judge model: every question gets `answer(body, id)`,
    unless `statuses` holds a failure to send first."""
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.requests.put({'path': self.path, 'authorization': self.headers.get('Authorization'),
                                  'body': body})
        status = self.server.statuses.pop(0) if self.server.statuses else 200
        headers = {'Content-Type': 'application/json'}
        if status == 200:
            answers = {id: {'type': 'noul', 'noul': self.server.answer(body, id)} for id in body['questions']}
            payload = {'model': body['model'], 'answers': answers,
                       'usage': {'input_tokens': 1000, 'output_tokens': 0}}
        else:
            payload = {'error': 'synthetic'}
            if status == 429:
                headers['Retry-After'] = '0.05'
        data = json.dumps(payload).encode()
        self.send_response(status)
        for name, value in {**headers, 'Content-Length': str(len(data))}.items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(data)


def risky(body, id):
    """Deletion is likely for a planned call whose arguments say DANGER, and
    nobody asked for it; everything else is low."""
    call, _, question = id.partition('_')
    [planned] = [c for c in body['state']['planned_calls'] if c['id'] == call]
    return .9 if question == 'delete' and 'DANGER' in json.dumps(planned['arguments']) else .05


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AutoApproverTests(ModelFixture):
    """`agent approver` serves `auto`: a judge model decides every call."""

    def setUp(self):
        super().setUp()
        self.judge = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Judge)
        self.judge.requests, self.judge.statuses, self.judge.answer = queue.Queue(), [], risky
        self.judge.daemon_threads = True
        threading.Thread(target=self.judge.serve_forever, daemon=True).start()
        self.addCleanup(self.judge.server_close)
        self.addCleanup(self.judge.shutdown)
        self.judge_url = f'http://127.0.0.1:{self.judge.server_port}'

    def daemon(self, tools='echo,shell,wait'):
        daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, tools)
        self.addCleanup(daemon.close)
        daemon.request('create', bot='Bob', workspace=str(self.path), approve=['shell'], approver='auto',
                       approve_expire_ms=15000)
        return daemon

    def approver(self, daemon, env=None, judge=None):
        """Jev at the stand-in unless `judge` names a model the daemon serves."""
        judged_by = ['--judge', judge] if judge else ['--judge-url', self.judge_url]
        key = {} if judge else {'TYPESAFE_API_KEY': 'synthetic-judge-key'}
        process = subprocess.Popen(
            [str(self.binary), 'approver', '--store', str(self.path / 'state.sqlite'),
             '--socket', str(daemon.socket_path), *judged_by],
            env={**clean_env(), **key, **(env or {})},
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.addCleanup(lambda: (process.kill(), process.wait(), process.stdout.close()))
        lines = queue.Queue()
        threading.Thread(target=lambda: [lines.put(json.loads(line)) for line in process.stdout],
                         daemon=True).start()
        self.assertEqual(lines.get(timeout=5)['event'], 'serving')
        self.assertEqual(daemon.request('stats')['result']['approvers'], ['auto'])
        return process, lines

    def poll(self, daemon, turn, call_id='shell-1'):
        events = daemon.request('events', bot='Bob', after=0, limit=256)['result']['events']
        return next(e['data'] for e in events if e['turn'] == turn and e['event'] == 'tool_completed'
                    and e['data']['call_id'] == call_id)

    def denial(self, daemon, turn, call_id='shell-1'):
        completed = self.poll(daemon, turn, call_id)
        self.assertTrue(completed.get('denied'), completed)
        output = node_item(daemon, 'Bob', completed['node'])['result']['output']
        return json.loads(output)['detail']

    def test_every_call_is_judged_once_a_round_and_risky_ones_are_denied(self):
        daemon = self.daemon()
        _, lines = self.approver(daemon)
        safe = daemon.request('submit', bot='Bob', request_id='a', prompt='shell:printf safe > out')['result']['turn']
        self.assertEqual(daemon.finished(safe)['data']['status'], 'completed')
        self.assertEqual((self.path / 'out').read_text(), 'safe')
        asked = self.judge.requests.get(timeout=5)
        self.assertEqual((asked['path'], asked['authorization']), ('/v1/systemone', 'Bearer synthetic-judge-key'))
        body = asked['body']
        self.assertEqual(body['model'], 'jev-latest')
        self.assertEqual(len(body['questions']), 11)
        self.assertEqual(body['state']['request'], [{'by': 'person', 'text': 'shell:printf safe > out'}])
        self.assertEqual(body['state']['planned_calls'][0]['arguments']['command'], 'printf safe > out')
        self.assertEqual(body['state']['workspace'], str(self.path))
        judged = lines.get(timeout=5)
        self.assertEqual((judged['event'], judged['calls'][0]['decision'], judged['input_tokens']),
                         ('judged', 'allow', 1000))
        # A risky call nobody asked for is denied with the reason; the
        # turn's earlier call shows as already allowed, and it succeeded.
        risky_turn = daemon.request('submit', bot='Bob', request_id='b',
                                    prompt='shell:printf DANGER')['result']['turn']
        self.assertEqual(daemon.finished(risky_turn)['data']['status'], 'completed')
        reason = self.denial(daemon, risky_turn)
        self.assertTrue(reason.startswith('judged risky: it deletes or overwrites'), reason)
        body = self.judge.requests.get(timeout=5)['body']
        self.assertEqual(body['state']['earlier_prompts'], [{'by': 'person', 'text': 'shell:printf safe > out'}])
        self.assertEqual(self.judge.requests.qsize(), 0)

    def test_a_model_the_daemon_serves_judges_without_a_jev_key(self):
        asked = queue.Queue()
        parsed = [True]

        def reply(request, user):
            if request['model'] != 'judge-model':
                return None
            round = json.loads(user)
            asked.put((request, round))
            answers = {id: risky(round, id) for id in round['questions']}
            return 'Answers: ' + json.dumps(answers) if parsed[0] else 'They look fine to me.'
        self.model.reply_for = reply
        self.model.models = ('synthetic-model', 'judge-model')
        daemon = self.daemon()
        # A bot that only has the judge's name is refused, not replaced.
        daemon.request('create', bot='approver.auto', workspace='/', model='openai/old-judge',
                       instructions='Help with the repo.', tools=[])
        refused = subprocess.run(
            [str(self.binary), 'approver', '--store', str(self.path / 'state.sqlite'),
             '--socket', str(daemon.socket_path), '--judge', 'openai/judge-model'],
            env=clean_env(), capture_output=True, text=True, timeout=30)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('approver_name_taken', refused.stderr)
        self.assertEqual(daemon.request('resume', bot='approver.auto')['result']['instructions'],
                         'Help with the repo.')
        self.assertIn('result', daemon.request('delete', bot='approver.auto'))
        # An approver that stopped leaves its judge and a fork from mid-round:
        # the next one makes the judge afresh, removes the fork, and keeps a
        # bot that only shares its prefix.
        old, _ = self.approver(daemon, judge='openai/old-judge')
        old.kill()
        old.wait()
        deadline = time.monotonic() + 5
        while daemon.request('stats')['result']['approvers']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.05)
        daemon.request('fork', source='approver.auto', bot='approver.auto.1.0', created_by='approver.auto')
        daemon.request('create', bot='approver.auto.notes', workspace='/', model='openai/synthetic-model',
                       tools=['shell'])
        _, lines = self.approver(daemon, judge='openai/judge-model')
        bots = {b['name']: b for b in daemon.request('bots')['result']['bots']}
        self.assertEqual(sorted(bots), ['Bob', 'approver.auto', 'approver.auto.notes'])
        self.assertIn('result', daemon.request('delete', bot='approver.auto.notes'))
        self.assertEqual((bots['approver.auto']['model'], bots['approver.auto']['tools'],
                          bots['approver.auto']['gates']), ('judge-model', [], []))
        safe = daemon.request('submit', bot='Bob', request_id='a', prompt='shell:printf safe > out')['result']['turn']
        self.assertEqual(daemon.finished(safe)['data']['status'], 'completed')
        self.assertEqual((self.path / 'out').read_text(), 'safe')
        request, round = asked.get(timeout=5)
        self.assertEqual(request['model'], 'judge-model')
        self.assertFalse(request.get('tools'))
        self.assertIn('You judge tool calls', json.dumps(request))
        self.assertEqual(round['state']['request'], [{'by': 'person', 'text': 'shell:printf safe > out'}])
        self.assertEqual(len(round['questions']), 11)
        self.assertIn(' Yes: ', round['questions']['c1_delete_ok'])
        judged = lines.get(timeout=5)
        self.assertEqual((judged['event'], judged['calls'][0]['decision'], judged['input_tokens']),
                         ('judged', 'allow', 100))
        risky_turn = daemon.request('submit', bot='Bob', request_id='b', prompt='shell:printf DANGER')['result']['turn']
        self.assertEqual(daemon.finished(risky_turn)['data']['status'], 'completed')
        reason = self.denial(daemon, risky_turn)
        self.assertTrue(reason.startswith('judged risky: it deletes or overwrites'), reason)
        # A reply without every answer is a failed check, not an allow.
        parsed[0] = False
        vague = daemon.request('submit', bot='Bob', request_id='c', prompt='shell:printf vague')['result']['turn']
        self.assertEqual(daemon.finished(vague)['data']['status'], 'completed')
        self.assertEqual(self.denial(daemon, vague), 'not reviewed: the check failed')
        events = [lines.get(timeout=5) for _ in range(3)]
        self.assertIn({'event': 'judge_failed', 'bot': 'Bob', 'turn': vague,
                       'detail': "the judge's answer did not parse"}, events)
        self.assertEqual(self.judge.requests.qsize(), 0)
        # Each round's fork goes once it is answered; the base stays.
        deadline = time.monotonic() + 5
        while sorted(b['name'] for b in daemon.request('bots')['result']['bots']) != ['Bob', 'approver.auto']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.05)

    def test_secrets_are_redacted_before_the_judge_sees_them(self):
        daemon = self.daemon()
        self.approver(daemon)
        prompt = 'shell:printf "Authorization: Bearer synthetic-secret-value" > sent'
        turn = daemon.request('submit', bot='Bob', request_id='a', prompt=prompt)['result']['turn']
        self.assertEqual(daemon.finished(turn)['data']['status'], 'completed')
        sent = json.dumps(self.judge.requests.get(timeout=5)['body'])
        self.assertNotIn('synthetic-secret-value', sent)
        self.assertIn('[secret: bearer token]', sent)

    def test_a_failed_check_denies_as_not_reviewed_and_a_rate_limit_waits(self):
        daemon = self.daemon()
        self.approver(daemon)
        self.judge.statuses = [500]
        failed = daemon.request('submit', bot='Bob', request_id='a', prompt='shell:printf one')['result']['turn']
        self.assertEqual(daemon.finished(failed)['data']['status'], 'completed')
        self.assertEqual(self.denial(daemon, failed), 'not reviewed: the check failed')
        # A 429 is retried after its Retry-After, within the round's deadline.
        self.judge.statuses = [429]
        paced = daemon.request('submit', bot='Bob', request_id='b', prompt='shell:printf two > two')['result']['turn']
        self.assertEqual(daemon.finished(paced)['data']['status'], 'completed')
        self.assertEqual((self.path / 'two').read_text(), 'two')
        self.assertEqual(self.judge.requests.qsize(), 3)

    def test_the_breaker_stops_a_turn_that_keeps_getting_denied(self):
        self.model.call_script = [('shell', {'command': f'printf DANGER-{n}'}) for n in range(5)]
        daemon = self.daemon()
        self.approver(daemon)
        turn = daemon.request('submit', bot='Bob', request_id='a', prompt='script')['result']['turn']
        finished = daemon.finished(turn)
        self.assertEqual(finished['data']['status'], 'interrupted', finished)
        for n in range(3):
            self.assertTrue(self.denial(daemon, turn, f'script-{n}').startswith('judged risky'))
        # The fourth is refused without asking, with a reason to stop; the
        # fifth round ends the turn.
        self.assertTrue(self.denial(daemon, turn, 'script-3').startswith('stopped: 3 calls in a row'))
        self.assertEqual(self.judge.requests.qsize(), 3)

    def test_a_delegated_turn_is_judged_on_the_persons_words(self):
        daemon = self.daemon()
        daemon.request('create', bot='Carol', workspace=str(self.path), approve=['shell'], approver='auto')
        self.approver(daemon)
        # A person asks Bob; Bob's model asks Carol, who plans the call.
        person = daemon.request('submit', bot='Bob', request_id='a', prompt='delete the build directory')
        person = person['result']['turn']
        daemon.finished(person)
        delegated = daemon.request('submit', bot='Carol', request_id='b', prompt='shell:printf DANGER',
                                   **{'from': {'bot': 'Bob', 'turn': person}})['result']['turn']
        daemon.finished(delegated)
        request = self.judge.requests.get(timeout=5)['body']['state']['request']
        self.assertEqual(request, [{'by': 'person', 'text': 'delete the build directory'},
                                   {'by': 'model', 'text': 'shell:printf DANGER'}])

    def test_a_call_is_judged_with_the_files_it_runs_that_the_turn_wrote(self):
        def shell_danger(body, id):
            call, _, question = id.partition('_')
            [planned] = [c for c in body['state']['planned_calls'] if c['id'] == call]
            return .9 if question == 'delete' and planned['tool'] == 'shell' and 'DANGER' in json.dumps(planned) else .05
        self.judge.answer = shell_danger
        self.model.call_script = [('write', {'path': 'run.sh', 'content': 'printf DANGER'}),
                                  ('shell', {'command': 'sh run.sh'}),
                                  ('write', {'path': 'big.sh', 'content': 'x' * (49 * 1024)}),
                                  ('shell', {'command': 'sh ./big.sh'})]
        daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'shell,write')
        self.addCleanup(daemon.close)
        daemon.request('create', bot='Bob', workspace=str(self.path), approve=['shell', 'write'], approver='auto')
        self.approver(daemon)
        turn = daemon.request('submit', bot='Bob', request_id='a', prompt='script')['result']['turn']
        self.assertEqual(daemon.finished(turn)['data']['status'], 'completed')
        self.assertTrue(self.denial(daemon, turn, 'script-1').startswith('judged risky'))
        self.assertEqual(self.denial(daemon, turn, 'script-3'), 'not reviewed: it runs a file too large to show')
        asked = [self.judge.requests.get(timeout=5)['body'] for _ in range(3)]
        [shell] = asked[1]['state']['planned_calls']
        self.assertEqual(shell['files_it_names'], [{'path': 'run.sh', 'content': 'printf DANGER'}])
        self.assertEqual(asked[1]['state']['already_allowed'],
                         [{'tool': 'write', 'arguments': {'path': 'run.sh', 'bytes': 13}, 'status': 'succeeded'}])

    def test_an_ungated_write_too_long_to_preview_still_shows_its_file(self):
        def shell_danger(body, id):
            call, _, question = id.partition('_')
            [planned] = [c for c in body['state']['planned_calls'] if c['id'] == call]
            return .9 if question == 'delete' and 'DANGER' in json.dumps(planned) else .05
        self.judge.answer = shell_danger
        script = '# padding line\n' * 250 + 'printf DANGER\n'
        self.model.call_script = [('write', {'path': 'run.sh', 'content': script}),
                                  ('shell', {'command': 'sh run.sh'})]
        daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'shell,write')
        self.addCleanup(daemon.close)
        daemon.request('create', bot='Bob', workspace=str(self.path), approve=['shell'], approver='auto')
        self.approver(daemon)
        turn = daemon.request('submit', bot='Bob', request_id='a', prompt='script')['result']['turn']
        self.assertEqual(daemon.finished(turn)['data']['status'], 'completed')
        self.assertTrue(self.denial(daemon, turn, 'script-1').startswith('judged risky'))
        state = self.judge.requests.get(timeout=5)['body']['state']
        [shell] = state['planned_calls']
        self.assertEqual(shell['files_it_names'], [{'path': 'run.sh', 'content': script}])
        self.assertEqual(state['already_allowed'],
                         [{'tool': 'write', 'arguments': {'path': 'run.sh', 'bytes': None}, 'status': 'succeeded'}])

    def test_the_cli_starts_the_approver_for_an_auto_bot(self):
        store = self.path / 'state.sqlite'
        common = ['--store', str(store), '--provider', f'openai=responses,{self.url}',
                  '--model', 'openai/synthetic-model', '--tools', 'echo,shell']
        env = {**clean_env(), 'AGENT_APPROVAL': 'auto'}
        def agent(*args):
            return subprocess.run([str(self.binary), *args], env=env,
                                  capture_output=True, text=True, timeout=30, cwd=self.path)
        self.addCleanup(lambda: agent('shutdown', '--store', str(store)))
        # Without a Jev key the bot's own model judges, through the daemon.
        self.model.reply_for = lambda request, user: (
            json.dumps({id: .05 for id in json.loads(user)['questions']})
            if user.startswith('{"questions"') else None)
        done = agent('run', *common, '--new', '--bot', 'Bob', 'shell:printf hi > hi')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual((self.path / 'hi').read_text(), 'hi')
        bots = {b['name']: b for b in json.loads(agent('ls', '--store', str(store)).stdout)}
        self.assertEqual(bots['Bob']['gates'], [{'tag': 'auto', 'tools': ['shell'], 'expire_ms': 45000}])
        self.assertEqual((bots['approver.auto']['model'], bots['approver.auto']['tools']), ('synthetic-model', []))
        stats = json.loads(agent('stats', '--store', str(store)).stdout)
        self.assertEqual(stats['approvers'], ['auto'])
        self.assertTrue((self.path / 'state.sqlite.approver.log').exists())

if __name__ == '__main__':
    unittest.main()
