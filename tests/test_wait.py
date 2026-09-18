"""Deferred tool results: background commands, peer turn handles, and parked turns."""
import json
import os
import psutil
import queue
import sqlite3
import subprocess
import threading
import time
import unittest

from bench.runtime_client import Client, serve_args
from bench.socket_client import SocketClient
from bench.targets import clean_env
from tests.test_runtime import ModelFixture


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class WaitTests(ModelFixture):
    def test_zero_timeout_tool_returns_ready_and_pending_handles(self):
        client = self.client('echo,wait')
        for bot in ('Alice', 'Bob', 'Carol'):
            client.request('create', bot=bot, workspace=str(self.path))
        slow = client.request('submit', bot='Alice', request_id='slow', prompt='wait')['result']
        quick = client.request('submit', bot='Carol', request_id='quick', prompt='hi')['result']
        client.finished(quick['turn'])
        turn = client.request('submit', bot='Bob', request_id='poll',
                              prompt=f"waitt:0:{slow['handle']},{quick['handle']}")['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        outcome = self.tool_output(client, 'Bob', 'wait-1')
        self.assertEqual(outcome['pending'], [slow['handle']])
        self.assertEqual(outcome['results'][quick['handle']]['text'], 'reply:hi')
        self.assertEqual(client.request('stats')['result']['handles'], {'waiters': 0, 'retained': 0})
        client.request('interrupt', bot='Alice', turn=slow['turn'])

    def test_retention_applies_when_parked_turns_are_interrupted(self):
        client = self.client('wait', extra=('--retain-turns', '1'))
        for bot in ('Alice', 'Bob'):
            client.request('create', bot=bot, workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='wait')['result']['turn']
        for n in range(3):
            turn = client.request('submit', bot='Bob', request_id=str(n),
                                  prompt=f'wait:turn:Alice/{alice}')['result']['turn']
            client.receive(lambda e: e.get('event') == 'turn_waiting' and e.get('turn') == turn)
            time.sleep(.03)  # Let the parked task retire; exercise the direct interrupt path.
            interrupted = client.request('interrupt', bot='Bob', turn=turn)['result']
            self.assertTrue(interrupted.get('parked'), interrupted)
            self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        page = client.request('events', bot='Bob', after=0, limit=256)['result']
        self.assertEqual([e['turn'] for e in page['events'] if e['event'] == 'turn_finished'], [turn])
        self.assertIn('pruned_before', page)
        self.assertEqual(client.request('result', bot='Bob', turn=turn)['result']['error'], 'cancelled')
        client.request('interrupt', bot='Alice', turn=alice)

    def test_slow_stdio_wait_reader_gets_disconnect_without_stdin_eof(self):
        client = self.client('echo,shell,wait')
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='output',
                              prompt="bg:printf '%065536d' 0; printf '%065536d' 0 >&2")['result']['turn']
        client.finished(turn)
        handle = self.tool_output(client, 'Bob', 'bg-1')['handle']
        client.request('wait', handles=[handle])
        client.close()
        process = subprocess.Popen(
            [str(self.binary), *serve_args(self.path / 'state.sqlite', self.url, 'echo,shell,wait')],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, env=clean_env())
        try:
            self.assertEqual(json.loads(process.stdout.readline())['event'], 'ready')
            # Each response is 128 KiB: stop reading until the 2 MiB queue fills.
            # Keep stdin open, so only the failed delivery can close the session.
            for n in range(64):
                process.stdin.write(json.dumps({'id': n, 'op': 'wait', 'handles': [handle]}) + '\n')
            process.stdin.flush()
            self.assertEqual(process.wait(timeout=2), 1)
            process.stdout.read()  # EOF, even with the input side still open.
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=2)
            process.stdin.close()
            process.stdout.close()

    def test_waiters_share_output_before_and_after_process_completion(self):
        client = self.client('echo,shell,wait')
        client.request('create', bot='Bob', workspace=str(self.path))
        def background(request, command):
            turn = client.request('submit', bot='Bob', request_id=request, prompt='bg:' + command)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            return self.tool_output(client, 'Bob', 'bg-1')['handle']
        output = background('output', "while [ ! -f release ]; do sleep .01; done; printf '%065536d' 0; printf '%065536d' 0 >&2")
        pending = background('pending', 'while [ ! -f finish ]; do sleep .01; done')
        process = psutil.Process(client.process.pid)
        baseline = process.memory_info().rss
        for phase in ('before', 'after'):
            for n in range(128):
                client.process.stdin.write(json.dumps({'id': f'{phase}-{n}', 'op': 'wait',
                                                       'handles': [output, pending]}) + '\n')
            client.process.stdin.flush()
            client.request('bots')  # Dispatch barrier: every wait is registered.
            if phase == 'before':
                (self.path / 'release').touch()
                result = client.request('wait', handles=[output])['result']['results'][output]
                self.assertEqual((len(result['stdout']), len(result['stderr'])), (65536, 65536))
            # Copying the 128 KiB output into each waiter costs 16 MiB per phase.
            # Allow ample allocator noise while rejecting that linear growth.
            self.assertLess(process.memory_info().rss - baseline, 8 * 1024 * 1024, phase)
        self.assertEqual(client.request('wait', handles=[pending], timeout_ms=0)['result']['pending'], [pending])

    def test_old_turn_outcomes_use_indexed_event_lookups(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='old', prompt='hello')['result']
        client.finished(turn['turn'])
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            for kind in ('turn_finished', 'message'):
                plan = db.execute("EXPLAIN QUERY PLAN SELECT data FROM events WHERE turn=? AND kind=? ORDER BY id DESC LIMIT 1",
                                  (turn['turn'], kind)).fetchall()
                self.assertTrue(any('SEARCH events' in row[3] for row in plan), plan)
                self.assertFalse(any('SCAN events' in row[3] for row in plan), plan)
        outcome = client.request('wait', handles=[turn['handle']])['result']['results'][turn['handle']]
        self.assertEqual((outcome['status'], outcome['text']), ('completed', 'reply:hello'))

    def tool_output(self, client, bot, call_id):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        node = [e for e in events if e['event'] == 'tool_completed' and e['data']['call_id'] == call_id][-1]['data']['node']
        output = client.request('item', bot=bot, node=node)['result']['output']
        try:
            return json.loads(output)
        except ValueError:
            return output

    def kinds(self, client, bot, turn):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        return [e['event'] for e in events if e.get('turn') == turn]

    def test_background_command_is_collected_by_a_parked_turn(self):
        client = self.client('echo,shell,wait')
        client.request('create', bot='Bob', workspace=str(self.path))
        started = client.request('submit', bot='Bob', request_id='bg', prompt='bg:sleep .3; printf out; printf err >&2; exit 3')['result']['turn']
        self.assertEqual(client.finished(started)['data']['status'], 'completed')
        handle = self.tool_output(client, 'Bob', 'bg-1')['handle']
        self.assertTrue(handle.startswith('proc:'))
        waited = client.request('submit', bot='Bob', request_id='w', prompt='wait:' + handle)['result']['turn']
        self.assertEqual(client.finished(waited)['data']['status'], 'completed')
        kinds = self.kinds(client, 'Bob', waited)
        self.assertEqual(kinds.index('turn_waiting') + 1, kinds.index('turn_resumed'))
        result = self.tool_output(client, 'Bob', 'wait-1')['results'][handle]
        self.assertEqual((result['stdout'], result['stderr'], result['exit_code']), ('out', 'err', 3))
        # Process results are durable: a second wait returns the same outcome.
        again = client.request('submit', bot='Bob', request_id='w2', prompt='wait:' + handle)['result']['turn']
        self.assertEqual(client.finished(again)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'Bob', 'wait-1')['results'][handle]['exit_code'], 3)
        unknown = client.request('submit', bot='Bob', request_id='w3', prompt='wait:proc:999')['result']['turn']
        self.assertEqual(client.finished(unknown)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'Bob', 'wait-1')['results']['proc:999']['error'], 'unknown_handle')

    def test_peer_turn_handles_resolve_with_status_and_text(self):
        client = self.client('echo,shell,wait')
        for bot in ('Bob', 'Alice'):
            client.request('create', bot=bot, workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='slow')['result']
        self.assertEqual(alice['handle'], f"turn:Alice/{alice['turn']}")
        bob = client.request('submit', bot='Bob', request_id='b', prompt='wait:' + alice['handle'])['result']['turn']
        self.assertEqual(client.request('resume', bot='Bob')['result']['status'] in ('running', 'waiting'), True)
        self.assertEqual(client.finished(alice['turn'])['data']['status'], 'completed')
        self.assertEqual(client.finished(bob)['data']['status'], 'completed')
        result = self.tool_output(client, 'Bob', 'wait-1')['results'][alice['handle']]
        self.assertEqual((result['status'], result['text'], result['turn']), ('completed', 'reply:slow', alice['turn']))
        self.assertIsNotNone(result['checkpoint'])
        # Waiting on an already finished turn resolves without parking.
        done = client.request('submit', bot='Bob', request_id='b2', prompt='wait:' + alice['handle'])['result']['turn']
        self.assertEqual(client.finished(done)['data']['status'], 'completed')
        self.assertIn('turn_resumed', self.kinds(client, 'Bob', done))
        # A turn cannot wait on itself, and unknown peers are errors, not hangs.
        bad = client.request('submit', bot='Bob', request_id='b3', prompt='wait:turn:Nobody/1')['result']['turn']
        self.assertEqual(client.finished(bad)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'Bob', 'wait-1')['results']['turn:Nobody/1']['error'], 'turn_not_found')

    def test_timeout_reports_pending_handles_and_interrupt_ends_a_parked_turn(self):
        client = self.client('echo,shell,wait')
        for bot in ('Bob', 'Alice', 'Carol'):
            client.request('create', bot=bot, workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='wait')['result']
        bob = client.request('submit', bot='Bob', request_id='b', prompt='waitt:300:' + alice['handle'])['result']['turn']
        self.assertEqual(client.finished(bob)['data']['status'], 'completed')
        outcome = self.tool_output(client, 'Bob', 'wait-1')
        self.assertEqual(outcome['pending'], [alice['handle']])
        self.assertTrue(outcome['results'][alice['handle']]['pending'])
        carol = client.request('submit', bot='Carol', request_id='c', prompt='wait:' + alice['handle'])['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == carol)
        self.assertEqual(client.request('resume', bot='Carol')['result']['status'], 'waiting')
        self.assertEqual(client.request('interrupt', bot='Carol', turn=carol + 1)['error'], 'stale_turn')
        self.assertTrue(client.request('interrupt', bot='Carol', turn=carol)['result']['parked'])
        self.assertEqual(client.finished(carol)['data']['status'], 'interrupted')
        self.assertEqual(client.request('resume', bot='Carol')['result']['status'], 'interrupted')
        # The abandoned wait received a cancellation result, so the history stays valid.
        cancelled = self.tool_output(client, 'Carol', 'wait-1')
        self.assertEqual(cancelled['error'], 'cancelled')
        client.request('interrupt', bot='Alice', turn=alice['turn'])
        self.assertEqual(client.finished(alice['turn'])['data']['status'], 'interrupted')
        # The interrupted parked turn left no uncertain tool; Carol can work again.
        again = client.request('submit', bot='Carol', request_id='c2', prompt='hi')['result']['turn']
        self.assertEqual(client.finished(again)['data']['status'], 'completed')

    def test_parked_turns_survive_restart_and_lost_processes_are_reported(self):
        client = self.client('echo,shell,wait')
        for bot in ('Bob', 'Alice', 'Dave'):
            client.request('create', bot=bot, workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='wait')['result']
        bob = client.request('submit', bot='Bob', request_id='b', prompt='wait:' + alice['handle'])['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == bob)
        dave = client.request('submit', bot='Dave', request_id='d', prompt='bgwait:sleep 30')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == dave)
        client.close(kill=True)
        client = self.client('echo,shell,wait')
        self.assertEqual(client.request('resume', bot='Alice')['result']['status'], 'interrupted')
        self.assertEqual(client.finished(bob)['data']['status'], 'completed')
        result = self.tool_output(client, 'Bob', 'wait-1')['results'][alice['handle']]
        self.assertEqual((result['status'], result['error']), ('interrupted', 'process_interrupted'))
        self.assertEqual(client.finished(dave)['data']['status'], 'completed')
        outcome = self.tool_output(client, 'Dave', 'wait-1')
        lost = list(outcome['results'].keys())[0]
        self.assertEqual(outcome['results'][lost]['error'], 'process_lost')
        # Process ids are store-wide: a new background command never reuses the lost handle.
        fresh = client.request('submit', bot='Dave', request_id='d2', prompt='bg:printf fresh')['result']['turn']
        self.assertEqual(client.finished(fresh)['data']['status'], 'completed')
        self.assertNotEqual(self.tool_output(client, 'Dave', 'bg-1')['handle'], lost)
        relook = client.request('submit', bot='Dave', request_id='d3', prompt='wait:' + lost)['result']['turn']
        self.assertEqual(client.finished(relook)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'Dave', 'wait-1')['results'][lost]['error'], 'process_lost')

    def test_calls_after_a_wait_in_the_same_response_run_on_resume(self):
        client = self.client('echo,shell,wait')
        for bot in ('Bob', 'Alice'):
            client.request('create', bot=bot, workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='slow')['result']
        bob = client.request('submit', bot='Bob', request_id='b', prompt='waitthen:' + alice['handle'])['result']['turn']
        self.assertEqual(client.finished(bob)['data']['status'], 'completed')
        kinds = self.kinds(client, 'Bob', bob)
        # The echo planned after the wait completes after the resume, before the next model call.
        self.assertLess(kinds.index('turn_resumed'), len(kinds) - 1)
        self.assertEqual(kinds.count('tool_completed'), 2)
        echoed = self.tool_output(client, 'Bob', 'after-1')
        self.assertEqual(echoed, 'after-the-wait')
        self.assertEqual(client.request('resume', bot='Bob')['result']['status'], 'completed')

    def test_protocol_wait_answers_clients_without_a_turn(self):
        client = self.client('echo,shell,wait')
        for bot in ('Bob', 'Alice'):
            client.request('create', bot=bot, workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='slow')['result']
        answer = client.request('wait', handles=[alice['handle'], 'proc:12345'], timeout_ms=5000)['result']
        self.assertEqual(answer['pending'], [])
        self.assertEqual(answer['results'][alice['handle']]['text'], 'reply:slow')
        self.assertEqual(answer['results']['proc:12345']['error'], 'unknown_handle')
        again = client.request('submit', bot='Alice', request_id='a2', prompt='wait')['result']
        timed = client.request('wait', handles=[again['handle']], timeout_ms=200)['result']
        self.assertEqual(timed['pending'], [again['handle']])
        client.request('interrupt', bot='Alice', turn=again['turn'])
        client.finished(again['turn'])

    def test_any_mode_returns_the_first_result_and_keeps_the_rest_valid(self):
        client = self.client('echo,shell,wait')
        for bot in ('Bob', 'Alice', 'Carol'):
            client.request('create', bot=bot, workspace=str(self.path))
        slow = client.request('submit', bot='Alice', request_id='a', prompt='wait')['result']
        quick = client.request('submit', bot='Carol', request_id='c', prompt='hi')['result']
        # The protocol op answers as soon as one handle resolves.
        first = client.request('wait', handles=[slow['handle'], quick['handle']], any=True, timeout_ms=5000)['result']
        self.assertEqual(first['results'][quick['handle']]['text'], 'reply:hi')
        self.assertEqual(first['pending'], [slow['handle']])
        # The tool does the same and the turn continues with what it got.
        bob = client.request('submit', bot='Bob', request_id='b',
                             prompt=f"waitany:{slow['handle']},{quick['handle']}")['result']['turn']
        self.assertEqual(client.finished(bob)['data']['status'], 'completed')
        outcome = self.tool_output(client, 'Bob', 'wait-1')
        self.assertEqual(outcome['pending'], [slow['handle']])
        self.assertEqual(outcome['results'][quick['handle']]['text'], 'reply:hi')
        # The pending handle stays valid: an all-mode wait still gets it.
        later = client.request('wait', handles=[slow['handle']], timeout_ms=10000)['result']
        self.assertEqual((later['pending'], later['results'][slow['handle']]['text']), ([], 'reply:wait'))

    def test_rejected_wait_preserves_the_rest_of_the_tool_batch(self):
        client = self.client('echo,wait')
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='invalid',
                              prompt='waitthen:bad-handle')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.tool_output(client, 'Bob', 'wait-1')['error'], 'invalid_handle')
        self.assertEqual(self.tool_output(client, 'Bob', 'after-1'), 'after-the-wait')

    def test_protocol_wait_rejects_malformed_handles_without_hanging(self):
        client = self.client('echo,wait')
        for handle in ('bad-handle', 'proc:01', 'turn:Bob/+1'):
            with self.subTest(handle=handle):
                result = client.request('wait', handles=[handle], timeout_ms=20)
                self.assertEqual(result['error'], 'invalid_handle')
        self.assertIn('result', client.request('wait', handles=['proc:999']))

    def test_large_wait_response_returns_an_error_and_keeps_connection_usable(self):
        client = self.client('echo,shell,wait')
        handles = []
        for n in range(9):
            bot = f'output-{n}'
            client.request('create', bot=bot, workspace=str(self.path))
            turn = client.request('submit', bot=bot, request_id='output',
                                  prompt="bg:printf '%065536d' 0; printf '%065536d' 0 >&2")['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            handle = self.tool_output(client, bot, 'bg-1')['handle']
            handles.append(handle)
            result = client.request('wait', handles=[handle])['result']['results'][handle]
            self.assertEqual((len(result['stdout']), len(result['stderr'])), (65536, 65536))
        self.assertEqual(client.request('wait', handles=handles)['error'], 'response_size_limit')
        self.assertEqual(client.request('wait', handles=[handles[0]])['result']['pending'], [])

    def test_cancelled_batch_results_match_live_socket_events_and_replay(self):
        client = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'echo,wait')
        self.addCleanup(client.close)
        for bot in ('Alice', 'Bob'):
            client.request('create', bot=bot, workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='wait')['result']
        bob = client.request('submit', bot='Bob', request_id='b',
                             prompt='waitthen:' + alice['handle'])['result']['turn']
        client.followers['Bob'].receive(lambda e: e.get('event') == 'turn_waiting')
        client.request('interrupt', bot='Bob', turn=bob)
        self.assertEqual(client.finished(bob)['data']['status'], 'interrupted')
        replay = client.request('events', bot='Bob', after=0, limit=256)['result']
        client.verify_followers({'Bob': replay})
        completed = [e for e in replay['events'] if e['event'] == 'tool_completed']
        self.assertEqual([e['data']['call_id'] for e in completed], ['wait-1', 'after-1'])
        self.assertTrue(all(e['data']['cancelled'] for e in completed))

    def test_fan_out_with_a_tiny_process_budget_never_deadlocks(self):
        client = Client(self.binary, self.path / 'state.sqlite', self.url, 'echo,shell,wait', extra=('--max-processes', '2'))
        self.addCleanup(client.close)
        turns = []
        for n in range(8):
            client.request('create', bot=f'parent-{n}', workspace=str(self.path))
            turns.append(client.request('submit', bot=f'parent-{n}', request_id='r',
                                        prompt=f'bgwait:sleep .2; printf done-{n}')['result']['turn'])
        deadline = time.monotonic() + 10
        for turn in turns:
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertLess(time.monotonic(), deadline)
        for n in range(8):
            outcome = self.tool_output(client, f'parent-{n}', 'wait-1')
            self.assertEqual(list(outcome['results'].values())[0]['stdout'], f'done-{n}')

    def test_shutdown_releases_pending_protocol_waits_without_stdin_eof(self):
        client = self.client('echo,shell,wait')
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='bg', prompt='bg:sleep 1')['result']['turn']
        client.finished(turn)
        handle = self.tool_output(client, 'Bob', 'bg-1')['handle']
        client.process.stdin.write(json.dumps({'id': 'pending', 'op': 'wait', 'handles': [handle],
                                              'timeout_ms': 60000}) + '\n')
        client.process.stdin.flush()
        try:
            self.assertTrue(client.request('shutdown')['result']['shutting_down'])
            self.assertEqual(client.process.wait(timeout=2), 0)
        finally:
            client.close(kill=True)

    def test_resumptions_obey_capacity_and_stale_queued_turns_are_skipped(self):
        self.model.release_headers = threading.Event()
        self.model.all_streaming = threading.Event()
        self.model.all_streaming.set()
        self.model.wait_resumed = queue.Queue()
        self.model.release_waiters = threading.Event()
        client = Client(self.binary, self.path / 'state.sqlite', self.url, 'wait', extra=('--max-active', '2'))
        self.addCleanup(client.close)
        self.addCleanup(self.model.release_waiters.set)
        self.addCleanup(self.model.release_headers.set)
        client.request('create', bot='Anchor', workspace=str(self.path))
        anchor = client.request('submit', bot='Anchor', request_id='a', prompt='gate')['result']
        turns = {}
        for n in range(4):
            bot = f'Waiter{n}'
            client.request('create', bot=bot, workspace=str(self.path), instructions=bot)
            deadline = time.monotonic() + 2
            while True:
                result = client.request('submit', bot=bot, request_id='w', prompt='waitgate:' + anchor['handle'])
                if 'result' in result:
                    break
                self.assertEqual(result['error'], 'active_agent_limit')
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.01)
            turn = turns[bot] = result['result']['turn']
            client.receive(lambda e: e.get('event') == 'turn_waiting' and e.get('turn') == turn)
        self.model.release_headers.set()
        live = {self.model.wait_resumed.get(timeout=2) for _ in range(2)}
        with self.assertRaises(queue.Empty):
            self.model.wait_resumed.get(timeout=.2)
        cancelled = next(bot for bot in turns if bot not in live)
        client.request('interrupt', bot=cancelled, turn=turns[cancelled])
        self.assertEqual(client.finished(turns[cancelled])['data']['status'], 'interrupted')
        deleted = next(bot for bot in turns if bot not in live and bot != cancelled)
        client.request('interrupt', bot=deleted, turn=turns[deleted])
        self.assertEqual(client.finished(turns[deleted])['data']['status'], 'interrupted')
        self.assertIn('result', client.request('delete', bot=deleted))
        self.model.release_waiters.set()
        for bot, turn in turns.items():
            if bot not in (cancelled, deleted):
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(client.request('resume', bot=deleted)['error'], 'bot_not_found')
        fresh = client.request('submit', bot=cancelled, request_id='fresh', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(fresh)['data']['status'], 'completed')

    def test_round_budget_survives_parking_and_daemon_restart(self):
        self.model.release_headers = threading.Event()
        self.model.all_streaming = threading.Event()
        self.model.all_streaming.set()
        self.addCleanup(self.model.release_headers.set)
        client = self.client('wait')
        for bot in ('Anchor', 'Loop'):
            client.request('create', bot=bot, workspace=str(self.path))
        anchor = client.request('submit', bot='Anchor', request_id='a', prompt='gate')['result']
        prompt = 'budget:' + anchor['handle']
        turn = client.request('submit', bot='Loop', request_id='loop', prompt=prompt)['result']['turn']
        client.receive(lambda e: e.get('event') == 'turn_waiting' and e.get('turn') == turn
                       and e['data']['handles'] == [anchor['handle']], timeout=10)
        client.close(kill=True)
        self.model.release_headers.set()
        client = self.client('wait')
        end = client.receive(lambda e: e.get('event') == 'turn_finished' and e.get('turn') == turn, timeout=10)
        self.assertEqual(end['data']['error'], 'tool_round_limit')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get_nowait())
        loop_calls = [r for r in requests if r['input'][0]['content'][0]['text'] == prompt]
        self.assertEqual(len(loop_calls), 200)

    def test_background_persistence_failure_disconnects_and_recovers_without_reexecution(self):
        for stage, trigger in (
            ('result', 'BEFORE UPDATE OF result ON processes'),
            ('artifact', 'BEFORE INSERT ON artifacts'),
        ):
            with self.subTest(stage=stage):
                workspace = self.path / stage
                workspace.mkdir()
                database = workspace / 'state.sqlite'
                client = Client(self.binary, database, self.url, 'echo,shell,wait')
                self.addCleanup(lambda c=client: c.close(kill=True))
                client.request('create', bot='Bob', workspace=str(workspace))
                with sqlite3.connect(database) as db:
                    db.execute(f"CREATE TRIGGER reject_completion {trigger} BEGIN "
                               "SELECT RAISE(ABORT, 'synthetic write failure'); END")
                command = "while [ ! -f release ]; do sleep .01; done; printf done >> marker; printf '%070000d' 0"
                started = client.request('submit', bot='Bob', request_id='bg', prompt='bg:' + command)['result']['turn']
                self.assertEqual(client.finished(started)['data']['status'], 'completed')
                handle = self.tool_output(client, 'Bob', 'bg-1')['handle']
                waiting = client.request('submit', bot='Bob', request_id='w', prompt='wait:' + handle)['result']['turn']
                client.receive(lambda e: e.get('event') == 'turn_waiting' and e.get('turn') == waiting)
                client.process.stdin.write(json.dumps({'id': 'pending', 'op': 'wait', 'handles': [handle]}) + '\n')
                client.process.stdin.flush()
                (workspace / 'release').touch()
                try:
                    self.assertEqual(client.process.wait(timeout=2), 1)
                    with self.assertRaisesRegex(AssertionError, 'runtime exited'):
                        client.receive(lambda e: e.get('id') == 'pending')
                finally:
                    client.close(kill=True)
                with sqlite3.connect(database) as db:
                    self.assertEqual(db.execute('SELECT status,result FROM processes').fetchall(), [('running', None)])
                    self.assertEqual(db.execute('SELECT count(*) FROM artifacts').fetchone()[0], 0)
                    db.execute('DROP TRIGGER reject_completion')
                recovered = Client(self.binary, database, self.url, 'echo,shell,wait')
                self.addCleanup(recovered.close)
                self.assertEqual(recovered.finished(waiting)['data']['status'], 'completed')
                outcome = self.tool_output(recovered, 'Bob', 'wait-1')['results'][handle]
                self.assertEqual(outcome['error'], 'process_lost')
                self.assertEqual((workspace / 'marker').read_text(), 'done')
                # Repaired storage accepts both a fresh result and its overflow.
                fresh = recovered.request('submit', bot='Bob', request_id='fresh',
                                          prompt="bg:printf '%070000d' 0")['result']['turn']
                self.assertEqual(recovered.finished(fresh)['data']['status'], 'completed')
                fresh_handle = self.tool_output(recovered, 'Bob', 'bg-1')['handle']
                result = recovered.request('wait', handles=[fresh_handle])['result']['results'][fresh_handle]
                self.assertEqual(result['exit_code'], 0)
                page = recovered.request('artifact', bot='Bob', turn=fresh, call_id='bg-1',
                                         stream='stdout', offset=69996, limit=4)['result']
                self.assertEqual((page['text'], page['total_bytes'], page['done']), ('0000', 70000, True))
