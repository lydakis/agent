"""Delivery modes on submit: reject, queue, and steer."""
import json
import os
import time
import unittest

from tests.test_runtime import ModelFixture
from bench.runtime_client import Client


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class DeliveryTests(ModelFixture):
    def events(self, client, bot):
        return client.request('events', bot=bot, after=0, limit=256)['result']['events']

    def test_cancelled_queue_applies_retention_before_completion(self):
        import threading
        self.model.release_headers = threading.Event()
        self.model.all_streaming = threading.Event()
        self.model.all_streaming.set()
        self.addCleanup(self.model.release_headers.set)
        client = self.client(extra=('--max-active', '1', '--retain-turns', '1'))
        for bot in ('Alice', 'Bob'):
            client.request('create', bot=bot, workspace=str(self.path))
        first = client.request('submit', bot='Alice', request_id='a', prompt='gate')['result']['turn']
        self.model.requests.get(timeout=3)
        queued = [client.request('submit', bot='Bob', request_id=str(n), prompt='queued',
                                 delivery='queue')['result']['turn'] for n in range(8)]
        # Finish the oldest last: its captured outcome must still reach an
        # existing waiter even though this completion prunes its own result.
        handle = f'turn:Bob/{queued[0]}'
        client.next_id += 1
        wait_id = client.next_id
        client.process.stdin.write(json.dumps({'id': wait_id, 'op': 'wait', 'handles': [handle]}) + '\n')
        client.process.stdin.flush()
        for turn in reversed(queued):
            client.request('interrupt', bot='Bob', turn=turn)
            self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        waited = client.receive(lambda m: m.get('id') == wait_id)
        self.assertEqual(waited['result']['results'][handle]['status'], 'interrupted')
        self.assertEqual(client.request('result', bot='Bob', turn=queued[0])['error'], 'turn_result_pruned')
        terminal = [e for e in self.events(client, 'Bob') if e['event'] == 'turn_finished']
        self.assertEqual([e['turn'] for e in terminal], queued[-1:])
        self.assertEqual(client.request('stats')['result']['queued_turns'], 0)
        self.model.release_headers.set()
        client.finished(first)

    def test_steer_with_another_workspace_runs_there_as_its_own_turn(self):
        original, requested = self.path / 'original', self.path / 'requested'
        original.mkdir()
        requested.mkdir()
        client = self.client('echo,shell')
        client.request('create', bot='Bob', workspace=str(original))
        first = client.request('submit', bot='Bob', request_id='a', prompt='shell:sleep .3')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == first)
        steer = client.request('submit', bot='Bob', request_id='b', prompt='bg:pwd',
                               workspace=str(requested), delivery='steer')['result']['turn']
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        self.assertEqual(client.finished(steer)['data']['status'], 'completed')
        text = client.request('result', bot='Bob', turn=steer)['result']['text']
        handle = json.loads(text.removeprefix('echo:'))['handle']
        output = client.request('wait', handles=[handle])['result']['results'][handle]['stdout']
        self.assertEqual(output.strip(), str(requested))

    def test_inherited_steers_validate_the_active_model_and_recheck_before_start(self):
        import threading
        self.model.release_headers = threading.Event()
        self.model.all_streaming = threading.Event()
        self.model.all_streaming.set()
        self.addCleanup(self.model.release_headers.set)
        elsewhere = self.path / 'elsewhere'
        elsewhere.mkdir()
        client = self.client(extra=('--provider', f'changed=responses,{self.url}'))
        for bot in ('Bob', 'Changed', 'Cancelled', 'Idle'):
            client.request('create', bot=bot, workspace=str(self.path),
                           model=('changed' if bot == 'Changed' else 'openai') + '/synthetic-model')
        client.close()
        client = Client(self.binary, self.path / 'state.sqlite', self.url, provider='other',
                        extra=('--provider', f'changed=anthropic,{self.url}'))
        self.addCleanup(client.close)
        active = {}
        for bot in ('Bob', 'Changed', 'Cancelled'):
            active[bot] = client.request('submit', bot=bot, request_id='active', prompt='gate',
                                         model='other/synthetic-model')['result']['turn']
            self.model.requests.get(timeout=3)
        # Inheritance applies only to steers eligible for the running turn.
        for bot, overrides in [('Idle', {}), ('Bob', {'workspace': str(elsewhere)}),
                               ('Bob', {'model': 'openai/synthetic-model'})]:
            refused = client.request('submit', bot=bot, request_id='refused', prompt='no',
                                     delivery='steer', **overrides)
            self.assertEqual(refused['error'], 'provider_unavailable')
        steers = {}
        for bot in active:
            steers[bot] = client.request('submit', bot=bot, request_id='steer', prompt='continue',
                                         delivery='steer', workspace=str(self.path) if bot == 'Bob' else None)['result']['turn']
        # Cancellation makes this steer start alone, where the default is invalid.
        head = client.request('resume', bot='Cancelled')['result']['head']
        client.request('interrupt', bot='Cancelled', turn=active['Cancelled'])
        self.assertEqual(client.finished(active['Cancelled'])['data']['status'], 'interrupted')
        self.assertEqual(client.finished(steers['Cancelled'])['data']['error'], 'provider_unavailable')
        self.assertEqual(client.request('resume', bot='Cancelled')['result']['head'], head)
        self.model.release_headers.set()
        for bot in ('Bob', 'Changed'):
            self.assertEqual(client.finished(steers[bot])['data']['status'], 'steered')
            self.assertEqual(client.finished(active[bot])['data']['status'], 'completed')
            self.assertEqual(client.request('result', bot=bot, turn=active[bot])['result']['text'], 'reply:continue')

    def test_multiple_steer_batches_are_delivered_in_order(self):
        import threading
        self.model.release_headers = threading.Event()
        self.model.all_streaming = threading.Event()
        self.model.all_streaming.set()
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='a', prompt='gate')['result']['turn']
        self.model.requests.get(timeout=3)  # Hold the first call until all steers are durable.
        steers = [client.request('submit', bot='Bob', request_id=str(n), prompt=f'steer-{n}',
                                 delivery='steer')['result']['turn'] for n in range(70)]
        self.model.release_headers.set()
        for turn in steers:
            self.assertEqual(client.finished(turn)['data']['status'], 'steered')
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        self.assertEqual(client.request('result', bot='Bob', turn=first)['result']['text'], 'reply:steer-69')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        counts = [sum(i.get('role') == 'user' for i in r['input']) for r in requests]
        self.assertEqual(counts, [71])

    def test_interrupt_queued_work_while_the_bot_is_running(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='a', prompt='wait')['result']['turn']
        for delivery in ('queue', 'steer'):
            pending = client.request('submit', bot='Bob', request_id=delivery,
                                     prompt='unwanted', delivery=delivery)['result']['turn']
            reply = client.request('interrupt', bot='Bob', turn=pending)
            self.assertTrue(reply['result']['queued'])
            self.assertEqual(client.finished(pending)['data']['status'], 'interrupted')
        self.assertEqual(client.request('resume', bot='Bob')['result']['running_turn'], first)
        self.assertEqual(client.request('interrupt', bot='Bob', turn=999)['error'], 'stale_turn')
        client.request('interrupt', bot='Bob', turn=first)
        self.assertEqual(client.finished(first)['data']['status'], 'interrupted')

    def test_interrupt_parked_turn_dispatches_its_successor(self):
        client = self.client('echo,shell,wait')
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='a', prompt='bgwait:sleep 2')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == first)
        deadline = time.monotonic() + 3
        while client.request('stats')['result']['active_turns']:
            self.assertLess(time.monotonic(), deadline)
        successor = client.request('submit', bot='Bob', request_id='b', prompt='successor',
                                   delivery='queue')['result']['turn']
        self.assertTrue(client.request('interrupt', bot='Bob', turn=first)['result']['parked'])
        self.assertEqual(client.finished(successor)['data']['status'], 'completed')
        self.assertEqual(client.request('result', bot='Bob', turn=successor)['result']['text'], 'reply:successor')

    def test_retention_preserves_completion_with_queued_and_steered_work(self):
        client = self.client(extra=('--retain-turns', '1'))
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='a', prompt='slow')['result']['turn']
        successor = client.request('submit', bot='Bob', request_id='b', prompt='next',
                                   delivery='queue')['result']['turn']
        self.assertEqual(client.request('prune', bot='Bob', keep_turns=1)['result']['events'], 0)
        for turn in (first, successor):
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        first = client.request('submit', bot='Bob', request_id='c', prompt='slow')['result']['turn']
        steer = client.request('submit', bot='Bob', request_id='d', prompt='steering',
                               delivery='steer')['result']['turn']
        self.assertEqual(client.finished(steer)['data']['status'], 'steered')
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        self.assertEqual(client.request('stats')['result']['queued_turns'], 0)

    def test_queued_turns_run_in_order_after_the_busy_turn(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='slow')['result']
        self.assertEqual(first['status'], 'running')
        self.assertEqual(client.request('submit', bot='Bob', request_id='x', prompt='never')['error'], 'bot_busy')
        self.assertEqual(client.request('submit', bot='Bob', request_id='x', prompt='never', delivery='later')['error'],
                         'invalid_delivery')
        second = client.request('submit', bot='Bob', request_id='2', prompt='second', delivery='queue')['result']
        third = client.request('submit', bot='Bob', request_id='3', prompt='third', delivery='queue')['result']
        self.assertEqual((second['status'], third['status']), ('queued', 'queued'))
        self.assertEqual(client.request('result', bot='Bob', turn=second['turn'])['result'],
                         {'turn': second['turn'], 'status': 'queued', 'finished': False})
        retry = client.request('submit', bot='Bob', request_id='2', prompt='second', delivery='queue')['result']
        self.assertEqual((retry['turn'], retry['duplicate'], retry['status']), (second['turn'], True, 'queued'))
        self.assertEqual(client.request('stats')['result']['queued_turns'], 2)
        for turn in (first['turn'], second['turn'], third['turn']):
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        kinds = [(e['turn'], e['event']) for e in self.events(client, 'Bob')
                 if e['event'] in ('accepted', 'queued', 'turn_finished')]
        self.assertEqual(kinds, [
            (first['turn'], 'accepted'), (second['turn'], 'queued'), (third['turn'], 'queued'),
            (first['turn'], 'turn_finished'), (second['turn'], 'accepted'), (second['turn'], 'turn_finished'),
            (third['turn'], 'accepted'), (third['turn'], 'turn_finished')])
        listed = client.request('turns', bot='Bob', after=0)['result']['turns']
        self.assertEqual([t['delivery'] for t in listed], ['reject', 'queue', 'queue'])
        self.assertEqual(client.request('result', bot='Bob', turn=third['turn'])['result']['text'], 'reply:third')

    def test_steer_joins_the_running_turn_at_its_next_round_boundary(self):
        client = self.client('echo,shell')
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='shell:sleep .4')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == first)
        steer = client.request('submit', bot='Bob', request_id='s', prompt='from-steer', delivery='steer')['result']
        self.assertEqual(steer['status'], 'queued')
        client.process.stdin.write(json.dumps({'id': 'w', 'op': 'wait', 'handles': [steer['handle']]}) + '\n')
        client.process.stdin.flush()
        done = client.finished(steer['turn'])['data']
        self.assertEqual((done['status'], done['into']), ('steered', first))
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        answered = client.receive(lambda m: m.get('id') == 'w')['result']['results'][steer['handle']]
        self.assertEqual((answered['status'], answered['into']), ('steered', first))
        # The model saw the steer after the shell result, and answered it.
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        tail = requests[-1]['input'][-2:]
        self.assertEqual(tail[0]['type'], 'function_call_output')
        self.assertEqual(tail[1]['content'][0]['text'], 'from-steer')
        self.assertEqual(client.request('result', bot='Bob', turn=first)['result']['text'], 'reply:from-steer')
        self.assertIn('steered', [e['event'] for e in self.events(client, 'Bob') if e['turn'] == first])
        # On an idle bot a steer is an ordinary turn.
        idle = client.request('submit', bot='Bob', request_id='s2', prompt='alone', delivery='steer')['result']
        self.assertEqual(idle['status'], 'running')
        self.assertEqual(client.finished(idle['turn'])['data']['status'], 'completed')

    def test_a_steer_that_misses_the_last_boundary_becomes_the_next_turn(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='slow')['result']['turn']
        steer = client.request('submit', bot='Bob', request_id='s', prompt='late', delivery='steer')['result']['turn']
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        # The final call was already in flight: the steer keeps the turn going
        # for one more round, or starts its own turn. Either way it is heard.
        outcome = client.finished(steer)['data']
        self.assertIn(outcome['status'], ('steered', 'completed'))
        listed = {t['turn']: t for t in client.request('turns', bot='Bob', after=0)['result']['turns']}
        self.assertEqual(listed[steer]['delivery'], 'steer')

    def test_ready_turns_start_when_a_slot_opens_and_queued_ones_can_be_interrupted(self):
        client = self.client(extra=('--max-active', '1'))
        for bot in ('Alice', 'Bob'):
            client.request('create', bot=bot, workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='slow')['result']['turn']
        self.assertEqual(client.request('submit', bot='Bob', request_id='x', prompt='never')['error'], 'active_agent_limit')
        bob = client.request('submit', bot='Bob', request_id='b', prompt='ready-work', delivery='queue')['result']
        self.assertEqual(bob['status'], 'ready')
        skipped = client.request('submit', bot='Bob', request_id='c', prompt='skipped', delivery='queue')['result']
        after = client.request('submit', bot='Bob', request_id='d', prompt='after', delivery='queue')['result']
        self.assertEqual((skipped['status'], after['status']), ('queued', 'queued'))
        self.assertEqual(client.request('delete', bot='Bob')['error'], 'bot_busy')
        cancelled = client.request('interrupt', bot='Bob', turn=skipped['turn'])['result']
        self.assertEqual(cancelled, {'interrupt_requested': True, 'turn': skipped['turn'], 'queued': True})
        self.assertEqual(client.finished(skipped['turn'])['data']['status'], 'interrupted')
        self.assertEqual(client.request('interrupt', bot='Bob', turn=skipped['turn'])['error'], 'no_active_turn')
        self.assertEqual(client.finished(alice)['data']['status'], 'completed')
        self.assertEqual(client.finished(bob['turn'])['data']['status'], 'completed')
        self.assertEqual(client.finished(after['turn'])['data']['status'], 'completed')
        self.assertEqual(client.request('stats')['result']['queued_turns'], 0)
        self.assertEqual(client.request('delete', bot='Bob')['result']['turns'], 3)

    def test_restart_rejects_queued_provider_changes_before_history_mutation(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        client.request('submit', bot='Bob', request_id='1', prompt='wait')
        default = client.request('submit', bot='Bob', request_id='2', prompt='default',
                                 delivery='queue')['result']['turn']
        override = client.request('submit', bot='Bob', request_id='3', prompt='override',
                                  delivery='queue', model='openai/synthetic-model')['result']['turn']
        head = client.request('resume', bot='Bob')['result']['head']
        client.close(kill=True)
        client = Client(self.binary, self.path / 'state.sqlite', self.url, family='anthropic')
        self.addCleanup(client.close)
        for turn in (default, override):
            self.assertEqual(client.finished(turn)['data']['error'], 'provider_family_mismatch')
        self.assertEqual(client.request('resume', bot='Bob')['result']['head'], head)
        events = self.events(client, 'Bob')
        self.assertFalse(any(e['event'] == 'accepted' and e['turn'] in (default, override) for e in events))
        # An accepted request remains reconcilable even if its provider changed.
        duplicate = client.request('submit', bot='Bob', request_id='3', prompt='override',
                                   delivery='queue', model='openai/synthetic-model')['result']
        self.assertTrue(duplicate['duplicate'])
        self.assertEqual(duplicate['turn'], override)

    def test_queued_turns_survive_a_restart(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='wait')['result']['turn']
        second = client.request('submit', bot='Bob', request_id='2', prompt='second', delivery='queue')['result']['turn']
        third = client.request('submit', bot='Bob', request_id='3', prompt='third', delivery='steer')['result']['turn']
        client.close(kill=True)
        client = self.client()
        # Recovery ends the interrupted turn and the line moves at once.
        self.assertEqual(client.request('result', bot='Bob', turn=first)['result']['status'], 'interrupted')
        self.assertEqual(client.finished(second)['data']['status'], 'completed')
        outcome = client.finished(third)['data']
        # The steer joins the next turn at its first boundary, or runs alone.
        text = client.request('result', bot='Bob', turn=second)['result']['text']
        if outcome['status'] == 'steered':
            self.assertEqual((outcome['into'], text), (second, 'reply:third'))
        else:
            self.assertEqual((outcome['status'], text), ('completed', 'reply:second'))


if __name__ == '__main__':
    unittest.main()
