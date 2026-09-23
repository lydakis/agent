"""Soak observer contracts and a short mixed-workload restart check."""
import json
import os
import shutil
import socket
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from bench.soak import PACE, ROLES, Soak
from bench.soak_followers import JournalFollower, SlowFollower


class TimingTests(unittest.TestCase):
    def test_turn_latency_includes_submission_and_early_completion(self):
        soak = Soak.__new__(Soak)
        soak.turn_counter, soak.in_flight, soak.next_at = {}, {}, {}
        soak.finished, soak.terminal_at, soak.turn_ms = {}, {}, []
        soak.counts = {'turns': 0, 'completed': 0}
        clock = [10.0]

        def acknowledge(*args, **kwargs):
            # The event connection can receive completion before the control
            # connection's acknowledgement is consumed by the observer.
            soak.finished[1] = {'data': {'status': 'completed'}, '_received_at': 10.1}
            clock[0] = 10.2
            return {'result': {'turn': 1}}

        soak.control = Mock(request=acknowledge)
        with patch('bench.soak.time.monotonic', side_effect=lambda: clock[0]):
            soak.submit('plain', 'bot', 'hello')
            soak.collect()
        self.assertAlmostEqual(soak.turn_ms[0], 100.0)
        self.assertEqual(soak.counts, {'turns': 1, 'completed': 1})


class ReplayTests(unittest.TestCase):
    def test_complete_retained_interval_is_required_across_pages(self):
        events = [{'event': 'sample', 'cursor': n} for n in range(1, 6)]
        for label, observed, expected in (
                ('complete', events, True), ('empty', [], False),
                ('suffix', events[-1:], False), ('gap', events[:3] + events[4:], False),
                ('duplicate', events + events[-1:], False),
                ('changed', events[:-1] + [dict(events[-1], data='wrong')], False)):
            with self.subTest(label=label), tempfile.TemporaryFile(mode='w+') as journal:
                follower = JournalFollower.__new__(JournalFollower)
                follower.initialize(journal)
                for event in observed:
                    follower.record(event)
                control = Mock()
                control.request.side_effect = [
                    {'result': {'events': events[2:4], 'next_cursor': 4, 'pruned_before': 2}},
                    {'result': {'events': events[4:], 'next_cursor': 5}},
                    {'result': {'events': [], 'next_cursor': 5}},
                ]
                self.assertEqual(follower.matches(control, 'bot', timeout=0), expected)
                self.assertEqual([c.kwargs['after'] for c in control.request.call_args_list], [0, 4, 5])


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires the release binary and sockets')
class SoakTests(unittest.TestCase):
    def test_slow_follower_throttles_reads_and_observes_peer_close(self):
        for peer_closes in (True, False):
            with self.subTest(peer_closes=peer_closes):
                with tempfile.TemporaryDirectory(prefix='soak-peer-', dir='/tmp') as directory:
                    path = Path(directory) / 'peer.sock'
                    with socket.socket(socket.AF_UNIX) as listener:
                        listener.bind(str(path))
                        listener.listen()
                        sent, release = threading.Event(), threading.Event()

                        def serve():
                            peer, _ = listener.accept()
                            with peer:
                                peer.recv(4096)
                                peer.sendall(b'x' * 1024)
                                sent.set()
                                release.wait(3)

                        worker = threading.Thread(target=serve)
                        worker.start()
                        started = time.monotonic()
                        follower = SlowFollower(path, 'bot', pause=.1)
                        try:
                            self.assertTrue(sent.wait(1))
                            deadline = time.monotonic() + 2
                            while follower.bytes_read < 1024 and time.monotonic() < deadline:
                                time.sleep(.01)
                            self.assertEqual(follower.bytes_read, 1024)
                            self.assertGreaterEqual(time.monotonic() - started, .35)
                            self.assertFalse(follower.disconnected)
                            if peer_closes:
                                release.set()
                                follower.worker.join(timeout=1)
                                self.assertTrue(follower.disconnected)
                            else:
                                follower.close()
                                self.assertFalse(follower.disconnected)
                        finally:
                            release.set()
                            follower.close()
                            worker.join(timeout=3)

    def test_journal_waits_for_the_replay_endpoint(self):
        with tempfile.TemporaryDirectory(prefix='soak-peer-', dir='/tmp') as directory:
            path = Path(directory) / 'peer.sock'
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(str(path))
                listener.listen()
                release, finish = threading.Event(), threading.Event()
                event = {'event': 'sample', 'cursor': 1}

                def serve():
                    peer, _ = listener.accept()
                    with peer:
                        peer.sendall(b'{"event":"ready"}\n')
                        peer.recv(4096)
                        peer.sendall(b'{"id":1,"result":{}}\n')
                        release.wait(2)
                        peer.sendall((json.dumps(event) + '\n').encode())
                        finish.wait(2)

                worker = threading.Thread(target=serve)
                worker.start()
                follower = JournalFollower(path, 'bot')
                timer = threading.Timer(.1, release.set)
                timer.start()
                try:
                    control = Mock()
                    control.request.side_effect = [
                        {'result': {'events': [event], 'next_cursor': 1}},
                        {'result': {'events': [], 'next_cursor': 1}}]
                    self.assertTrue(follower.matches(control, 'bot', timeout=1))
                    self.assertEqual(follower.durable, [])
                    self.assertTrue(follower.queue.empty())
                finally:
                    release.set()
                    finish.set()
                    timer.join()
                    follower.close()
                    worker.join(timeout=3)

    def test_short_soak_checks_followers_and_restarts_cleanly(self):
        out = Path(tempfile.mkdtemp(prefix='soak-test-', dir='/tmp')) / 'run'
        try:
            soak = Soak(Path('.local/target/release/agent').resolve(), out, minutes=0.25, seed=1)
            socket_directory = soak.socket.parent
            self.assertTrue(socket_directory.is_dir())
            result = soak.run()
            self.assertFalse(socket_directory.exists())
            self.assertEqual(result['schema'], 'soak_v3')
            self.assertEqual(result['turn_ms']['boundary'], 'before_submit_to_terminal_receive')
            counts = result['counts']
            self.assertEqual(counts['turns'], counts['completed'] + counts['failed'] + counts['interrupted'])
            self.assertGreater(counts['turns'], sum(ROLES.values()))
            self.assertEqual(counts['failed'], 0, result['errors'])
            self.assertGreaterEqual(counts['interrupt_attempts'], 1)
            self.assertGreaterEqual(counts['interrupted'], 1, result['errors'])
            self.assertGreaterEqual(result['cancel_ms']['n'], 1)
            self.assertEqual(result['follower_mismatches'], 0)
            self.assertEqual(len(result['slow_followers']), 4)
            self.assertTrue(all(f['bytes_read'] > 0 for f in result['slow_followers']))
            self.assertEqual(counts['follower_disconnects'], sum(f['disconnected'] for f in result['slow_followers']))
            self.assertEqual(result['restart']['bot_statuses'].get('interrupted'), result['restart']['held'])
            self.assertGreater(result['provider_requests'], counts['turns'])
            self.assertTrue(result['samples'])
            self.assertEqual(set(PACE) - {'fork'}, set(ROLES))
            (out / 'result.json').write_text(json.dumps(result))
        finally:
            shutil.rmtree(out.parent, ignore_errors=True)
