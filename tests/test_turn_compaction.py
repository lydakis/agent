"""Compaction inside a running turn: one long task summarizes its own
earlier rounds and finishes, its prompt kept whole."""
from contextlib import closing
import json
import os
import sqlite3
import threading
import time
from unittest import skipUnless
from tests.test_runtime import AnthropicModel, ModelFixture, is_summary
from tests.test_elision import drain, encoded
from bench.runtime_client import Client, node_item
from bench.targets import clean_env


def all_events(client, bot):
    events, after = [], 0
    while True:
        page = client.request('events', bot=bot, after=after, limit=256)['result']['events']
        if not page:
            return events
        events += page
        after = page[-1]['cursor']


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class TurnCompactionTests(ModelFixture):
    def test_one_long_turn_crosses_several_compactions_and_finishes(self):
        # Forty rounds in 24 KiB: even as stubs, the turn's own calls and
        # stubs outgrow the budget several times over.
        client = self.client(tools='shell,read', settings={'context_bytes': 24576})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:40')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = node_item(client, 'Bob', ended['data']['checkpoint'])['result']
        self.assertIn('done after 40 rounds', json.dumps(answer))
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in requests))
        work = [r for r in requests if not is_summary(r)]
        self.assertGreaterEqual(len(requests) - len(work), 3)
        # Each round ran once: no work was repeated after a cut.
        events = all_events(client, 'Bob')
        ran = [e['data']['call_id'] for e in events if e['event'] == 'tool_completed']
        self.assertEqual(ran[:40], [f'long-{n}' for n in range(40)])
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertGreaterEqual(len(compacted), 3)
        self.assertFalse([e for e in events if e['event'] == 'compaction_failed'])
        # Every cut is inside the turn and keeps its prompt, which each
        # later request carries whole after the summary of the turn's start.
        prompt = compacted[0]['pinned']
        self.assertTrue(all(c['pinned'] == prompt and c['covered_turns'] == [1, 1] for c in compacted))
        users = [i for i in work[-1]['input'] if i.get('role') == 'user']
        self.assertIn('covering the start of turn 1]', users[0]['content'][0]['text'])
        self.assertEqual(users[-1]['content'][0]['text'], 'long:40')
        # Calls and results stay paired in every request.
        for request in work:
            items = request['input']
            asked = [i['call_id'] for i in items if i.get('type') == 'function_call']
            answered = [i['call_id'] for i in items if i.get('type') == 'function_call_output']
            self.assertEqual(asked[:len(answered)], answered)
            self.assertLessEqual(len(asked) - len(answered), 1)

    def test_a_summary_inside_a_turn_runs_beside_its_calls(self):
        # Each summary takes a second. Inside the turn, the calls go on
        # while it runs, sending the view as it is, and a later boundary
        # installs it.
        self.model.timeline, self.model.summary_delay = [], 1.0
        client = self.client(tools='shell', settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:150x40')['result']['turn']
        ended = client.finished(turn, timeout=60)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = node_item(client, 'Bob', ended['data']['checkpoint'])['result']
        self.assertIn('done after 150 rounds', json.dumps(answer))
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 65536 for r in requests))
        summaries = [(start, end) for summary, start, end in self.model.timeline if summary]
        calls = [start for summary, start, _ in self.model.timeline if not summary]
        self.assertGreaterEqual(len(summaries), 2)
        for start, end in summaries:
            self.assertGreaterEqual(sum(start < call < end for call in calls), 2)
        events = all_events(client, 'Bob')
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertEqual(len(compacted), len(summaries))
        self.assertTrue(all(c['request']['beside'] for c in compacted))
        self.assertFalse([e for e in events if e['event'] == 'compaction_failed'])
        # Each round ran once, and the turn's calls and results stay paired.
        ran = [e['data']['call_id'] for e in events if e['event'] == 'tool_completed']
        self.assertEqual(ran, [f'long-{n}' for n in range(150)])
        for request in requests:
            if is_summary(request):
                continue
            items = request['input']
            asked = [i['call_id'] for i in items if i.get('type') == 'function_call']
            answered = [i['call_id'] for i in items if i.get('type') == 'function_call_output']
            self.assertEqual(asked[:len(answered)], answered)

    def test_an_interrupt_still_installs_a_summary_that_landed_beside_a_call(self):
        # The summary lands while the call sent beside it is still held;
        # the interrupt then drops the turn's rounds before a boundary.
        self.model.timeline, self.model.summary_delay = [], 0.5
        self.model.hold_after_summary = threading.Event()
        self.addCleanup(self.model.hold_after_summary.set)
        client = self.client(tools='shell', settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:150x40')['result']['turn']
        deadline = time.monotonic() + 30
        while not any(summary for summary, *_ in self.model.timeline):
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.05)
        time.sleep(.5)
        client.request('interrupt', bot='Bob', turn=turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        self.model.hold_after_summary.set()
        events = all_events(client, 'Bob')
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertEqual(len(compacted), 1)
        self.assertTrue(compacted[0]['request']['beside'])
        billed = [e['data'] for e in events if e['event'] == 'usage' and e['data'].get('purpose') == 'compaction']
        self.assertEqual(len(billed), 1)

    def test_a_summary_beside_the_turn_holds_its_calls_to_the_round_limit(self):
        # Small rounds: compaction comes due past round 150, and the view
        # would not outgrow the limit until well past 200, so the summary
        # is still running as the turn nears its round limit. The turn
        # waits for it there, and fails with it counted among its rounds.
        self.model.timeline, self.model.summary_delay = [], 5.0
        client = self.client(tools='shell', settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:250x1')['result']['turn']
        ended = client.finished(turn, timeout=90)
        self.assertEqual((ended['data']['status'], ended['data']['error']), ('failed', 'tool_round_limit'))
        requests = drain(self.model)
        self.assertEqual(len(requests), 200)
        self.assertEqual(sum(map(is_summary, requests)), 1)

    def test_a_round_that_overflows_before_compaction_is_due_forces_a_summary(self):
        # Four small rounds, then one that takes the turn past its budget
        # before compaction is due. Without read nothing is elided, so the
        # runtime summarizes the earlier rounds, keeping the prompt, and the
        # turn goes on; with no summarizer instructions it fails.
        client = self.client(tools='shell', settings={'context_bytes': 24576, 'compact_at': 99})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:4x250,1x600')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = node_item(client, 'Bob', ended['data']['checkpoint'])['result']
        self.assertIn('done after 5 rounds', json.dumps(answer))
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in requests))
        # One summary, made after the round that overflowed.
        kinds = ['summary' if is_summary(r) else 'work' for r in requests]
        self.assertEqual(kinds, ['work'] * 5 + ['summary', 'work'])
        events = all_events(client, 'Bob')
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertEqual(len(compacted), 1)
        self.assertTrue(compacted[0]['pinned'])
        self.assertEqual(compacted[0]['covered_turns'], [1, 1])
        users = [i['content'][0]['text'] for i in requests[-1]['input'] if i.get('role') == 'user']
        self.assertIn('covering the start of turn 1]', users[0])
        self.assertEqual(users[-1], 'long:4x250,1x600')
        calls = [i['call_id'] for i in requests[-1]['input'] if i.get('type') == 'function_call']
        self.assertEqual(calls, ['long-4'])

        client.request('create', bot='Ann', workspace=str(self.path), tools=['shell'])
        turn = client.request('submit', bot='Ann', request_id='1', prompt='long:4x250,1x600')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'failed', ended)
        self.assertEqual(ended['data']['error'], 'context_limit', ended)

    def test_a_turn_resumed_under_a_smaller_budget_takes_several_summary_steps(self):
        # Twelve rounds of about 3 KiB fit 64 KiB; the call after them is
        # paced, and the daemon restarts with Bob's stored budget cut to
        # 12 KiB (settings are fixed at creation, so the test edits the
        # store). The resumed turn cannot fit, and one summarizer budget
        # covers only a few of its rounds: the steps go on at that head,
        # extending one version, until the view fits.
        client = self.client(tools='shell', settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        self.model.pace_at = 12
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:12x150,2x1')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn, timeout=30)
        client.close(kill=True)
        drain(self.model)
        with closing(sqlite3.connect(self.path / 'state.sqlite')) as db, db:
            db.execute('''UPDATE bots SET settings='{"context_bytes":12288}' WHERE name='Bob' ''')
        client = Client(self.binary, self.path / 'state.sqlite', self.url, tools='shell')
        self.addCleanup(client.close)
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 12288 for r in requests))
        compacted = [e['data'] for e in all_events(client, 'Bob') if e['event'] == 'compacted']
        steps = [c for c in compacted if c['catch_up']]
        self.assertGreater(len(steps), 1)
        self.assertEqual(len({c['version'] for c in steps}), 1)
        self.assertEqual([c['cut'] for c in steps], sorted({c['cut'] for c in steps}))
        self.assertEqual(sum(is_summary(r) for r in requests), len(compacted))


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class AnthropicTurnCompactionTests(ModelFixture):
    handler = AnthropicModel

    def test_thinking_stays_bound_across_cuts_inside_the_turn(self):
        self.model.bind_thinking = True
        self.model.binding_errors = []
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell,read', provider='anthropic', family='anthropic',
                        model='synthetic-claude', key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'},
                        settings={'context_bytes': 24576})
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path), reasoning='low',
                                               compaction_instructions='Summarize.'))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:30')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = node_item(client, 'Bob', ended['data']['checkpoint'])['result']
        self.assertIn('done after 30 rounds', json.dumps(answer))
        self.assertEqual(self.model.binding_errors, [])
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['messages']) <= 24576 for r in requests))
        summaries = [r for r in requests if is_summary(r)]
        self.assertGreaterEqual(len(summaries), 2)
        compacted = [e['data'] for e in all_events(client, 'Bob') if e['event'] == 'compacted']
        self.assertTrue(compacted and all(c['pinned'] for c in compacted))
