"""Compaction budgets, failures, fork isolation, and request-prefix stability."""
import json
import os
import sqlite3
from unittest import skipUnless
from tests.test_runtime import AnthropicModel, ModelFixture
from bench.runtime_client import Client
from bench.targets import clean_env


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class AnthropicCompactionTests(ModelFixture):
    handler = AnthropicModel

    def test_compaction_preserves_tool_history_and_disables_new_tool_calls(self):
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell', provider='anthropic', family='anthropic',
                        model='synthetic-claude', key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'},
                        extra=('--context-bytes', '8192', '--compact-at', '50'))
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path),
                                               compaction_instructions='Summarize.', reasoning='low'))
        for n in range(7):
            prompt = ('tool:' if n == 0 else f'{n}:') + 'x' * 500
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt=prompt)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        summaries = [r for r in requests if r['system'][0]['text'] == 'Summarize.']
        self.assertTrue(summaries)
        first = summaries[0]
        self.assertEqual(first.get('tools'), requests[0]['tools'])
        self.assertEqual(first.get('tool_choice'), {'type': 'none'})
        blocks = [b for m in first['messages'] for b in m['content']]
        self.assertTrue(any(b['type'] == 'tool_use' for b in blocks))
        self.assertTrue(any(b['type'] == 'tool_result' for b in blocks))
        self.assertIsNotNone(client.request('resume', bot='Bob')['result']['compaction'])
        self.assertFalse(any(m.get('event') == 'compaction_failed' for m in client.saved))


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class CompactionTests(ModelFixture):
    def test_optional_previews_do_not_block_compaction_of_large_turns(self):
        self.model.compaction_text = 'A brief summary.'
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client)
        for n in range(15):
            self.assertEqual(self.run_turn(client, 'Bob', n, f'{n}: ' + 'x' * 150)['data']['status'],
                             'completed')
        previous = client.request('resume', bot='Bob')['result']['compaction']
        self.assertIsNotNone(previous)
        self.requests()
        for n in range(15, 20):
            self.assertEqual(self.run_turn(client, 'Bob', n, f'{n}: ' + 'x' * 1700)['data']['status'],
                             'completed')
            current = client.request('resume', bot='Bob')['result']['compaction']
            self.assertNotEqual(current, previous)
            previous = current
        requests = self.requests()
        self.assertEqual(sum(r['instructions'] == 'Summarize.' for r in requests), 5)
        self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':'), ensure_ascii=False).encode()) - 2
                            <= 4096 for r in requests))
        self.assertFalse(any(m.get('event') == 'compaction_failed' and m.get('error') == 'compaction_context_limit'
                             for m in client.saved))

    def test_output_cap_advances_compaction_without_reducing_the_input_envelope(self):
        for cap in (None, 2048):
            with self.subTest(cap=cap):
                extra = ('--context-bytes', '8192', '--compact-at', '95')
                if cap is not None:
                    extra += ('--max-output-tokens', str(cap))
                client = Client(self.binary, self.path / f'cap-{cap}.sqlite', self.url, extra=extra)
                self.addCleanup(client.close)
                self.create(client)
                for n in range(7):
                    self.assertEqual(self.run_turn(client, 'Bob', n, 'x' * 500)['data']['status'], 'completed')
                requests = self.requests()
                self.assertEqual(any(r['instructions'] == 'Summarize.' for r in requests), cap is not None)
                self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':')).encode()) - 2 <= 8192
                                    for r in requests))
                client.close()

    def test_retained_prompts_leave_room_for_history_after_repeated_compactions(self):
        client = self.client(tools='echo,history', extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client)
        for n in range(12):
            text = 'x' * 500 if n % 2 == 0 else '\"\\\nλ' * 100
            self.assertEqual(self.run_turn(client, 'Bob', n, f'{n}: ' + text)['data']['status'], 'completed')
        self.assertIsNotNone(client.request('resume', bot='Bob')['result']['compaction'])
        for offset in (0, 97):
            ended = self.run_turn(client, 'Bob', f'h-{offset}', f'history:1,{offset},97')
            self.assertEqual(ended['data']['status'], 'completed')
            requests = self.requests()
            results = [json.loads(i['output']) for r in requests for i in r['input']
                       if i.get('type') == 'function_call_output' and i.get('call_id') == 'history-1']
            self.assertTrue(results)
            self.assertTrue(all('error' not in r and r['text'] for r in results), results)
            self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':'), ensure_ascii=False).encode()) - 2
                                <= 4096 for r in requests))
        self.assertFalse(any(m.get('event') == 'compaction_failed' and m.get('error') == 'compaction_context_limit'
                             for m in client.saved))

    def test_nonshrinking_summary_is_billed_without_installing_it(self):
        self.model.compaction_text = 'x' * 900
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client)
        for n in range(3):
            self.assertEqual(self.run_turn(client, 'Bob', n, str(n) * 500)['data']['status'], 'completed')
        bot = client.request('resume', bot='Bob')['result']
        self.assertIsNone(bot['compaction'])
        self.assertEqual(bot['tokens_used'], 440)  # three answers plus the rejected summary
        self.assertTrue(any(m.get('event') == 'compaction_failed' and m.get('error') == 'compaction_not_smaller'
                            for m in client.saved))

    def test_escaped_summary_cannot_exceed_the_encoded_prefix_budget(self):
        self.model.compaction_text = '\\' * 1100  # raw target fits; encoded block does not
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client)
        for n in range(3):
            self.assertEqual(self.run_turn(client, 'Bob', n, str(n) * 500)['data']['status'], 'completed')
        bot = client.request('resume', bot='Bob')['result']
        self.assertIsNone(bot['compaction'])
        self.assertEqual(bot['tokens_used'], 440)
        self.assertTrue(any(m.get('event') == 'compaction_failed' and m.get('error') == 'compaction_context_limit'
                            for m in client.saved))

    def test_small_items_trigger_compaction_below_the_byte_threshold(self):
        client = self.client(extra=('--context-bytes', '65536', '--context-items', '16'))
        self.create(client)
        for n in range(10):
            self.assertEqual(self.run_turn(client, 'Bob', n, 'small')['data']['status'], 'completed')
        requests = self.requests()
        self.assertTrue(any(r['instructions'] == 'Summarize.' for r in requests))
        self.assertTrue(all(len(r['input']) <= 15 for r in requests))

    def test_history_result_takes_precedence_over_optional_previews(self):
        client = self.client(tools='history', extra=('--context-bytes', '4096'))
        client.request('create', bot='Bob', workspace=str(self.path))
        for n in range(30):
            self.assertEqual(self.run_turn(client, 'Bob', n, f'{n}: ' + 'x' * 150)['data']['status'], 'completed')
        self.requests()
        self.model.history_prefill = 1800
        for offset in (0, 97):
            self.assertEqual(self.run_turn(client, 'Bob', f'h-{offset}', f'history:1,{offset},97')['data']['status'],
                             'completed')
            requests = self.requests()
            results = [json.loads(i['output']) for r in requests for i in r['input']
                       if i.get('type') == 'function_call_output' and i.get('call_id') == 'history-1']
            self.assertTrue(results)
            page = results[-1]
            self.assertNotIn('error', page)
            self.assertEqual(page['offset'], offset)
            self.assertEqual(page['next_offset'], offset + 97)
            self.assertEqual(len(page['text'].encode()), 97)
            self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':'), ensure_ascii=False).encode()) - 2 <= 4096
                                for r in requests))
            self.assertTrue(any('[context note]' in str(r['input']) for r in requests))

    def test_optional_previews_leave_room_for_the_current_prompt(self):
        client = self.client(extra=('--context-bytes', '4096'))
        client.request('create', bot='Bob', workspace=str(self.path))
        for n in range(30):
            self.assertEqual(self.run_turn(client, 'Bob', n, f'{n}: ' + 'x' * 150)['data']['status'], 'completed')
        self.requests()
        self.assertEqual(self.run_turn(client, 'Bob', 'large', 'y' * 2000)['data']['status'], 'completed')
        requests = self.requests()
        self.assertTrue(requests)
        self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':')).encode()) - 2 <= 4096
                            for r in requests))
        self.assertTrue(any('[context note]' in str(r['input']) for r in requests))

    def test_oversized_note_preserves_previous_note_and_bot_remains_usable(self):
        client = self.client(tools='note', extra=('--context-bytes', '4096'))
        client.request('create', bot='Bob', workspace=str(self.path))
        self.assertEqual(self.run_turn(client, 'Bob', 'seed', 'note:remember violet')['data']['status'], 'completed')
        for n, note in enumerate(('x' * 5000, '\\' * 2100, 'n' * 2000)):
            self.model.note_text = note
            ended = self.run_turn(client, 'Bob', f'bad-{n}', 'note:replace')
            if n == 2:
                self.assertEqual(ended['data']['status'], 'completed')
                self.assertTrue(any('note_context_limit' in str(r['input']) for r in self.requests()))
            # The provider output itself can overflow this turn, but its note
            # must not poison subsequent turns or replace the last good note.
            self.requests()
            self.assertEqual(self.run_turn(client, 'Bob', f'next-{n}', 'continue')['data']['status'], 'completed')
            requests = self.requests()
            self.assertTrue(any('remember violet' in str(r['input']) for r in requests))
            with sqlite3.connect(self.path / 'state.sqlite') as db:
                self.assertEqual(db.execute('select n.text from bots b join notes n on n.node=b.note where b.name=?',
                                            ('Bob',)).fetchone()[0], 'remember violet')
        del self.model.note_text
        self.assertEqual(self.run_turn(client, 'Bob', 'clear', 'note:')['data']['status'], 'completed')

    def test_pinned_note_counts_toward_compaction_and_request_room(self):
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '75'))
        self.create(client)
        # Seed a durable note at an existing node, then reopen the daemon.
        # Escaped note bytes count; transcript payloads remain small.
        self.run_turn(client, 'Bob', 'seed', 'seed')
        client.close()
        db = sqlite3.connect(self.path / 'state.sqlite')
        head = db.execute('select head from bots where name="Bob"').fetchone()[0]
        db.execute('insert into notes(node,text) values (?,?)', (head, '"\n' * 550))
        db.execute('update bots set note=? where name="Bob"', (head,))
        db.commit()
        db.close()
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        extra=('--context-bytes', '4096', '--compact-at', '75'))
        self.addCleanup(client.close)
        self.requests()
        for n in range(6):
            self.assertEqual(self.run_turn(client, 'Bob', n, 'small')['data']['status'], 'completed')
        requests = self.requests()
        self.assertTrue(any(r['instructions'] == 'Summarize.' for r in requests))
        normal = [r for r in requests if r['instructions'] != 'Summarize.']
        self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':')).encode()) - 2 <= 4096
                            for r in normal))

    def test_parked_summary_resumes_as_a_summary_after_restart(self):
        extra = ('--context-bytes', '4096', '--compact-at', '50')
        client = self.client(extra=extra)
        self.create(client)
        for n in range(2):
            self.run_turn(client, 'Bob', n, str(n) * 500)
        self.requests()
        self.model.compaction_refusals = 1
        turn = client.request('submit', bot='Bob', request_id='next', prompt='2' * 500)['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn)
        client.close(kill=True)
        client = Client(self.binary, self.path / 'state.sqlite', self.url, extra=extra)
        self.addCleanup(client.close)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = self.requests()
        self.assertEqual(sum(r['instructions'] == 'Summarize.' for r in requests), 2)
        self.assertEqual(sum(r['instructions'] != 'Summarize.' for r in requests), 1)
        self.assertIsNotNone(client.request('resume', bot='Bob')['result']['compaction'])

    def test_exhausted_summary_is_skipped_when_the_ordinary_call_resumes(self):
        for restart in (False, True):
            with self.subTest(restart=restart):
                path = self.path / f'park-{restart}.sqlite'
                extra = ('--context-bytes', '4096', '--compact-at', '50')
                client = Client(self.binary, path, self.url, extra=extra)
                self.addCleanup(client.close)
                self.create(client)
                for n in range(2):
                    self.run_turn(client, 'Bob', n, str(n) * 500)
                self.requests()
                self.model.compaction_refusals = 64
                turn = client.request('submit', bot='Bob', request_id='next', prompt='2' * 500)['result']['turn']
                client.receive(lambda m: m.get('event') == 'compaction_failed', timeout=15)
                client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn)
                if restart:
                    client.close(kill=True)
                    client = Client(self.binary, path, self.url, extra=extra)
                    self.addCleanup(client.close)
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                requests = self.requests()
                self.assertEqual(sum(r['instructions'] == 'Summarize.' for r in requests), 64)
                self.assertEqual(sum(r['instructions'] != 'Summarize.' for r in requests), 1)
                # A subsequent head may compact again; the failed attempt does
                # not permanently disable compaction for this bot.
                self.run_turn(client, 'Bob', 'later', 'continue')
                self.assertTrue(any(r['instructions'] == 'Summarize.' for r in self.requests()))
                client.close()

    def run_turn(self, client, bot, request, prompt):
        response = client.request('submit', bot=bot, request_id=str(request), prompt=prompt)
        self.assertIn('result', response)
        return client.finished(response['result']['turn'])

    def requests(self):
        out = []
        while not self.model.requests.empty():
            out.append(self.model.requests.get())
        return out

    def create(self, client, bot='Bob', **options):
        response = client.request('create', bot=bot, workspace=str(self.path),
                                  compaction_instructions='Summarize.', **options)
        self.assertIn('result', response)

    def test_summary_cost_is_durable_and_stops_the_next_call_at_budget(self):
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client, budget_tokens=330)
        for n in range(3):
            ended = self.run_turn(client, 'Bob', n, str(n) * 500)
        self.assertEqual(ended['data']['error'], 'budget_exhausted')
        requests = self.requests()
        self.assertEqual(len(requests), 3)  # two answers, one summary; no fourth call
        self.assertEqual(sum(r['instructions'] == 'Summarize.' for r in requests), 1)
        self.assertEqual(client.request('resume', bot='Bob')['result']['tokens_used'], 330)
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        usage = [e['data'] for e in events if e['event'] == 'usage']
        self.assertEqual(sum(u['input_tokens'] + u['output_tokens'] for u in usage), 330)
        self.assertEqual(sum(u.get('purpose') == 'compaction' for u in usage), 1)
        self.assertFalse(any(m.get('event') == 'text_delta' and '[compaction request]' in m.get('text', '')
                             for m in client.saved))
        self.assertTrue(any(m.get('event') == 'compaction_text_delta' for m in client.saved))
        client.close()
        db = sqlite3.connect(self.path / 'state.sqlite')
        self.addCleanup(db.close)
        self.assertEqual(db.execute('select sum(model_rounds) from turns').fetchone()[0], 3)
        self.assertEqual(db.execute('select sum(input_tokens+output_tokens) from turns').fetchone()[0], 330)

    def test_empty_summary_is_charged_even_though_the_view_is_not_changed(self):
        self.model.empty_compaction = True
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client, budget_tokens=330)
        for n in range(3):
            ended = self.run_turn(client, 'Bob', n, str(n) * 500)
        self.assertEqual(ended['data']['error'], 'budget_exhausted')
        self.assertEqual(len(self.requests()), 3)
        bot = client.request('resume', bot='Bob')['result']
        self.assertEqual(bot['tokens_used'], 330)
        self.assertIsNone(bot['compaction'])
        self.assertTrue(any(m.get('event') == 'compaction_failed' and m.get('error') == 'empty_summary'
                            for m in client.saved))

    def test_forks_compact_shared_cuts_and_keep_their_own_summary(self):
        client = self.client(extra=('--context-bytes', '65536'))
        self.create(client)
        for n in range(6):
            self.run_turn(client, 'Bob', n, str(n) * 500)
        client.request('fork', source='Bob', bot='Alice', workspace=str(self.path))
        client.close()
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        extra=('--context-bytes', '8192', '--compact-at', '50', '--compact-keep', '25'))
        self.addCleanup(client.close)
        for bot in ('Bob', 'Alice'):
            self.assertEqual(self.run_turn(client, bot, 'next', 'continue')['data']['status'], 'completed')
        db = sqlite3.connect(self.path / 'state.sqlite')
        self.addCleanup(db.close)
        rows = db.execute('select node,cut from compactions order by node').fetchall()
        self.assertEqual(len(rows), 2)
        self.assertNotEqual(rows[0][0], rows[1][0])
        self.assertEqual(rows[0][1], rows[1][1])

    def test_failed_summaries_do_not_grow_requests_with_the_transcript(self):
        self.model.reject_compaction = True
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client)
        for n in range(24):
            self.assertEqual(self.run_turn(client, 'Bob', n, f'{n}: ' + 'x' * 500)['data']['status'], 'completed')
        summaries = [r for r in self.requests() if r['instructions'] == 'Summarize.']
        self.assertTrue(summaries)
        self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':')).encode()) <= 4098 for r in summaries))
        # Once the backlog outgrows the budget, each attempt is a catch-up
        # step over the oldest turns, still within the budget.
        self.assertTrue(summaries[-1]['input'][0]['content'][0]['text'].startswith('0: '))
        self.assertEqual(len(client.request('turns', bot='Bob', after=0, limit=64)['result']['turns']), 24)

    def test_a_backlog_is_caught_up_oldest_first_once_summaries_succeed(self):
        self.model.reply_text = 'x' * 500
        self.model.reject_compaction = True
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client)
        for n in range(12):
            self.run_turn(client, 'Bob', n, f'{n}: short')
        self.model.reject_compaction = False
        for n in range(12, 24):
            self.assertEqual(self.run_turn(client, 'Bob', n, f'{n}: short')['data']['status'], 'completed')
        compacted, after = [], 0
        while page := client.request('events', bot='Bob', after=after, limit=256)['result']['events']:
            compacted += [e['data'] for e in page if e['event'] == 'compacted']
            after = page[-1]['cursor']
        steps = [c for c in compacted if c['catch_up']]
        self.assertGreater(len(steps), 1)
        # Steps are contiguous from the first turn, each within the budget,
        # and ordinary compaction takes over once caught up.
        self.assertEqual(compacted[0]['span_turns'][0], 1)
        for earlier, later in zip(compacted, compacted[1:]):
            self.assertEqual(later['span_turns'][0], earlier['span_turns'][1] + 1)
        self.assertTrue(all(c['bytes'] <= 4096 for c in steps))
        self.assertTrue(all(c['covered_turns'][0] == 1 for c in compacted))
        self.assertEqual([c['catch_up'] for c in compacted],
                         [True] * len(steps) + [False] * (len(compacted) - len(steps)))
        self.assertFalse(compacted[-1]['catch_up'])
        for c in compacted:
            self.assertGreater(c['reclaimed_bytes'], 0)
            self.assertGreaterEqual(c['reclaimed_items'], 0)
            self.assertEqual(c['headroom_bytes'], c['input_limit']['bytes'] - c['context_after']['bytes'])

    def test_prompt_cache_keys_follow_the_shared_prefix(self):
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client)
        self.create(client, bot='Eve')
        # A fork with its source's instructions shares the source's prefix
        # and so its key, as does a fork of that fork; new instructions or a
        # new bot mean a new prefix and a key of their own.
        client.request('fork', source='Bob', bot='Alice', workspace=str(self.path))
        client.request('fork', source='Alice', bot='Ann', workspace=str(self.path))
        client.request('fork', source='Bob', bot='Carol', workspace=str(self.path), instructions='Other.')
        keys = {}
        for name in ('Alice', 'Ann', 'Carol', 'Eve'):
            self.run_turn(client, name, name, 'small')
            keys[name] = self.requests()[0]['prompt_cache_key']
        for n in range(3):
            self.run_turn(client, 'Bob', n, str(n) * 500)
        bob = self.requests()
        calls = {r['prompt_cache_key'] for r in bob if r['instructions'] != 'Summarize.'}
        summaries = {r['prompt_cache_key'] for r in bob if r['instructions'] == 'Summarize.'}
        self.assertEqual(len(calls), 1)
        self.assertEqual(summaries, {calls.copy().pop() + '-summary'})
        self.assertEqual({keys['Alice'], keys['Ann']}, calls)
        self.assertEqual(len({keys['Carol'], keys['Eve']} | calls), 3)

    def test_normal_calls_and_forks_reuse_an_unchanged_compacted_prefix(self):
        client = self.client(extra=('--context-bytes', '4096', '--compact-at', '50'))
        self.create(client)
        for n in range(3):
            self.run_turn(client, 'Bob', n, str(n) * 500)
        # A larger budget prevents another compaction while testing pure append.
        client.close()
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        extra=('--context-bytes', '16384'))
        self.addCleanup(client.close)
        self.requests()
        self.run_turn(client, 'Bob', 'a', 'small-a')
        first = self.requests()[-1]
        client.request('fork', source='Bob', bot='Alice', workspace=str(self.path))
        self.run_turn(client, 'Bob', 'b', 'small-b')
        second = self.requests()[-1]
        self.run_turn(client, 'Alice', 'c', 'small-c')
        fork = self.requests()[-1]
        self.assertTrue(first['input'][0]['content'][0]['text'].startswith('[compaction summary'))
        self.assertEqual(first['input'], second['input'][:len(first['input'])])
        self.assertEqual(second['input'][:-1], fork['input'][:-1])
        self.assertEqual(first['tools'], second['tools'])
        self.assertEqual(first['instructions'], second['instructions'])


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class AnthropicThinkingBindingTests(ModelFixture):
    """The endpoint refuses a replayed thinking block whose earlier context
    changed, as newer Claude models do for enforced accounts."""
    handler = AnthropicModel

    def setUp(self):
        super().setUp()
        self.model.bind_thinking = True
        self.model.binding_errors = []

    def anthropic(self, extra):
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell', provider='anthropic', family='anthropic',
                        model='synthetic-claude', key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'}, extra=extra)
        self.addCleanup(client.close)
        return client

    def requests(self):
        out = []
        while not self.model.requests.empty():
            out.append(self.model.requests.get())
        return out

    def turn(self, client, bot, request, prompt):
        turn = client.request('submit', bot=bot, request_id=str(request), prompt=prompt)['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    @staticmethod
    def thinking(message):
        return sum(b['type'] == 'thinking' for b in message['content'])

    def test_a_sliding_window_drops_thinking_bound_to_the_turns_it_left(self):
        client = self.anthropic(('--context-bytes', '4096'))
        client.request('create', bot='Bob', workspace=str(self.path), reasoning='low')
        for n in range(12):
            self.turn(client, 'Bob', n, ('tool:' if n % 3 == 0 else f'{n}:') + 'x' * 400)
        requests = self.requests()
        self.assertEqual(self.model.binding_errors, [])
        assistants = [[self.thinking(m) for m in r['messages'] if m['role'] == 'assistant'] for r in requests]
        # The window slid: some requests sent older answers without thinking,
        # and answers written after the slide kept theirs.
        self.assertTrue(any(0 in counts for counts in assistants))
        self.assertTrue(any(counts and counts[-1] == 1 and 0 in counts for counts in assistants))

    def test_summaries_and_the_compacted_window_carry_no_foreign_thinking(self):
        client = self.anthropic(('--context-bytes', '8192', '--compact-at', '50'))
        client.request('create', bot='Bob', workspace=str(self.path), reasoning='low',
                       compaction_instructions='Summarize.')
        for n in range(8):
            self.turn(client, 'Bob', n, ('tool:' if n % 3 == 0 else f'{n}:') + 'x' * 500)
        requests = self.requests()
        self.assertEqual(self.model.binding_errors, [])
        summaries = [r for r in requests if r['system'][0]['text'] == 'Summarize.']
        self.assertTrue(summaries)
        self.assertFalse(any(self.thinking(m) for r in summaries for m in r['messages']))

    def test_a_fork_keeps_thinking_only_under_its_sources_instructions(self):
        client = self.anthropic(('--context-bytes', '65536'))
        client.request('create', bot='Bob', workspace=str(self.path), reasoning='low')
        for n in range(3):
            self.turn(client, 'Bob', n, ('tool:' if n == 0 else f'{n}:') + 'x' * 100)
        self.requests()
        client.request('fork', source='Bob', bot='Alice', workspace=str(self.path))
        client.request('fork', source='Bob', bot='Carol', workspace=str(self.path), instructions='Other.')
        self.turn(client, 'Alice', 'a', 'same')
        alice = self.requests()[0]
        self.turn(client, 'Carol', 'c', 'other')
        carol = self.requests()[0]
        self.assertEqual(self.model.binding_errors, [])
        self.assertTrue(all(self.thinking(m) for m in alice['messages'] if m['role'] == 'assistant'))
        self.assertFalse(any(self.thinking(m) for m in carol['messages']))
