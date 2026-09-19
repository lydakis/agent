"""Measurement contracts for the store-scale screen."""
from collections import Counter
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock

from bench.store_scale import CONCURRENCY, HEAVY, SHELL, TEXT, Screen
from bench.synthetic_model import start


class StoreScaleTests(unittest.TestCase):
    def test_growth_balances_shapes_within_each_heavy_bot(self):
        screen = Screen(None, None, None, None, 4096, None)
        batch = screen.growth_batch(0, 4 * HEAVY * 2)
        self.assertEqual(Counter(prompt for _, prompt in batch), {TEXT: 32, SHELL: 32})
        for i in range(HEAVY):
            self.assertEqual(Counter(p for bot, p in batch if bot == f'h{i}'), {TEXT: 1, SHELL: 1})
        self.assertEqual(sum(bot.startswith('h') for bot, _ in batch), len(batch) // 4)

    def test_crash_requires_new_arrivals_and_fails_without_killing(self):
        server = SimpleNamespace(requests=10000)
        screen = Screen(None, None, None, server, CONCURRENCY, None)
        client = screen.client = Mock()
        client.request.side_effect = [{'result': {'turn': n}} for n in range(CONCURRENCY)]
        with self.assertRaisesRegex(TimeoutError, '0/32'):
            screen.crash_restart(timeout=0)
        client.close.assert_not_called()


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires local runtime and sockets')
class StoreScaleRuntimeTests(unittest.TestCase):
    def test_repeated_crash_checkpoints_recover_every_held_turn(self):
        with tempfile.TemporaryDirectory() as folder:
            server, url = start()
            screen = Screen(Path('.local/target/release/agent').resolve(), Path(folder) / 'state.sqlite',
                            url, server, CONCURRENCY, SimpleNamespace(pid=None))
            try:
                screen.connect()
                screen.create_all()
                for _ in range(2):
                    screen.run_batch([(screen.name(i), TEXT) for i in range(CONCURRENCY)])
                    result = screen.crash_restart()
                    self.assertEqual(result['requests_started'], CONCURRENCY)
                    self.assertEqual(result['recovered_turns'], CONCURRENCY)
            finally:
                if screen.client:
                    screen.client.close(kill=True)
                server.release.set()
                server.shutdown()
                server.server_close()
