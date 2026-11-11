"""Compaction inside a running turn: one long task summarizes its own
earlier rounds and finishes, its prompt kept whole."""
from contextlib import closing
import json
import os
import re
import sqlite3
import threading
import time
from unittest import skipUnless
from tests.test_runtime import AnthropicModel, ModelFixture, is_summary
from tests.test_elision import drain, encoded
from bench.runtime_client import Client, node_item
from bench.targets import clean_env


def asked_bytes(request):
    """The size a summary request asks for."""
    items = request.get('input') or request.get('messages')
    return int(re.search(r'within ([0-9]+) UTF-8 bytes', items[-1]['content'][-1]['text']).group(1))


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
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:40')['result']['message']
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

    def test_a_message_sent_to_the_turn_stays_whole_after_its_prompt_once_summarized(self):
        # A correction steered into a long turn is covered by the cuts that
        # follow. Each later request carries it verbatim right after the
        # prompt, so the prompt never reads as the newer instruction, and
        # the summary does not stand in for it.
        client = self.client(tools='shell,read', settings={'context_bytes': 24576})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:40')['result']['message']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn
                       and m.get('data', {}).get('call_id') == 'long-3')
        correction = 'steer: round half to even instead'
        steer = client.request('submit', bot='Bob', request_id='s', prompt=correction,
                               delivery='steer', expected_turn=turn)['result']['message']
        ended = client.finished(turn, timeout=30)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        self.assertEqual(client.finished(steer)['data']['status'], 'taken')
        events = all_events(client, 'Bob')
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertGreaterEqual(len(compacted), 2)
        self.assertTrue(all(c['pinned'] for c in compacted))
        kept = [c for c in compacted if c['pinned_messages']]
        self.assertTrue(kept)
        # Once a cut covers it, every cut keeps the same message.
        self.assertEqual(compacted[compacted.index(kept[0]):], kept)
        self.assertTrue(all(c['pinned_messages'] == kept[0]['pinned_messages'] for c in kept))
        texts = lambda r: [i['content'][0]['text'] for i in r['input'] if i.get('role') == 'user']
        work = [r for r in drain(self.model) if not is_summary(r)]
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in work))
        after = [r for r in work if 'covering the start of turn 1]' in texts(r)[0]]
        self.assertTrue(after)
        last = texts(after[-1])
        prompt = last.index('long:40')
        self.assertEqual(last[prompt:prompt + 2], ['long:40', correction])
        self.assertEqual(last.count(correction), 1)
        self.assertNotIn(correction, last[0])

    def test_a_summary_inside_a_turn_runs_beside_its_calls(self):
        # Each summary takes a second. Inside the turn, it runs beside the
        # call its boundary sends, with the view as it is, and is installed
        # before that call's tools run.
        self.model.timeline, self.model.summary_delay = [], 1.0
        client = self.client(tools='shell', settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:150x40')['result']['message']
        ended = client.finished(turn, timeout=60)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = node_item(client, 'Bob', ended['data']['checkpoint'])['result']
        self.assertIn('done after 150 rounds', json.dumps(answer))
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 65536 for r in requests))
        summaries = [(start, end) for summary, start, end in self.model.timeline if summary]
        calls = [start for summary, start, _ in self.model.timeline if not summary]
        self.assertGreaterEqual(len(summaries), 2)
        # Each summary went out with a call, either reaching the model
        # first; the next call waited for it.
        for start, end in summaries:
            self.assertLess(min(abs(call - start) for call in calls), .02)
            self.assertGreaterEqual(min(call for call in calls if call > start + .02), end)
        events = all_events(client, 'Bob')
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertEqual(len(compacted), len(summaries))
        self.assertTrue(all(c['request']['beside'] for c in compacted))
        self.assertFalse([e for e in events if e['event'] == 'compaction_failed'])
        # The turn counts the time it held for them.
        held = client.request('turns', bot='Bob', after=0, limit=10)['result']['turns']
        self.assertGreater(held[0]['summary_ms'], 0)
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

    def test_under_a_budget_a_summary_runs_beside_only_when_its_spend_is_bounded(self):
        # Under a token budget, a summary runs beside the turn only when the
        # most it may bill is known: here, when its output is bounded.
        client = self.client(tools='shell')
        for bot, bounded in (('Bob', {}), ('Ann', {'max_output_tokens': 512})):
            client.request('create', bot=bot, workspace=str(self.path), tools=['shell'],
                           compaction_instructions='Summarize.', budget_tokens=10 ** 9,
                           settings={'context_bytes': 65536, **bounded})
            turn = client.request('submit', bot=bot, request_id='1', prompt='long:150x40')['result']['message']
            ended = client.finished(turn, timeout=60)
            self.assertEqual(ended['data']['status'], 'completed', ended)
            beside = {e['data']['request']['beside'] for e in all_events(client, bot) if e['event'] == 'compacted'}
            self.assertEqual(beside, {bool(bounded)}, bot)

    def test_an_interrupt_still_installs_a_summary_that_landed_beside_a_call(self):
        # The summary lands while the call sent beside it is still held;
        # the interrupt then drops the turn's rounds before the call ends,
        # and the summary is installed and billed all the same.
        self.model.timeline, self.model.summary_delay = [], 0.5
        self.model.hold_after_summary = threading.Event()
        self.addCleanup(self.model.hold_after_summary.set)
        client = self.client(tools='shell', settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:150x40')['result']['message']
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

    def test_a_summary_beside_the_turn_counts_among_its_rounds(self):
        # Small rounds: compaction comes due past round 150, and the summary
        # beside that boundary's call is counted among the turn's rounds,
        # which end at the limit.
        self.model.timeline, self.model.summary_delay = [], 1.0
        client = self.client(tools='shell', settings={'context_bytes': 65536, 'max_rounds': 200})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:250x1')['result']['message']
        ended = client.finished(turn, timeout=90)
        # The last round may call no tool, so the turn ends on its reply.
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = node_item(client, 'Bob', ended['data']['checkpoint'])['result']
        self.assertIn('final reply', json.dumps(answer))
        requests = drain(self.model)
        self.assertEqual(len(requests), 200)
        self.assertEqual(sum(map(is_summary, requests)), 1)
        self.assertEqual(requests[-1]['tool_choice'], 'none')
        self.assertFalse(any('tool_choice' in r for r in requests[:-1]))

    def test_a_billed_retry_that_is_the_last_round_calls_no_tools(self):
        # The 199th call is billed and fails; its retry is the 200th round,
        # so it goes without calls and the turn ends on its reply.
        self.model.fail_billed_at = 199
        client = self.client(tools='shell', settings={'max_rounds': 200})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'])
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:250x1')['result']['message']
        ended = client.finished(turn, timeout=90)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        requests = drain(self.model)
        self.assertEqual(len(requests), 200)
        self.assertEqual([r.get('tool_choice') for r in requests[-2:]], [None, 'none'])

    def test_a_bot_compacts_once_its_calls_reach_its_token_threshold(self):
        # Four bytes a token, so 16K tokens is about 64 KiB: far under the
        # byte budget, which alone never makes forty 11 KiB rounds compact.
        self.model.bytes_per_token = 4
        for threshold, expected in ((16384, True), (0, False)):
            with self.subTest(compact_tokens=threshold):
                bot = f'Bob{threshold}'
                client = self.client(tools='shell', settings={'compact_tokens': threshold})
                client.request('create', bot=bot, workspace=str(self.path), tools=['shell'],
                               compaction_instructions='Summarize.')
                record = client.request('inspect', bot=bot)['result']
                self.assertEqual(record['settings']['compact_tokens'], threshold)
                turn = client.request('submit', bot=bot, request_id='1', prompt='long:40')['result']['message']
                ended = client.finished(turn, timeout=60)
                self.assertEqual(ended['data']['status'], 'completed', ended)
                requests = drain(self.model)
                compacted = [e for e in all_events(client, bot) if e['event'] == 'compacted']
                self.assertEqual(bool(compacted), expected)
                work = [r for r in requests if not is_summary(r)]
                largest = max(len(json.dumps(r)) for r in work)
                if expected:
                    # Under the threshold, give or take the round that
                    # crossed it and the summary that went ahead.
                    self.assertLess(largest, 16384 * 4 + 24576)
                else:
                    self.assertGreater(largest, 40 * 11000)
                client.close()

    def test_an_unset_threshold_follows_the_models_listed_window(self):
        # A 40,000-token window compacts at 7,000 tokens, its window less
        # 33,000: about 28 KiB at four bytes a token. The window is the
        # listing a client asked for.
        self.model.bytes_per_token, self.model.context_window = 4, 40000
        client = self.client(tools='shell')
        listed = client.request('provider_models')['result']['providers']
        self.assertEqual([m['context_tokens'] for p in listed.values() for m in p['models']][:1], [40000])
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        self.assertIsNone(client.request('inspect', bot='Bob')['result']['settings']['compact_tokens'])
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:40')['result']['message']
        self.assertEqual(client.finished(turn, timeout=60)['data']['status'], 'completed')
        self.assertTrue([e for e in all_events(client, 'Bob') if e['event'] == 'compacted'])
        work = [r for r in drain(self.model) if not is_summary(r)]
        self.assertLess(max(len(json.dumps(r)) for r in work), 7000 * 4 + 24576)

    def test_a_window_the_person_set_beats_the_models_listing(self):
        # The listing says a million tokens and the person 40,000, on
        # purpose: the bot compacts at 7,000 tokens, about 28 KiB at four
        # bytes a token, as with a listed 40,000.
        self.model.bytes_per_token, self.model.context_window = 4, 1_000_000
        client = self.client(tools='shell', extra=('--context-window', 'openai/*=40000',
                                                   '--context-window', 'other/*=2000'))
        self.assertEqual(client.ready['context_windows'], {'openai/*': 40000, 'other/*': 2000})
        client.request('provider_models')
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:40')['result']['message']
        self.assertEqual(client.finished(turn, timeout=60)['data']['status'], 'completed')
        events = all_events(client, 'Bob')
        self.assertTrue([e for e in events if e['event'] == 'compacted'])
        windows = {e['data'].get('context_window') for e in events if e['event'] == 'usage'}
        self.assertEqual(windows, {40000})
        work = [r for r in drain(self.model) if not is_summary(r)]
        self.assertLess(max(len(json.dumps(r)) for r in work), 7000 * 4 + 24576)

    def test_a_compaction_a_client_asks_for_runs_before_the_next_call(self):
        client = self.client(tools='shell')
        client.request('create', bot='Plain', workspace=str(self.path), tools=['shell'])
        refused = client.request('compact', bot='Plain')
        self.assertEqual(refused['error'], 'compaction_unavailable')
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        for n in range(3):
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt='long:2')['result']['message']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        drain(self.model)
        # Idle: it waits for the next turn, once, however often it is asked.
        asked = client.request('compact', bot='Bob')['result']
        self.assertEqual((asked['running_turn'], asked['duplicate']), (None, False))
        self.assertTrue(client.request('compact', bot='Bob')['result']['duplicate'])
        self.assertTrue(client.request('inspect', bot='Bob')['result']['compaction_requested'])
        turn = client.request('submit', bot='Bob', request_id='next', prompt='long:1')['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = drain(self.model)
        self.assertTrue(is_summary(requests[0]))
        self.assertEqual(sum(map(is_summary, requests)), 1)
        record = client.request('inspect', bot='Bob')['result']
        self.assertFalse(record['compaction_requested'])
        self.assertIsNotNone(record['compaction'])
        # Running: the next round boundary takes it, once however often it
        # is asked while its summary runs.
        self.model.summary_delay = 1.0
        turn = client.request('submit', bot='Bob', request_id='long', prompt='long:30x40')['result']['message']
        client.receive(lambda e: e.get('event') == 'tool_completed' and e.get('turn') == turn)
        asked = client.request('compact', bot='Bob')['result']
        self.assertEqual(asked['running_turn'], turn)
        while not is_summary(self.model.requests.get(timeout=10)):
            pass
        self.assertTrue(client.request('compact', bot='Bob')['result']['duplicate'])
        self.assertEqual(client.finished(turn, timeout=30)['data']['status'], 'completed')
        compacted = [e for e in all_events(client, 'Bob') if e['event'] == 'compacted' and e['turn'] == turn]
        self.assertEqual(len(compacted), 1)
        self.assertFalse(client.request('inspect', bot='Bob')['result']['compaction_requested'])

    def test_a_compaction_a_client_asks_for_survives_its_summary_parking(self):
        # The summary is turned away until its pool reopens; the turn parks
        # and, resumed, still summarizes before its call.
        client = self.client(tools='shell')
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='0', prompt='long:2')['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        drain(self.model)
        self.model.compaction_refusals, self.model.compaction_retry_after = 1, '1'
        client.request('compact', bot='Bob')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:1')['result']['message']
        ended = client.finished(turn, timeout=30)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        kinds = [e['event'] for e in all_events(client, 'Bob') if e['turn'] == turn]
        self.assertIn('turn_paced', kinds)
        self.assertEqual(kinds.count('compacted'), 1)
        work = [r for r in drain(self.model) if not is_summary(r)]
        self.assertTrue(any('summary' in json.dumps(i) for i in work[0]['input'][:2]), work[0]['input'][:2])
        self.assertFalse(client.request('inspect', bot='Bob')['result']['compaction_requested'])

    def test_a_summary_beside_a_call_is_installed_before_the_turn_parks(self):
        # Compaction comes due past round 150 and the summary takes five
        # seconds; at round 160 the turn starts a command and waits on it,
        # which parks the turn. The summary is installed before its call's
        # tools run, in this turn, and the turn resumed from the park
        # finishes with it.
        self.model.timeline, self.model.summary_delay = [], 5.0
        self.model.long_wait = 'sleep .5'
        client = self.client(tools='shell,wait', settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'wait'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:160x1')['result']['message']
        ended = client.finished(turn, timeout=60)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        events = all_events(client, 'Bob')
        compacted = [e for e in events if e['event'] == 'compacted']
        self.assertEqual([(e['turn'], e['data']['request']['beside']) for e in compacted], [(turn, True)])
        billed = [e['turn'] for e in events if e['event'] == 'usage' and e['data'].get('purpose') == 'compaction']
        self.assertEqual(billed, [turn])
        kinds = [e['event'] for e in events]
        self.assertLess(kinds.index('compacted'), kinds.index('turn_waiting'))
        # The turn counts the time it held for the summary.
        held, = client.request('turns', bot='Bob', after=0, limit=10)['result']['turns']
        self.assertGreater(held['summary_ms'], 1000)

    def test_a_failed_summary_the_turn_waited_for_counts_the_wait(self):
        # As above, but each summary comes back empty: nothing is installed,
        # each later boundary tries again, and the turn still counts the
        # time it held for them.
        self.model.timeline, self.model.summary_delay = [], 1.2
        self.model.empty_compaction, self.model.long_wait = True, 'sleep .5'
        client = self.client(tools='shell,wait', settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'wait'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:160x1')['result']['message']
        ended = client.finished(turn, timeout=60)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        events = all_events(client, 'Bob')
        self.assertNotIn('compacted', [e['event'] for e in events])
        held, = client.request('turns', bot='Bob', after=0, limit=10)['result']['turns']
        self.assertGreater(held['summary_ms'], 1000)

    def test_a_turn_resumed_before_its_summary_pool_reopens_does_not_wait_for_it(self):
        # The turn runs on another model than its summarizer. Its summary
        # beside it is turned away until its pool reopens three seconds
        # later; meanwhile the turn parks on a wait and resumes. Its own
        # model's pool is open, so it goes on rather than waiting for the
        # summarizer's.
        self.model.models = ('synthetic-model', 'synthetic-large')
        self.model.compaction_refusals, self.model.compaction_retry_after = 1, '3'
        self.model.long_wait = 'sleep .5'
        client = self.client(tools='shell,wait', settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'wait'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:160x1',
                              model='openai/synthetic-large')['result']['message']
        ended = client.finished(turn, timeout=60)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        kinds = [e['event'] for e in all_events(client, 'Bob')]
        self.assertIn('turn_waiting', kinds)
        self.assertNotIn('turn_paced', kinds)

    def test_a_round_that_overflows_before_compaction_is_due_forces_a_summary(self):
        # Four small rounds, then one that takes the turn past its budget
        # before compaction is due. Without read nothing is elided, so the
        # runtime summarizes the earlier rounds, keeping the prompt, and the
        # turn goes on; with no summarizer instructions it fails.
        client = self.client(tools='shell', settings={'context_bytes': 24576, 'compact_at': 99})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:4x250,1x600')['result']['message']
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
        turn = client.request('submit', bot='Ann', request_id='1', prompt='long:4x250,1x600')['result']['message']
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
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:12x150,2x1')['result']['message']
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


    def test_a_summary_longer_than_asked_is_asked_again_smaller(self):
        # A summarizer that writes half again what it is asked for: refused
        # at the 32 KiB limit, it is asked for 16 KiB and writes 24 KiB,
        # which the limit takes, so the turn is compacted, not left as it is.
        self.model.compaction_text = lambda request: 's' * (asked_bytes(request) * 3 // 2)
        client = self.client(tools='shell', settings={'context_bytes': 98304})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:40')['result']['message']
        self.assertEqual(client.finished(turn, timeout=60)['data']['status'], 'completed')
        compacted = [e['data'] for e in all_events(client, 'Bob') if e['event'] == 'compacted']
        self.assertTrue(compacted)
        self.assertTrue(all(c['summary_bytes'] == 24576 for c in compacted), compacted)
        asks = [asked_bytes(r) for r in drain(self.model) if is_summary(r)]
        self.assertEqual(asks[:2], [32768, 16384])
        self.assertEqual(asks.count(16384), len(compacted))
        # One that comes back no shorter is not asked again.
        self.model.compaction_text = 's' * 40000
        client.request('create', bot='Ann', workspace=str(self.path), tools=['shell'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Ann', request_id='1', prompt='long:12')['result']['message']
        client.finished(turn, timeout=60)
        asks = [asked_bytes(r) for r in drain(self.model) if is_summary(r)]
        self.assertEqual(asks[:2], [32768, 16384])
        self.assertNotIn(8192, asks)
        self.assertFalse([e for e in all_events(client, 'Ann') if e['event'] == 'compacted'])

    def drive(self, window, per_token, overhead, lines, compactions=20):
        """A lead's many turns at a `window`-token context, `per_token` bytes a
        token, with `overhead` bytes of instructions, each turn four rounds
        of `lines` lines, until it has compacted `compactions` times; each
        summary as long as it is asked for, the least room it can leave.
        The compacted events, oldest first."""
        self.model.bytes_per_token = per_token
        self.model.compaction_text = lambda request: 's' * asked_bytes(request)
        client = self.client(tools='shell', extra=('--context-window', f'openai/*={window}'))
        client.request('create', bot='Lead', workspace=str(self.path), tools=['shell'],
                       instructions='i' * overhead, compaction_instructions='Summarize.')
        compacted = []
        for n in range(400):
            turn = client.request('submit', bot='Lead', request_id=str(n),
                                  prompt=f'long:4x{lines}')['result']['message']
            self.assertEqual(client.finished(turn, timeout=60)['data']['status'], 'completed')
            drain(self.model)
            compacted = [e['data'] for e in all_events(client, 'Lead') if e['event'] == 'compacted']
            if len(compacted) >= compactions:
                break
        self.assertGreaterEqual(len(compacted), compactions)
        return compacted

    def assert_room(self, compacted, window):
        for n, data in enumerate(compacted):
            trigger = data['trigger']
            self.assertEqual((trigger['reason'], trigger['window']), ('bytes', window), data)
            # The summary, kept prompts and the kept tail leave at least a
            # quarter of the view's threshold: a third is the keep target's
            # share, and a cut lands on a turn's prompt or a round start of
            # the newest turn, so the tail may keep one older turn whole.
            self.assertGreaterEqual(data['room_bytes'], trigger['at_bytes'] // 4, (n, data))
        # Nothing piles up: the twentieth leaves the room the second did.
        rooms = [data['room_bytes'] for data in compacted[1:]]
        self.assertGreaterEqual(min(rooms), rooms[0] * 95 // 100, rooms)

    def test_a_lead_keeps_room_to_work_across_twenty_compactions_at_200k(self):
        # Four bytes a token, 40 KB of instructions and tools, rounds of
        # about 36 KB, each summary a full 64 KiB.
        compacted = self.drive(200_000, 4, 40_000, 2000)
        self.assert_room(compacted, 200_000)

    def test_a_lead_keeps_room_to_work_across_twenty_compactions_at_1m(self):
        # A byte a token, so the test sends a million bytes, not four.
        compacted = self.drive(1_000_000, 1, 15_000, 3500)
        self.assert_room(compacted, 1_000_000)


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
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path), effort='low',
                                               compaction_instructions='Summarize.'))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:30')['result']['message']
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
