"""Fleet outcomes, measured latency boundaries, and viable parking configuration."""
import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from bench.fleet_screen import Screen
from bench.targets import clean_env


class FleetScreenTests(unittest.TestCase):
    def test_outcomes_and_latency_use_submission_to_receipt_not_batch_processing(self):
        screen = Screen.__new__(Screen)
        screen.bots = screen.max_active = 3
        screen.finished = {}
        now = [0.0]
        outcomes = [{'status': 'completed'}, {'status': 'failed', 'error': 'provider_http_429'},
                    {'status': 'interrupted', 'error': 'cancelled'}]

        def submit(op, *, bot, **kwargs):
            index = int(bot[1:])
            # The reply costs 10 ms and the completion is received another
            # 10 ms later; subsequent submissions delay processing that event.
            received = now[0] + .020
            now[0] += .010
            screen.absorb({'event': 'turn_finished', 'turn': index + 1,
                           'data': outcomes[index], '_received_at': received})
            return {'result': {'turn': index + 1}}

        screen.client = SimpleNamespace(request=submit)
        screen.pump = lambda: now.__setitem__(0, now[0] + 1.0)
        screen.sample = lambda label: {}
        with patch('bench.fleet_screen.time.monotonic', side_effect=lambda: now[0]):
            result = screen.run_all('test', 'hello')
        self.assertEqual((result['finished'], result['completed'], result['failed']), (3, 1, 2))
        self.assertEqual(result['refusals'], {'finished:provider_http_429': 1, 'finished:cancelled': 1})
        self.assertAlmostEqual(result['turns_per_second'] * 3, result['finished_per_second'], delta=.2)
        self.assertEqual((result['p50_ms'], result['p95_ms'], result['max_ms']), (20, 20, 20))

    def test_zero_active_limit_submits_the_whole_fleet_before_waiting(self):
        for limit, expected_batches in ((0, [5]), (2, [2, 2, 1])):
            with self.subTest(limit=limit):
                screen = Screen.__new__(Screen)
                screen.bots, screen.max_active = 5, limit
                screen.finished = {}
                outstanding, batches = [], []

                def submit(op, *, bot, **kwargs):
                    turn = int(bot[1:]) + 1
                    outstanding.append(turn)
                    return {'result': {'turn': turn}}

                def pump():
                    batches.append(len(outstanding))
                    for turn in outstanding:
                        screen.absorb({'event': 'turn_finished', 'turn': turn,
                                       'data': {'status': 'completed'}, '_received_at': 1.0})
                    outstanding.clear()

                screen.client = SimpleNamespace(request=submit)
                screen.pump = pump
                screen.sample = lambda label: {}
                with patch('bench.fleet_screen.time.monotonic', return_value=0.0):
                    result = screen.run_all('test', 'hello')
                self.assertEqual(result['completed'], 5)
                self.assertEqual(batches, expected_batches)

    def test_single_slot_parking_is_rejected_before_creating_the_workspace(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / 'screen'
            with self.assertRaisesRegex(ValueError, 'parking requires.*2'):
                Screen(Path('agent'), out, bots=2, max_active=1, model=None,
                       parked=1, delay_ms=0)
            self.assertFalse(out.exists())


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class FleetRestartTests(unittest.TestCase):
    def test_restart_recovers_held_turns_for_single_and_full_wave_fleets(self):
        root = Path(__file__).resolve().parent.parent
        for bots, limit in ((1, 0), (8, 0), (8, 2)):
            with self.subTest(bots=bots, limit=limit), tempfile.TemporaryDirectory(dir=root/'.local') as d:
                screen = Screen(root/'.local/target/release/agent', Path(d), bots=bots,
                                max_active=limit, model=None, parked=0, delay_ms=0)
                screen.env = clean_env()
                screen.daemon()
                original = screen.client
                try:
                    screen.create_all()
                    # Full-batch completion used to skip the kill entirely.
                    screen.restart()
                    phase = screen.phases['restart']
                    expected = min(limit or bots, bots)
                    self.assertIsNotNone(original.process.poll())
                    self.assertEqual(phase['in_flight_at_kill'], expected)
                    self.assertEqual(phase['bot_statuses'].get('interrupted'), expected)
                    for i in range(expected):
                        row = screen.client.request('turns', bot=f'b{i}')['result']['turns'][-1]
                        self.assertEqual(row['status'], 'interrupted')
                    # Recovery releases the active slots for new work.
                    self.assertEqual(screen.run_all('after', 'delay:0')['completed'], bots)
                finally:
                    original.close(kill=True)
                    if screen.client is not original:
                        screen.client.close(kill=True)
                    screen.server.release.set()
                    screen.server.shutdown()
                    screen.server.server_close()
