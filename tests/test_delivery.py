"""Delivery modes on submit: reject, queue, and steer."""
import json
import os
import queue
import threading
import time
import unittest

from tests.test_runtime import ModelFixture
from bench.runtime_client import Client


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class DeliveryTests(ModelFixture):
    def tool_output(self, client, bot, call_id):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        node = [e for e in events if e['event'] == 'tool_completed' and e['data']['call_id'] == call_id][-1]['data']['node']
        return client.request('item', bot=bot, node=node)['result']['output']

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
        # A completion never prunes its own records: the last cancellation's
        # terminal event is published and replayable until the next pass,
        # alongside the one retention keeps.
        self.assertEqual(client.request('result', bot='Bob', turn=queued[0])['result']['status'], 'interrupted')
        self.assertEqual(client.request('result', bot='Bob', turn=queued[1])['error'], 'turn_result_pruned')
        terminal = [e for e in self.events(client, 'Bob') if e['event'] == 'turn_finished']
        self.assertEqual([e['turn'] for e in terminal], [queued[-1], queued[0]])
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

    def test_cancelling_a_blocker_rechecks_pending_steers_at_the_next_boundary(self):
        elsewhere = self.path / 'elsewhere'
        elsewhere.mkdir()
        client = self.client(extra=('--provider', f'other=responses,{self.url}'))
        for bot, override in [('Workspace', {'workspace': str(elsewhere)}),
                              ('Model', {'model': 'other/synthetic-model'})]:
            with self.subTest(override=bot):
                first_gate, final_gate = threading.Event(), threading.Event()
                self.addCleanup(first_gate.set)
                self.addCleanup(final_gate.set)
                self.model.request_gates = queue.Queue()
                self.model.request_gates.put(first_gate)
                self.model.request_gates.put(final_gate)
                client.request('create', bot=bot, workspace=str(self.path))
                turn = client.request('submit', bot=bot, request_id='active', prompt='tool:hello')['result']['turn']
                self.model.requests.get(timeout=3)
                blocker = client.request('submit', bot=bot, request_id='blocker', prompt='not here',
                                         delivery='steer', **override)['result']['turn']
                correction = client.request('submit', bot=bot, request_id='correction', prompt='corrected',
                                            delivery='steer', expected_turn=turn)['result']['turn']
                first_gate.set()
                # The post-tool boundary encountered the incompatible head.
                # The final response stays held until that blocker is gone.
                second = self.model.requests.get(timeout=3)
                self.assertEqual(second['input'][-1]['type'], 'function_call_output')
                self.assertTrue(client.request('interrupt', bot=bot, turn=blocker)['result']['queued'])
                final_gate.set()
                outcome = client.finished(correction)['data']
                self.assertEqual((outcome['status'], outcome.get('into')), ('steered', turn))
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                third = self.model.requests.get(timeout=3)
                self.assertEqual(third['input'][-1]['content'][0]['text'], 'corrected')
                self.assertEqual(client.request('result', bot=bot, turn=turn)['result']['text'], 'reply:corrected')

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

    def test_pending_bounds_refuse_waiting_work_and_report_in_stats(self):
        client = self.client(extra=('--max-pending', '2', '--max-pending-bytes', '64'))
        for bot in ('Bob', 'Ann'):
            client.request('create', bot=bot, workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='slow')['result']['turn']
        for n in ('2', '3'):
            self.assertEqual(client.request('submit', bot='Bob', request_id=n, prompt='x' * 20,
                                            delivery='queue')['result']['status'], 'queued')
        refused = client.request('submit', bot='Bob', request_id='4', prompt='x', delivery='queue')

        self.assertEqual(refused['error'], 'pending_limit')
        self.assertIn('bound is 2', refused['detail'])
        stats = client.request('stats')['result']
        self.assertEqual((stats['queued_turns'], stats['pending_bytes'], stats['pending_limit'],
                          stats['pending_bytes_limit']), (2, 40, 2, 64))
        # Work that starts at once never counts; a steer that would wait does.
        self.assertEqual(client.request('submit', bot='Ann', request_id='a', prompt='hi')['result']['status'], 'running')
        self.assertEqual(client.request('submit', bot='Bob', request_id='s', prompt='steer', delivery='steer')['error'],
                         'pending_limit')
        for turn in (first,):
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        # Once the line drains, the bytes bound is the next thing in the way.
        client.request('wait', handles=[f'turn:Bob/{first + 1}', f'turn:Bob/{first + 2}'], timeout_ms=10000)
        self.assertEqual(client.request('stats')['result']['queued_turns'], 0)
        blocker = client.request('submit', bot='Bob', request_id='5', prompt='slow')['result']['turn']
        big = client.request('submit', bot='Bob', request_id='6', prompt='y' * 65, delivery='queue')
        self.assertEqual(big['error'], 'pending_limit')
        self.assertIn('bound of 64', big['detail'])
        self.assertEqual(client.request('submit', bot='Bob', request_id='6', prompt='y' * 64,
                                        delivery='queue')['result']['status'], 'queued')
        client.finished(blocker)

    def test_queued_turns_run_in_order_after_the_busy_turn(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='slow')['result']
        self.assertEqual(first['status'], 'running')
        refused = client.request('submit', bot='Bob', request_id='x', prompt='never')
        self.assertEqual(refused['error'], 'bot_busy')
        # The refusal names the running turn and every way past it. A first
        # turn leaves no settled point to fork from, so none is offered.
        self.assertIn(f"turn {first['turn']} is running", refused['detail'])
        for way in (f"--delivery steer --turn {first['turn']}", '--delivery queue'):
            self.assertIn(way, refused['detail'])
        self.assertNotIn('fork', refused['detail'])
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

    def test_compaction_fires_at_the_threshold_and_replaces_older_turns_with_a_summary(self):
        client = self.client('echo,history', extra=('--context-bytes', '4096', '--compact-at', '50', '--compact-keep', '25'))
        client.request('create', bot='Bob', workspace=str(self.path), compaction_instructions='Summarize the conversation.')
        client.request('create', bot='Plain', workspace=str(self.path))
        prompts = [f'Task {n}: ' + f'{n}' * 500 for n in range(1, 7)]
        turns = {}
        for bot in ('Bob', 'Plain'):
            for n, prompt in enumerate(prompts):
                turn = client.request('submit', bot=bot, request_id=str(n), prompt=prompt)['result']['turn']
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                turns[(bot, n)] = turn
        compacted = [e for e in self.events(client, 'Bob') if e['event'] == 'compacted']
        self.assertGreaterEqual(len(compacted), 1)
        first = compacted[0]['data']
        self.assertEqual(first['covered_turns'][0], 1)
        self.assertGreater(first['bytes'], 0)
        self.assertEqual([e for e in self.events(client, 'Plain') if e['event'] == 'compacted'], [])
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        # The summarizer's own call: the client's instructions, no tools, the
        # span's items, and the request to write, whose echo became the summary.
        summarizer = [r for r in requests if r.get('instructions') == 'Summarize the conversation.']
        self.assertGreaterEqual(len(summarizer), 1)
        self.assertEqual(summarizer[0]['tools'], [])
        self.assertTrue(summarizer[0]['input'][-1]['content'][0]['text'].startswith('[compaction request]'))
        # Bob's later requests carry the summary and the covered prompts verbatim, ahead of the window.
        later = [r for r in requests if r.get('instructions') != 'Summarize the conversation.'
                 and any(i.get('role') == 'user' and i['content'][0]['text'].startswith('[compaction summary')
                         for i in r['input'])]
        self.assertGreaterEqual(len(later), 1)
        text = [i for i in later[-1]['input'] if i.get('role') == 'user'
                and i['content'][0]['text'].startswith('[compaction summary')][0]['content'][0]['text']
        self.assertIn('reply:[compaction request]', text)
        self.assertIn('User messages from those turns, verbatim:', text)
        self.assertIn('\n1: Task 1: 111', text)
        bob = client.request('resume', bot='Bob')['result']
        # Later compactions stand for everything since the first: the newest
        # version is the bot's, and its coverage still starts at turn 1.
        self.assertEqual(bob['compaction'], compacted[-1]['data']['version'])
        self.assertEqual(compacted[-1]['data']['covered_turns'][0], 1)
        self.assertEqual(bob['compaction_instructions'], 'Summarize the conversation.')
        # The transcript is intact: history reads a covered turn; a fork before
        # the compaction has none; a fork at the head inherits it.
        read = client.request('submit', bot='Bob', request_id='h', prompt='history:1')['result']['turn']
        self.assertEqual(client.finished(read)['data']['status'], 'completed')
        self.assertIn('Task 1: 111', self.tool_output(client, 'Bob', 'history-1'))
        page = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        early = [e['data']['checkpoint'] for e in page if e['event'] == 'turn_finished' and e['turn'] == turns[('Bob', 0)]][0]
        client.request('fork', source='Bob', checkpoint=early, bot='Early', workspace=str(self.path))
        self.assertIsNone(client.request('resume', bot='Early')['result']['compaction'])
        client.request('fork', source='Bob', bot='Late', workspace=str(self.path))
        self.assertEqual(client.request('resume', bot='Late')['result']['compaction'],
                         client.request('resume', bot='Bob')['result']['compaction'])
        # A summarizer of another family is refused at creation; an unknown one too.
        self.assertEqual(client.request('create', bot='Bad', workspace=str(self.path),
                                        compaction_instructions='x', compaction_model='nobody/model')['error'],
                         'provider_unavailable')

    def test_carry_forward_note_is_pinned_versioned_and_inherited_by_forks(self):
        client = self.client('echo,note')
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='note:Rule: end every file with the marker.')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        completed = [e for e in self.events(client, 'Bob') if e['event'] == 'tool_completed'][-1]
        version = completed['data']['note']
        self.assertEqual(version, completed['data']['node'])
        self.assertEqual(client.request('resume', bot='Bob')['result']['note'], version)
        self.assertEqual(json.loads(client.request('item', bot='Bob', node=version)['result']['output']),
                         {'bytes': 37, 'cleared': False})
        # The next request carries the note ahead of the window, before the prompt.
        second = client.request('submit', bot='Bob', request_id='2', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(second)['data']['status'], 'completed')
        while not self.model.requests.empty():
            request = self.model.requests.get()
        texts = [item['content'][0]['text'] for item in request['input'] if item.get('role') == 'user']
        self.assertEqual(texts[0], f'[carry-forward note, version {version}]\nRule: end every file with the marker.')
        self.assertEqual(texts[-1], 'hello')
        checkpoint = client.request('resume', bot='Bob')['result']['head']
        # A rewrite is a new version; a fork at the earlier checkpoint keeps the old one.
        third = client.request('submit', bot='Bob', request_id='3', prompt='note:Rule v2.')['result']['turn']
        client.finished(third)
        later = client.request('resume', bot='Bob')['result']['note']
        self.assertGreater(later, version)
        client.request('fork', source='Bob', checkpoint=checkpoint, bot='Early', workspace=str(self.path))
        self.assertEqual(client.request('resume', bot='Early')['result']['note'], version)
        # Empty text clears it: the next request has no pinned note.
        cleared = client.request('submit', bot='Bob', request_id='4', prompt='note:')['result']['turn']
        client.finished(cleared)
        after = client.request('submit', bot='Bob', request_id='5', prompt='again')['result']['turn']
        client.finished(after)
        while not self.model.requests.empty():
            request = self.model.requests.get()
        texts = [item['content'][0]['text'] for item in request['input'] if item.get('role') == 'user']
        self.assertFalse(any(t.startswith('[carry-forward note') for t in texts))
        # A bot without the tool cannot call it.
        client.request('create', bot='Plain', workspace=str(self.path), tools=['echo'])
        refused = client.request('submit', bot='Plain', request_id='1', prompt='note:x')['result']['turn']
        self.assertEqual(client.finished(refused)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.tool_output(client, 'Plain', 'note-1'))['error'], 'tool_not_available')

    def test_a_steer_is_measured_against_a_note_written_in_the_same_round(self):
        # The steer arrives while the model is asked; the model's answer
        # writes a large carry-forward note. Beside that note the steer does
        # not fit, though it would have beside the request that was sent.
        client = self.client('echo,note', extra=('--context-bytes', '8192'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['echo', 'note'])
        self.model.note_text = 'N' * 3500
        gate = threading.Event()
        self.model.request_gates = queue.Queue()
        self.model.request_gates.put(gate)
        turn = client.request('submit', bot='Bob', request_id='1', prompt='note:')['result']['turn']
        self.model.requests.get(timeout=5)
        steer = client.request('submit', bot='Bob', request_id='s', prompt='steer:' + 'x' * 2000,
                               delivery='steer', expected_turn=turn)['result']['turn']
        gate.set()
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        self.assertEqual(client.finished(steer)['data']['error'], 'stale_turn')
        while not self.model.requests.empty():
            request = self.model.requests.get()
            self.assertLessEqual(len(json.dumps(request['input'], separators=(',', ':')).encode()) - 2, 8192)

    def test_a_steer_with_no_room_beside_a_note_goes_in_once_the_note_is_cleared(self):
        # The steer arrives while the model is asked; beside the bot's large
        # note it does not fit at the next boundary. The model then clears
        # the note, and the steer goes in at the boundary after that,
        # before the answer, rather than failing when the turn ends.
        client = self.client('echo,note', extra=('--context-bytes', '8192'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['echo', 'note'])
        self.model.note_text = 'N' * 3500
        noted = client.request('submit', bot='Bob', request_id='1', prompt='note:')['result']['turn']
        self.assertEqual(client.finished(noted)['data']['status'], 'completed')
        del self.model.note_text
        while not self.model.requests.empty():
            self.model.requests.get()
        self.model.call_script = [('echo', {'text': 'a'}), ('note', {'text': ''})]
        gate = threading.Event()
        self.model.request_gates = queue.Queue()
        self.model.request_gates.put(gate)
        turn = client.request('submit', bot='Bob', request_id='2', prompt='script')['result']['turn']
        self.model.requests.get(timeout=5)
        correction = 'steer:' + 'x' * 2500
        steer = client.request('submit', bot='Bob', request_id='s', prompt=correction,
                               delivery='steer', expected_turn=turn)['result']['turn']
        gate.set()
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        outcome = client.finished(steer)['data']
        self.assertEqual((outcome['status'], outcome.get('into')), ('steered', turn), outcome)
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        carries = [any(i.get('role') == 'user' and i['content'][0]['text'] == correction for i in r['input'])
                   for r in requests]
        # Asked after the echo without it, after the note is cleared with it.
        self.assertEqual(carries, [False, True])

    def test_a_steer_is_admitted_beside_the_context_note_without_its_previews(self):
        # A long history leaves most turns out of view, and the context note
        # lists how each began. Those previews yield to the running turn, so
        # a strict steer that fits beside the note without them goes in.
        client = self.client('echo', extra=('--context-bytes', '16384'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['echo'])
        for n in range(50):
            turn = client.request('submit', bot='Bob', request_id=str(n),
                                  prompt=f'Task {n}: ' + 'p' * 300)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        while not self.model.requests.empty():
            self.model.requests.get()
        self.model.call_script = [('echo', {'text': 'a'})]
        gate = threading.Event()
        self.model.request_gates = queue.Queue()
        self.model.request_gates.put(gate)
        turn = client.request('submit', bot='Bob', request_id='run', prompt='script')['result']['turn']
        first = self.model.requests.get(timeout=5)
        note = first['input'][0]['content'][0]['text']
        self.assertIn('How they began', note)
        self.assertGreater(len(note), 4000)
        correction = 'steer:' + 'x' * 9000
        steer = client.request('submit', bot='Bob', request_id='s', prompt=correction,
                               delivery='steer', expected_turn=turn)['result']['turn']
        gate.set()
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        outcome = client.finished(steer)['data']
        self.assertEqual((outcome['status'], outcome.get('into')), ('steered', turn), outcome)
        while not self.model.requests.empty():
            request = self.model.requests.get()
            self.assertLessEqual(len(json.dumps(request['input'], separators=(',', ':')).encode()) - 2, 16384)

    def test_context_note_lists_how_omitted_turns_began(self):
        prompts = [f'Task {n}: ' + f'{n}' * 700 for n in range(1, 8)]
        # The note names the history tool only to a bot that has it.
        for note_turns, listing, tools in (('3', True, ['echo', 'history']), ('0', False, ['echo'])):
            with self.subTest(note_turns=note_turns):
                client = self.client(extra=('--context-bytes', '2048', '--note-turns', note_turns))
                client.request('create', bot=f'Bob{note_turns}', workspace=str(self.path), tools=tools)
                for n, prompt in enumerate(prompts):
                    turn = client.request('submit', bot=f'Bob{note_turns}', request_id=str(n), prompt=prompt)['result']['turn']
                    self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                last = None
                while not self.model.requests.empty():
                    last = self.model.requests.get()
                note = last['input'][0]['content'][0]['text']
                self.assertTrue(note.startswith('[context note] '))
                self.assertEqual('Use the history tool' in note, 'history' in tools)
                if listing:
                    lines = note.split('\n')
                    self.assertTrue(lines[0].endswith('How they began, newest first:'))
                    # The three newest omitted turns, each cut to its first 120 bytes.
                    listed = [line.split(': ', 1) for line in lines[1:4]]
                    ordinals = [int(o) for o, _ in listed]
                    self.assertEqual(ordinals, sorted(ordinals, reverse=True))
                    for ordinal, opening in listed:
                        self.assertTrue(opening.startswith(f'Task {ordinal}: {ordinal * int(ordinal)}'[:20]), opening)
                        self.assertTrue(opening.endswith('…'))
                    self.assertTrue(lines[4].startswith(f'Turns 1 to {ordinals[-1] - 1} are older'))
                else:
                    self.assertNotIn('\n', note)
                client.request('shutdown')
                client.close()

    def test_steers_beyond_the_context_budget_stay_queued_and_run_as_their_own_turns(self):
        # 4 KiB of context keeps 3 KiB for the running turn; one 1.5 KiB steer fits at
        # its boundary, the other two would have pushed the turn past its budget.
        client = self.client('echo,shell', extra=('--context-bytes', '4096'))
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='shell:sleep .4')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == first)
        steers = [client.request('submit', bot='Bob', request_id=f's{n}', prompt=f'{n}' * 1500,
                                 delivery='steer')['result']['turn'] for n in range(3)]
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        outcomes = [client.finished(turn)['data'] for turn in steers]
        self.assertEqual([o['status'] for o in outcomes], ['steered', 'completed', 'completed'])
        self.assertEqual(outcomes[0]['into'], first)
        self.assertEqual(client.request('result', bot='Bob', turn=steers[2])['result']['text'], 'reply:' + '2' * 1500)

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


    def test_durable_events_arrive_in_commit_order_exactly_once(self):
        client = self.client('echo,shell')
        bots = [f'bot-{n}' for n in range(8)]
        for bot in bots:
            client.request('create', bot=bot, workspace=str(self.path))
        handles = []
        for bot in bots:
            # Two-round turns, with queued work and steers landing on running bots.
            handles.append(client.request('submit', bot=bot, request_id='a', prompt='shell:sleep .2')['result']['handle'])
            handles.append(client.request('submit', bot=bot, request_id='b', prompt='queued', delivery='queue')['result']['handle'])
            handles.append(client.request('submit', bot=bot, request_id='c', prompt='nudge', delivery='steer')['result']['handle'])
        done = client.request('wait', handles=handles, timeout_ms=20000)['result']
        self.assertEqual(done['pending'], [])
        live = [m['cursor'] for m in client.saved if m.get('cursor') is not None]
        self.assertEqual(live, sorted(set(live)), 'live cursors rise strictly, no duplicates')
        replay = []
        for bot in bots:
            page = client.request('events', bot=bot, after=0, limit=256)['result']
            self.assertLess(len(page['events']), 256)
            replay.extend(e['cursor'] for e in page['events'])
        self.assertEqual(live, sorted(replay), 'live delivery is the replay, complete')
        # Wait answers followed the terminal events they report.
        finished = {m['turn'] for m in client.saved if m.get('event') == 'turn_finished'}
        self.assertEqual(len(finished), len(handles))

    def test_strict_steers_name_their_turn(self):
        client = self.client('echo,shell')
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='shell:sleep .4')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == first)
        wrong = client.request('submit', bot='Bob', request_id='w', prompt='no', delivery='steer', expected_turn=first + 99)
        self.assertEqual(wrong['error'], 'stale_turn')
        mode = client.request('submit', bot='Bob', request_id='m', prompt='no', delivery='queue', expected_turn=first)
        self.assertEqual(mode['error'], 'invalid_delivery')
        right = client.request('submit', bot='Bob', request_id='r', prompt='correction', delivery='steer',
                               expected_turn=first)['result']
        self.assertEqual(right['status'], 'queued')
        self.assertEqual(client.finished(right['turn'])['data']['into'], first)
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        # Nothing was written for the refused ones, and an idle bot has no turn to steer.
        self.assertEqual([t['request_id'] for t in client.request('turns', bot='Bob', after=0)['result']['turns']], ['1', 'r'])
        idle = client.request('submit', bot='Bob', request_id='i', prompt='late', delivery='steer', expected_turn=first)
        self.assertEqual(idle['error'], 'stale_turn')


if __name__ == '__main__':
    unittest.main()
