"""Retry accounting survives cancellation and parked execution segments."""
import os
import threading
import time
import unittest

from tests.test_runtime import ModelFixture


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class RetryAccountingTests(ModelFixture):
    def submit(self, client, bot, prompt):
        client.request('create', bot=bot, workspace=str(self.path))
        response = client.request('submit', bot=bot, request_id='run', prompt=prompt)
        self.assertIn('result', response)
        return response['result']['turn']

    def interrupt(self, client, bot, turn):
        client.request('interrupt', bot=bot, turn=turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        return client.request('turns', bot=bot)['result']['turns'][0]

    def test_completed_retries_and_waits_survive_interrupt_and_restart(self):
        self.model.retry_delays = ['0.05', '0.05', '10']
        client = self.client()
        turn = self.submit(client, 'Bob', 'limited-forever:cancel')
        client.receive(lambda e: e.get('event') == 'retry' and e.get('attempt') == 3)
        row = self.interrupt(client, 'Bob', turn)
        self.assertEqual(self.model.requests.qsize(), 3)
        self.assertEqual(row['retries'], 2)
        self.assertGreaterEqual(row['paced_ms'], 90)
        client.close()
        client = self.client()
        persisted = client.request('turns', bot='Bob')['result']['turns'][0]
        self.assertEqual((persisted['retries'], persisted['paced_ms']), (2, row['paced_ms']))
        following = client.request('submit', bot='Bob', request_id='next', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(following)['data']['status'], 'completed')
        self.assertEqual(client.request('turns', bot='Bob')['result']['turns'][-1]['retries'], 0)

    def test_cancelled_backoff_is_not_a_retry_but_dispatched_retry_is(self):
        client = self.client()
        turn = self.submit(client, 'Backoff', 'flaky:backoff')
        client.receive(lambda e: e.get('event') == 'retry' and e.get('turn') == turn)
        self.assertEqual(self.interrupt(client, 'Backoff', turn)['retries'], 0)
        self.model.gate_entered = threading.Event()
        self.model.release_headers = threading.Event()
        self.addCleanup(self.model.release_headers.set)
        turn = self.submit(client, 'Dispatched', 'flaky:gate')
        self.assertTrue(self.model.gate_entered.wait(3))
        self.assertEqual(self.interrupt(client, 'Dispatched', turn)['retries'], 1)

    def test_partial_pacing_wait_is_recorded_without_dispatch(self):
        client = self.client()
        warm = self.submit(client, 'Warm', 'paced:1')
        self.assertEqual(client.finished(warm)['data']['status'], 'completed')
        turn = self.submit(client, 'Waiting', 'hello')
        time.sleep(.1)
        row = self.interrupt(client, 'Waiting', turn)
        self.assertEqual(self.model.requests.qsize(), 1)
        self.assertEqual(row['retries'], 0)
        self.assertGreaterEqual(row['paced_ms'], 50)

    def test_park_and_resume_flush_retry_accounting_once(self):
        self.model.all_streaming = threading.Event()
        self.model.all_streaming.set()
        self.model.release_headers = threading.Event()
        self.addCleanup(self.model.release_headers.set)
        client = self.client('wait')
        anchor = self.submit(client, 'Anchor', 'gate')
        turn = self.submit(client, 'Bob', f'waitretry:turn:Anchor/{anchor}')
        client.receive(lambda e: e.get('event') == 'turn_waiting' and e.get('turn') == turn)
        # Wait for the parked task to retire and flush its execution segment.
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            row = client.request('turns', bot='Bob')['result']['turns'][0]
            if row['retries'] == 1:
                break
            time.sleep(.01)
        self.assertEqual(row['retries'], 1)
        self.model.release_headers.set()
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(client.request('turns', bot='Bob')['result']['turns'][0]['retries'], 1)
