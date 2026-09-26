"""A turn whose provider is rate-limited parks instead of holding an active slot."""
import json
import os
import sqlite3
import time
import unittest

from tests.test_runtime import ModelFixture


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class PacedParkTests(ModelFixture):
    def client(self, tools='echo', extra=()):
        # Two providers on one fixture: prompts decide which one is throttled.
        return super().client(tools, extra=('--provider', f'b=responses,{self.url}', *extra))

    def test_throttled_turns_park_and_leave_slots_to_a_healthy_provider(self):
        self.model.retry_delays = ['1']
        client = self.client(extra=('--max-active', '4'))
        for n in range(4):
            client.request('create', bot=f'a{n}', workspace=str(self.path))
        for n in range(4):
            client.request('create', bot=f'b{n}', workspace=str(self.path), model='b/synthetic-model')
        throttled = [client.request('submit', bot=f'a{n}', request_id='t', prompt=f'limited-forever:{n}')['result']
                     for n in range(4)]
        # Each throttled turn reaches its 429 and parks, freeing its slot; from
        # then on it cycles between a short live retry and the park.
        for t in throttled:
            client.receive(lambda m, t=t: m.get('event') == 'turn_paced' and m.get('turn') == t['turn'])
        # A parked row and its retiring task overlap briefly, so the counts are
        # not a partition; what matters is that the limit has room again.
        self.assertGreaterEqual(client.request('stats')['result']['paced_turns'], 1)
        # Throttled turns are live only for the attempt itself, so the
        # healthy provider's work gets through at once; before the fix it
        # waited the whole retry budget behind them.
        healthy = [client.request('submit', bot=f'b{n}', request_id='t', prompt='hello', delivery='queue')['result']
                   for n in range(4)]
        started = time.monotonic()
        for t in healthy:
            self.assertEqual(client.finished(t['turn'])['data']['status'], 'completed')
        self.assertLess(time.monotonic() - started, 3)
        # A paced turn is still the bot's current turn: busy to submit and
        # interruptible, whether caught parked or in a live attempt.
        self.assertEqual(client.request('submit', bot='a0', request_id='again', prompt='x')['error'], 'bot_busy')
        self.assertIn(client.request('resume', bot='a0')['result']['status'], ('paced', 'running'))
        self.assertIn('interrupt_requested', client.request('interrupt', bot='a0', turn=throttled[0]['turn'])['result'])
        self.assertEqual(client.finished(throttled[0]['turn'])['data']['status'], 'interrupted')
        for t in throttled[1:]:
            client.request('interrupt', bot=t['bot'], turn=t['turn'])

    def test_new_call_on_closed_pool_leaves_capacity_for_healthy_work(self):
        self.model.retry_delays = ['10']
        client = self.client(extra=('--max-active', '1'))
        for bot in ('Seed', 'Blocked', 'Healthy'):
            client.request('create', bot=bot, workspace=str(self.path),
                           model=('b' if bot == 'Healthy' else 'openai') + '/synthetic-model')
        started = time.monotonic()
        seed = client.request('submit', bot='Seed', request_id='s', prompt='limited-forever:seed')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == seed)
        blocked = client.request('submit', bot='Blocked', request_id='b', prompt='hello', delivery='queue')['result']['turn']
        healthy = client.request('submit', bot='Healthy', request_id='h', prompt='hello', delivery='queue')['result']['turn']
        ended = client.receive(lambda m: m.get('event') == 'turn_finished' and m.get('turn') == healthy, timeout=2)
        self.assertEqual(ended['data']['status'], 'completed')
        self.assertEqual(client.request('resume', bot='Blocked')['result']['status'], 'paced')
        # Admission waiting did not send a call or spend a retry.
        self.assertEqual(self.model.requests.qsize(), 2)
        row = client.request('turns', bot='Blocked')['result']['turns'][0]
        self.assertEqual(row['retries'], 0)
        # Reconcile both kinds of park without marking unfinished work done
        # or dispatching another provider call.
        for bot, turn, request_id, prompt, delivery in (
                ('Seed', seed, 's', 'limited-forever:seed', 'reject'),
                ('Blocked', blocked, 'b', 'hello', 'queue')):
            duplicate = client.request('submit', bot=bot, request_id=request_id,
                                       prompt=prompt, delivery=delivery)['result']
            self.assertEqual((duplicate['duplicate'], duplicate['turn'], duplicate['status']),
                             (True, turn, 'paced'))
            self.assertFalse(client.request('result', bot=bot, turn=turn)['result']['finished'])
        self.assertEqual(self.model.requests.qsize(), 2)
        time.sleep(.05)
        for bot, turn in (('Seed', seed), ('Blocked', blocked)):
            client.request('interrupt', bot=bot, turn=turn)
            client.finished(turn)
            paced_ms = client.request('turns', bot=bot)['result']['turns'][0]['paced_ms']
            self.assertGreaterEqual(paced_ms, 30)
            self.assertLessEqual(paced_ms, (time.monotonic() - started) * 1000 + 50)

    def test_elapsed_park_time_includes_late_restart_and_is_counted_once(self):
        self.model.retry_delays = ['0.3']
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        started = time.monotonic()
        turn = client.request('submit', bot='Bob', request_id='r', prompt='limited:elapsed')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn)
        client.close(kill=True)
        # The deadline expires while the daemon is down; the park lasts until
        # actual resumption, not just the provider's suggested retry time.
        time.sleep(.6)
        client = self.client()
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        row = client.request('turns', bot='Bob')['result']['turns'][0]
        self.assertGreaterEqual(row['paced_ms'], 550)
        self.assertLessEqual(row['paced_ms'], (time.monotonic() - started) * 1000 + 50)
        self.assertEqual(row['retries'], 1)
        client.close()
        client = self.client()
        again = client.request('turns', bot='Bob')['result']['turns'][0]
        self.assertEqual((again['paced_ms'], again['retries']), (row['paced_ms'], 1))

    def test_next_model_call_gets_fresh_retry_budget_after_paced_tool_call(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='r', prompt='tool:park-rounds')['result']['turn']
        ended = client.receive(lambda m: m.get('event') == 'turn_finished' and m.get('turn') == turn, timeout=10)
        self.assertEqual(ended['data']['status'], 'completed')
        self.assertEqual(self.model.attempts['tool:park-rounds'], 11)
        row = client.request('turns', bot='Bob')['result']['turns'][0]
        self.assertEqual(row['retries'], 9)

    def test_a_paced_turn_survives_restart_and_ends_at_the_paced_cap(self):
        self.model.retry_delays = ['0.3']
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='t', prompt='limited-forever:x')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn)
        client.close(kill=True)
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            retries, waiting = db.execute('SELECT retries,waiting FROM turns WHERE id=?', (turn,)).fetchone()
        self.assertEqual(retries, 0)  # The announced retry has not dispatched yet.
        self.assertEqual(json.loads(waiting)['call_attempts'], 1)
        # Retries persist in the turn row across the restart, so the cap holds
        # over the whole life of the turn, not per execution segment.
        self.model.retry_delays = ['0.02']
        client = self.client()
        client.receive(lambda m: m.get('event') == 'turn_resumed' and m.get('turn') == turn, timeout=10)
        ended = client.receive(lambda m: m.get('event') == 'turn_finished' and m.get('turn') == turn, timeout=60)
        self.assertEqual(ended['data']['status'], 'failed')
        self.assertIn(ended['data']['error'], ('provider_rate_limited', 'provider_http_429'))
        # 64 attempts across two execution segments: 63 retries, the last attempt fails.
        row = client.request('turns', bot='Bob', after=0)['result']['turns'][0]
        self.assertEqual(row['retries'], 63)
        self.assertEqual(self.model.requests.qsize(), 64)
        # The bot is usable again, and nothing about the conversation was written for the failed turn.
        again = client.request('submit', bot='Bob', request_id='again', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(again)['data']['status'], 'completed')


if __name__ == '__main__':
    unittest.main()
