"""Compaction budgets, failures, fork isolation, and request-prefix stability."""
import json
import os
import queue
import sqlite3
import threading
from contextlib import closing
from unittest import skipUnless
from tests.test_runtime import AnthropicModel, ModelFixture, is_summary
from bench.runtime_client import Client
from bench.targets import clean_env


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class AnthropicCompactionTests(ModelFixture):
    handler = AnthropicModel

    def test_a_summary_copies_the_call_before_it_with_its_tools_and_tool_choice(self):
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell', provider='anthropic', family='anthropic',
                        model='synthetic-claude', key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'},
                        settings={'context_bytes': 8192, 'compact_at': 50})
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path),
                                               compaction_instructions='Summarize.', effort='low'))
        for n in range(7):
            prompt = ('tool:' if n == 0 else f'{n}:') + 'x' * 500
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt=prompt)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        summaries = [r for r in requests if is_summary(r)]
        self.assertTrue(summaries)
        first = summaries[0]
        # The call before it through the span, at a turn's prompt: its
        # system prompt, tools, tool choice, and first messages unchanged,
        # so the cache covers the tools and instructions, and the request
        # to write last. A request of its own would send the tools uncached.
        before = requests[requests.index(first) - 1]
        self.assertEqual(first['system'], before['system'])
        self.assertEqual(first['tools'], before['tools'])
        self.assertNotIn('tool_choice', first)
        self.assertEqual(first['thinking'], before['thinking'])
        copied = first['messages'][:-1]
        self.assertEqual(copied, before['messages'][:len(copied)])
        self.assertTrue(first['messages'][-1]['content'][-1]['text'].endswith('Summarize.'))
        blocks = [b for m in first['messages'] for b in m['content']]
        self.assertTrue(any(b['type'] == 'tool_use' for b in blocks))
        self.assertTrue(any(b['type'] == 'tool_result' for b in blocks))
        self.assertIsNotNone(client.request('resume', bot='Bob')['result']['compaction'])
        self.assertFalse(any(m.get('event') == 'compaction_failed' for m in client.saved))

    def test_a_summary_inside_a_turn_copies_the_call_before_it_whole(self):
        # Anthropic reads a cache where a breakpoint went, and the call put
        # its own at its end, so a summary inside a turn copies that call
        # whole, then asks. The result the model answered since stays out.
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell', provider='anthropic', family='anthropic',
                        model='synthetic-claude', key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'},
                        settings={'context_bytes': 32768})
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path),
                                               tools=['echo', 'shell'], compaction_instructions='Summarize.',
                                               effort='low'))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:6')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        indices = [n for n, r in enumerate(requests) if is_summary(r)]
        self.assertEqual(len(indices), 2)
        for index in indices:
            # The call before it, or the one before that when a summary
            # beside the boundary's own call reached the model second.
            summary = requests[index]
            calls = [r for r in requests[max(0, index - 2):index] if not is_summary(r)]
            call = next((c for c in reversed(calls) if c['messages'] == summary['messages'][:-1]), calls[-1])
            self.assertEqual((summary['system'], summary['tools']), (call['system'], call['tools']))
            self.assertEqual(summary['messages'][:-1], call['messages'])
            self.assertTrue(summary['messages'][-1]['content'][-1]['text'].endswith('Summarize.'))
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        requested = [e['data']['request'] for e in events if e['event'] == 'compacted']
        self.assertEqual([(r['form'], r['items']) for r in requested], [('copy', 5), ('copy', 5)])
        self.assertTrue(all(r['estimate']['copy'] * 5 < r['estimate']['own'] for r in requested))

    def test_a_paced_copy_inside_a_turn_sends_the_same_call_after_a_restart(self):
        # Small rounds, so the span ends before the call's window does. The
        # copy had that window whole; the retry reads the view with the
        # round since, and sends the call again through the node it ended
        # at, not through the span or a later node.
        def start():
            client = Client(self.binary, self.path / 'state.sqlite', self.url,
                            tools='echo,shell', provider='anthropic', family='anthropic',
                            model='synthetic-claude', key_env='ANTHROPIC_TEST_KEY',
                            env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'},
                            settings={'context_bytes': 8192})
            self.addCleanup(client.close)
            return client
        client = start()
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path),
                                               tools=['echo', 'shell'], compaction_instructions='Summarize.',
                                               effort='low'))
        self.model.compaction_refusals = 1
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:12x40')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn)
        client.close(kill=True)
        client = start()
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        paced, retry, _ = [n for n, r in enumerate(requests) if is_summary(r)]
        self.assertEqual(retry, paced + 1)
        self.assertEqual(requests[retry], requests[paced])
        self.assertEqual(requests[paced]['messages'][:-1], requests[paced - 1]['messages'])
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        requested = [e['data']['request'] for e in events if e['event'] == 'compacted']
        self.assertEqual([(r['form'], r['items']) for r in requested], [('copy', 11), ('copy', 11)])


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class CompactionTests(ModelFixture):
    def test_optional_previews_do_not_block_compaction_of_large_turns(self):
        self.model.compaction_text = 'A brief summary.'
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
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
        self.assertEqual(sum(is_summary(r) for r in requests), 5)
        self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':'), ensure_ascii=False).encode()) - 2
                            <= 4096 for r in requests))
        self.assertFalse(any(m.get('event') == 'compaction_failed' and m.get('error') == 'compaction_context_limit'
                             for m in client.saved))

    def test_output_cap_advances_compaction_without_reducing_the_input_envelope(self):
        for cap in (None, 2048):
            with self.subTest(cap=cap):
                capped = {'max_output_tokens': cap} if cap is not None else {}
                client = Client(self.binary, self.path / f'cap-{cap}.sqlite', self.url,
                                settings={'context_bytes': 8192, 'compact_at': 95, **capped})
                self.addCleanup(client.close)
                self.create(client)
                for n in range(7):
                    self.assertEqual(self.run_turn(client, 'Bob', n, 'x' * 500)['data']['status'], 'completed')
                requests = self.requests()
                self.assertEqual(any(is_summary(r) for r in requests), cap is not None)
                self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':')).encode()) - 2 <= 8192
                                    for r in requests))
                client.close()

    def test_retained_prompts_leave_room_for_history_after_repeated_compactions(self):
        client = self.client(tools='echo,history', settings={'context_bytes': 4096, 'compact_at': 50})
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
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
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
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
        self.create(client)
        for n in range(3):
            self.assertEqual(self.run_turn(client, 'Bob', n, str(n) * 500)['data']['status'], 'completed')
        bot = client.request('resume', bot='Bob')['result']
        self.assertIsNone(bot['compaction'])
        self.assertEqual(bot['tokens_used'], 440)
        self.assertTrue(any(m.get('event') == 'compaction_failed' and m.get('error') == 'compaction_context_limit'
                            for m in client.saved))

    def test_small_items_trigger_compaction_below_the_byte_threshold(self):
        client = self.client(settings={'context_bytes': 65536, 'context_items': 16})
        self.create(client)
        for n in range(10):
            self.assertEqual(self.run_turn(client, 'Bob', n, 'small')['data']['status'], 'completed')
        requests = self.requests()
        self.assertTrue(any(is_summary(r) for r in requests))
        self.assertTrue(all(len(r['input']) <= 15 for r in requests))

    def test_history_result_takes_precedence_over_optional_previews(self):
        client = self.client(tools='history', settings={'context_bytes': 4096})
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
        client = self.client(settings={'context_bytes': 4096})
        client.request('create', bot='Bob', workspace=str(self.path))
        for n in range(30):
            self.assertEqual(self.run_turn(client, 'Bob', n, f'{n}: ' + 'x' * 150)['data']['status'], 'completed')
        self.requests()
        self.assertEqual(self.run_turn(client, 'Bob', 'large', 'y' * 2000)['data']['status'], 'completed')
        requests = self.requests()
        self.assertTrue(requests)
        # The bound is on the bytes sent, so measure UTF-8, not \u escapes.
        self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':'), ensure_ascii=False).encode()) - 2 <= 4096
                            for r in requests))
        self.assertTrue(any('[context note]' in str(r['input']) for r in requests))

    def test_oversized_note_preserves_previous_note_and_bot_remains_usable(self):
        client = self.client(tools='note', settings={'context_bytes': 4096})
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
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 75})
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
                        settings={'context_bytes': 4096, 'compact_at': 75})
        self.addCleanup(client.close)
        self.requests()
        for n in range(6):
            self.assertEqual(self.run_turn(client, 'Bob', n, 'small')['data']['status'], 'completed')
        requests = self.requests()
        self.assertTrue(any(is_summary(r) for r in requests))
        normal = [r for r in requests if not is_summary(r)]
        self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':')).encode()) - 2 <= 4096
                            for r in normal))

    def test_parked_summary_resumes_as_a_summary_after_restart(self):
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
        self.create(client)
        for n in range(2):
            self.run_turn(client, 'Bob', n, str(n) * 500)
        self.requests()
        self.model.compaction_refusals = 1
        turn = client.request('submit', bot='Bob', request_id='next', prompt='2' * 500)['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn)
        client.close(kill=True)
        # The bot keeps its settings across the restart.
        client = Client(self.binary, self.path / 'state.sqlite', self.url)
        self.addCleanup(client.close)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = self.requests()
        self.assertEqual(sum(is_summary(r) for r in requests), 2)
        self.assertEqual(sum(not is_summary(r) for r in requests), 1)
        self.assertIsNotNone(client.request('resume', bot='Bob')['result']['compaction'])

    def test_exhausted_summary_is_skipped_when_the_ordinary_call_resumes(self):
        for restart in (False, True):
            with self.subTest(restart=restart):
                path = self.path / f'park-{restart}.sqlite'
                client = Client(self.binary, path, self.url, settings={'context_bytes': 4096, 'compact_at': 50})
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
                    client = Client(self.binary, path, self.url)
                    self.addCleanup(client.close)
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                requests = self.requests()
                self.assertEqual(sum(is_summary(r) for r in requests), 64)
                self.assertEqual(sum(not is_summary(r) for r in requests), 1)
                # A subsequent head may compact again; the failed attempt does
                # not permanently disable compaction for this bot.
                self.run_turn(client, 'Bob', 'later', 'continue')
                self.assertTrue(any(is_summary(r) for r in self.requests()))
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
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
        self.create(client, budget_tokens=330)
        for n in range(3):
            ended = self.run_turn(client, 'Bob', n, str(n) * 500)
        self.assertEqual(ended['data']['error'], 'budget_exhausted')
        requests = self.requests()
        self.assertEqual(len(requests), 3)  # two answers, one summary; no fourth call
        self.assertEqual(sum(is_summary(r) for r in requests), 1)
        self.assertEqual(client.request('resume', bot='Bob')['result']['tokens_used'], 330)
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        usage = [e['data'] for e in events if e['event'] == 'usage']
        self.assertEqual(sum(u['input_tokens'] + u['output_tokens'] for u in usage), 330)
        self.assertEqual(sum(u.get('purpose') == 'compaction' for u in usage), 1)
        # The summary names the model that wrote it, so it is priced at that
        # model's rates even when that is not the turn's model.
        summary = next(u for u in usage if u.get('purpose') == 'compaction')
        bob = client.request('resume', bot='Bob')['result']
        self.assertEqual([(m['provider'], m['model']) for m in summary['models']],
                         [(bob['provider'], bob['model'])])
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
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
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
        client = self.client(settings={'context_bytes': 8192, 'compact_at': 50, 'compact_keep': 25})
        self.create(client)
        for n in range(3):
            self.run_turn(client, 'Bob', n, str(n) * 500)
        # The fork copies Bob's settings with his history, so the same next
        # prompt crosses both thresholds at the same point.
        client.request('fork', source='Bob', bot='Alice', workspace=str(self.path))
        db = sqlite3.connect(self.path / 'state.sqlite')
        self.addCleanup(db.close)
        self.assertEqual(db.execute('select count(*) from compactions').fetchone()[0], 0)
        for bot in ('Bob', 'Alice'):
            self.assertEqual(self.run_turn(client, bot, 'next', 'y' * 1500)['data']['status'], 'completed')
        rows = db.execute('select node,cut from compactions order by node').fetchall()
        self.assertEqual(len(rows), 2)
        self.assertNotEqual(rows[0][0], rows[1][0])
        self.assertEqual(rows[0][1], rows[1][1])

    def test_failed_summaries_do_not_grow_requests_with_the_transcript(self):
        self.model.reject_compaction = True
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
        self.create(client)
        for n in range(24):
            self.assertEqual(self.run_turn(client, 'Bob', n, f'{n}: ' + 'x' * 500)['data']['status'], 'completed')
        summaries = [r for r in self.requests() if is_summary(r)]
        self.assertTrue(summaries)
        self.assertTrue(all(len(json.dumps(r['input'], separators=(',', ':')).encode()) <= 4098 for r in summaries))
        # Once the backlog outgrows the budget, each attempt is a catch-up
        # step over the oldest turns, still within the budget.
        self.assertTrue(summaries[-1]['input'][0]['content'][0]['text'].startswith('0: '))
        self.assertEqual(len(client.request('turns', bot='Bob', after=0, limit=64)['result']['turns']), 24)
        # The first attempts copy the bot's calls; a catch-up step is a
        # request of its own: the client's instructions, no tools, and a key
        # of its own.
        self.assertEqual(summaries[0]['instructions'], 'Test agent.')
        self.assertEqual((summaries[-1]['instructions'], summaries[-1]['tools']), ('Summarize.', []))
        self.assertTrue(summaries[-1]['prompt_cache_key'].endswith('-summary'))

    def test_a_backlog_is_caught_up_oldest_first_once_summaries_succeed(self):
        self.model.reply_text = 'x' * 500
        self.model.reject_compaction = True
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
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
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
        self.create(client)
        self.create(client, bot='Eve')
        # A fork repeats its source's prefix and so shares its key, as does a
        # fork of that fork; a new bot means a new prefix and a key of its own.
        client.request('fork', source='Bob', bot='Alice', workspace=str(self.path))
        client.request('fork', source='Alice', bot='Ann', workspace=str(self.path))
        keys = {}
        for name in ('Alice', 'Ann', 'Eve'):
            self.run_turn(client, name, name, 'small')
            keys[name] = self.requests()[0]['prompt_cache_key']
        for n in range(3):
            self.run_turn(client, 'Bob', n, str(n) * 500)
        bob = self.requests()
        calls = {r['prompt_cache_key'] for r in bob if not is_summary(r)}
        summaries = {r['prompt_cache_key'] for r in bob if is_summary(r)}
        self.assertEqual(len(calls), 1)
        # A summary copies the bot's call, so it shares the call's key.
        self.assertEqual(summaries, calls)
        # The key is the store's, not the daemon's: a restart keeps every
        # bot's cache affinity, and it is announced in ready.
        identity = client.ready['store']['identity']
        self.assertTrue(all(k.startswith(identity + '-') for k in calls | set(keys.values())))
        client.close()
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        settings={'context_bytes': 4096, 'compact_at': 50})
        self.addCleanup(client.close)
        self.assertEqual(client.ready['store']['identity'], identity)
        self.run_turn(client, 'Bob', 'again', 'small')
        later = self.requests()
        self.assertEqual({r['prompt_cache_key'] for r in later if not is_summary(r)}, calls)
        # A live backup has the same durable lineage and bot IDs, but must
        # not route a divergent conversation through the source's cache key.
        copied = self.path / 'copy.sqlite'
        source = sqlite3.connect(self.path / 'state.sqlite')
        destination = sqlite3.connect(copied)
        source.backup(destination)
        source.close()
        destination.close()
        clone = Client(self.binary, copied, self.url,
                       settings={'context_bytes': 4096, 'compact_at': 50})
        self.addCleanup(clone.close)
        self.assertEqual(clone.ready['store']['lineage'], client.ready['store']['lineage'])
        self.assertNotEqual(clone.ready['store']['identity'], identity)
        self.run_turn(clone, 'Bob', 'copy', 'small')
        self.assertNotIn(self.requests()[0]['prompt_cache_key'], calls)
        copied_identity = clone.ready['store']['identity']
        clone.close()
        replacement = self.path / 'replacement.sqlite'
        source = sqlite3.connect(self.path / 'state.sqlite')
        destination = sqlite3.connect(replacement)
        source.backup(destination)
        source.close()
        destination.close()
        os.replace(replacement, copied)
        restored = Client(self.binary, copied, self.url,
                          settings={'context_bytes': 4096, 'compact_at': 50})
        self.addCleanup(restored.close)
        self.assertNotEqual(restored.ready['store']['identity'], copied_identity)
        self.assertEqual({keys['Alice'], keys['Ann']}, calls)
        self.assertEqual(len({keys['Eve']} | calls), 2)

    def test_normal_calls_and_forks_reuse_an_unchanged_compacted_prefix(self):
        client = self.client(settings={'context_bytes': 4096, 'compact_at': 50})
        self.create(client)
        for n in range(3):
            self.run_turn(client, 'Bob', n, str(n) * 500)
        # A larger budget prevents another compaction while testing pure append.
        # Settings are fixed at creation, so the test edits the store.
        client.close()
        with closing(sqlite3.connect(self.path / 'state.sqlite')) as db, db:
            db.execute('''UPDATE bots SET settings='{"context_bytes":16384}' WHERE name='Bob' ''')
        client = Client(self.binary, self.path / 'state.sqlite', self.url)
        self.addCleanup(client.close)
        self.requests()
        self.run_turn(client, 'Bob', 'a', 'small-a')
        calls = self.requests()
        first = calls[-1]
        client.request('fork', source='Bob', bot='Alice', workspace=str(self.path))
        self.run_turn(client, 'Bob', 'b', 'small-b')
        calls += self.requests()
        second = calls[-1]
        self.run_turn(client, 'Alice', 'c', 'small-c')
        calls += self.requests()
        fork = calls[-1]
        self.assertFalse(any(is_summary(r) for r in calls))
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

    def anthropic(self, settings):
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell', provider='anthropic', family='anthropic',
                        model='synthetic-claude', key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'}, settings=settings)
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
        client = self.anthropic(settings={'context_bytes': 4096})
        client.request('create', bot='Bob', workspace=str(self.path), effort='low')
        for n in range(12):
            self.turn(client, 'Bob', n, ('tool:' if n % 3 == 0 else f'{n}:') + 'x' * 400)
        requests = self.requests()
        self.assertEqual(self.model.binding_errors, [])
        assistants = [[self.thinking(m) for m in r['messages'] if m['role'] == 'assistant'] for r in requests]
        # The window slid: some requests sent older answers without thinking,
        # and answers written after the slide kept theirs.
        self.assertTrue(any(0 in counts for counts in assistants))
        self.assertTrue(any(counts and counts[-1] == 1 and 0 in counts for counts in assistants))

    def test_summaries_and_the_compacted_window_replay_only_bound_thinking(self):
        client = self.anthropic(settings={'context_bytes': 8192, 'compact_at': 50})
        client.request('create', bot='Bob', workspace=str(self.path), effort='low',
                       compaction_instructions='Summarize.')
        for n in range(8):
            self.turn(client, 'Bob', n, ('tool:' if n % 3 == 0 else f'{n}:') + 'x' * 500)
        requests = self.requests()
        self.assertEqual(self.model.binding_errors, [])
        summaries = [r for r in requests if is_summary(r)]
        self.assertTrue(summaries)
        # A summary copies the bot's call, so the thinking that call replayed
        # is still bound to what precedes it, and goes along.
        self.assertTrue(any(self.thinking(m) for r in summaries for m in r['messages']))

    def test_a_fork_keeps_its_sources_thinking(self):
        client = self.anthropic(settings={'context_bytes': 65536})
        client.request('create', bot='Bob', workspace=str(self.path), effort='low')
        for n in range(3):
            self.turn(client, 'Bob', n, ('tool:' if n == 0 else f'{n}:') + 'x' * 100)
        self.requests()
        client.request('fork', source='Bob', bot='Alice', workspace=str(self.path))
        self.turn(client, 'Alice', 'a', 'same')
        alice = self.requests()[0]
        self.assertEqual(self.model.binding_errors, [])
        self.assertTrue(all(self.thinking(m) for m in alice['messages'] if m['role'] == 'assistant'))

    def test_an_answer_of_only_thinking_is_left_out_once_its_context_changes(self):
        client = self.anthropic(settings={'context_bytes': 4096})
        client.request('create', bot='Bob', workspace=str(self.path), effort='low')
        text = lambda m: m['content'][0].get('text')
        self.turn(client, 'Bob', 0, 'x' * 1500)
        self.turn(client, 'Bob', 1, 'think-only')
        self.turn(client, 'Bob', 2, 'after')
        bob = self.requests()[-1]
        # Under unchanged context the answer goes back as it was written.
        self.assertEqual(text(bob['messages'][0]), 'x' * 1500)
        self.assertEqual([self.thinking(m) for m in bob['messages']], [0, 1, 0, 1, 0])
        # A long prompt slides the window past the first turn, which changes
        # the context in front of the answer: its only block goes, and so
        # does the answer.
        self.turn(client, 'Bob', 3, 'y' * 1500)
        slid = self.requests()[-1]
        self.assertEqual(self.model.binding_errors, [])
        texts = [text(m) for m in slid['messages']]
        self.assertNotIn('x' * 1500, texts)
        at = texts.index('think-only')
        self.assertEqual([m['role'] for m in slid['messages'][at:at + 2]], ['user', 'user'])
        self.assertEqual(texts[at + 1], 'after')
        self.assertFalse(any(self.thinking(m) for m in slid['messages'][:at + 2]))

    def test_thinking_the_provider_drops_is_reported_live(self):
        client = self.anthropic(None)
        client.request('create', bot='Bob', workspace=str(self.path), effort='low')
        self.turn(client, 'Bob', 0, 'kept')
        self.model.report_drops = 2
        self.turn(client, 'Bob', 1, 'dropped')
        drops = [m for m in client.saved if m.get('event') == 'thinking_dropped']
        self.assertEqual([(m['bot'], m['count'], m['durable']) for m in drops], [('Bob', 2, False)])


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class SummaryCopyTests(ModelFixture):
    """A summary request copies the bot's last call through the span it
    summarizes, with the request to write after it, so the provider reads
    it from cache, when that is estimated cheaper than a request of its
    own."""

    def start(self, tools='echo', budget=4096, **options):
        client = self.client(tools=tools, settings={'context_bytes': budget, 'compact_at': 50})
        response = client.request('create', bot='Bob', workspace=str(self.path), tools=tools.split(','),
                                  compaction_instructions='Summarize.', **options)
        self.assertIn('result', response)
        return client

    def turn(self, client, request, prompt):
        turn = client.request('submit', bot='Bob', request_id=request, prompt=prompt)['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)

    def requests(self):
        out = []
        while not self.model.requests.empty():
            out.append(self.model.requests.get())
        return out

    def events(self, client, *kinds):
        events, after = [], 0
        while page := client.request('events', bot='Bob', after=after, limit=256)['result']['events']:
            events += [e for e in page if e['event'] in kinds]
            after = page[-1]['cursor']
        return events

    def copied_call(self, requests, index):
        """The call a summary copies: the one before it, or, for a summary
        beside a turn's call, the one before that when the boundary's own
        call reached the model first."""
        copied = requests[index]['input'][:-1]
        calls = [r for r in requests[max(0, index - 2):index] if not is_summary(r)]
        call = next((c for c in reversed(calls) if c['input'][:len(copied)] == copied), calls[-1])
        return call

    def copies(self, requests):
        """Each summary copies the call before it through its span: the same
        instructions, tools, tool choice, cache key, and the start of its
        input, which the Responses cache reads, then the request to write."""
        indices = [n for n, r in enumerate(requests) if is_summary(r)]
        self.assertTrue(indices)
        for index in indices:
            summary, call = requests[index], self.copied_call(requests, index)
            self.assertFalse(is_summary(call))
            self.assertEqual(summary['instructions'], call['instructions'])
            self.assertEqual(summary['tools'], call['tools'])
            self.assertNotIn('tool_choice', summary)
            self.assertEqual(summary['prompt_cache_key'], call['prompt_cache_key'])
            copied = summary['input'][:-1]
            self.assertEqual(copied, call['input'][:len(copied)])
            self.assertTrue(summary['input'][-1]['content'][0]['text'].endswith('Summarize.'))
        return indices

    def test_summaries_copy_the_call_before_them_on_the_turns_route(self):
        self.model.routes = []
        client = self.start(tools='shell', budget=8192)
        self.turn(client, 'a', 'first: ' + 'x' * 300)
        self.model.call_script = [('shell', {'command': 'seq 1 300'})] * 3
        self.turn(client, 'b', 'script')
        requests = self.requests()
        first, second = [n for n, r in enumerate(requests) if is_summary(r)]
        self.copies(requests[:second])
        # One at the turn's prompt, one inside the turn, each beside a call.
        # The first copies the call before it, on the turn's first routing
        # token, to the server that holds the call's cache. The second is
        # due a boundary after the first was installed: the call between
        # sent the view from before it, so no call sent the view it would
        # copy, and it is a request of its own.
        compacted = [e['data'] for e in self.events(client, 'compacted')]
        self.assertEqual([(c['pinned'] is not None, c['request']['form'], c['request']['beside'])
                          for c in compacted], [(False, 'copy', True), (True, 'own', True)])
        self.assertIsNone(self.model.routes[1])
        self.assertEqual(self.model.routes[first], 'route-2')

    def test_a_summary_copies_what_the_call_sent_ahead_of_a_note_written_since(self):
        client = self.start(tools='shell,note', budget=8192)
        self.turn(client, 'a', 'first: ' + 'x' * 800)
        note = 'n' * 1500
        self.model.call_script = [('shell', {'command': 'seq 1 300'}), ('note', {'text': note}),
                                  ('shell', {'command': 'seq 1 10'})]
        self.turn(client, 'b', 'script')
        requests = self.requests()
        index, = self.copies(requests)
        # The note went ahead of the view after the call that ran before
        # it; the copy sends what that call sent, and the next call carries
        # the note.
        pinned = lambda r: [i['content'][0]['text'] for i in r['input']
                            if i.get('role') == 'user' and i['content'][0]['text'].startswith('[carry-forward note')]
        call = self.copied_call(requests, index)
        own = next(r for r in requests[requests.index(call) + 1:] if not is_summary(r))
        self.assertEqual(call['input'][-1]['call_id'], 'script-0')
        self.assertEqual(pinned(requests[index]), [])
        self.assertIn(note, pinned(own)[0])

    def test_a_summary_copies_the_view_from_before_the_stubs_of_its_boundary(self):
        client = self.client(tools='shell,read',
                             settings={'context_bytes': 16384, 'compact_at': 50, 'compact_keep': 10})
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'],
                       compaction_instructions='Summarize.')
        self.turn(client, 'a', 'first: ' + 'x' * 1500)
        self.model.call_script = [('shell', {'command': f'seq {n}0001 {n}0400'}) for n in range(1, 6)]
        self.turn(client, 'b', 'script')
        requests = self.requests()
        index, = self.copies(requests)
        self.assertEqual([e['event'] for e in self.events(client, 'elided', 'compacted')],
                         ['elided', 'elided', 'compacted', 'elided', 'elided'])
        # The boundary stubbed a result the call before it saw whole, then
        # summarized beside its own call: the copy shows that result as the
        # call before did, and the boundary's call sends its stub.
        outputs = lambda r: [i['output'] for i in r['input'] if i.get('type') == 'function_call_output']
        call = self.copied_call(requests, index)
        own = next(r for r in requests[requests.index(call) + 1:] if not is_summary(r))
        copied = outputs(requests[index])
        self.assertEqual(copied, outputs(call)[:len(copied)])
        self.assertFalse(copied[-1].startswith('[tool result elided'))
        self.assertTrue(outputs(own)[len(copied) - 1].startswith('[tool result elided'))

    def test_a_summary_the_budget_forces_copies_the_call_before_it(self):
        # The third result cannot fit beside the first two, so the view is
        # over the budget at that boundary. The forced summary, a catch-up
        # step, still copies the call that sent the first two.
        client = self.start(tools='shell', budget=16384)
        turn = client.request('submit', bot='Bob', request_id='1',
                              prompt='long:2x100,1x700,1x10')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = self.requests()
        index, = self.copies(requests)
        self.assertEqual(requests[index]['input'][:-1], requests[index - 1]['input'])
        compacted, = [e['data'] for e in self.events(client, 'compacted')]
        self.assertTrue(compacted['catch_up'])
        self.assertEqual((compacted['request']['form'], compacted['request']['items']), ('copy', 5))
        self.assertGreater(compacted['request']['estimate']['own'], 4 * compacted['request']['estimate']['copy'])

    def test_a_paced_summary_the_budget_forces_copies_the_same_call_after_a_restart(self):
        # The view the retry reads is still over the budget; the call it
        # copies was not, and the retry sends that call again.
        client = self.start(tools='shell', budget=16384)
        self.model.compaction_refusals = 1
        turn = client.request('submit', bot='Bob', request_id='1',
                              prompt='long:2x100,1x700,1x10')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn)
        client.close(kill=True)
        client = Client(self.binary, self.path / 'state.sqlite', self.url, 'shell',
                        settings={'context_bytes': 16384, 'compact_at': 50})
        self.addCleanup(client.close)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = self.requests()
        paced, retry = [n for n, r in enumerate(requests) if is_summary(r)]
        self.assertEqual((self.copies(requests[:retry]), retry), ([paced], paced + 1))
        self.assertEqual(requests[retry], requests[paced])
        compacted, = [e['data'] for e in self.events(client, 'compacted')]
        self.assertTrue(compacted['catch_up'])
        self.assertEqual((compacted['request']['form'], compacted['request']['items']), ('copy', 5))

    def test_a_copy_that_calls_a_tool_is_billed_and_asked_again_on_its_own(self):
        # As Claude Code does: the copy's reply is not installed, and the
        # summary is asked for again at once, without tools.
        self.model.compaction_call = True
        client = self.start()
        for n in range(3):
            self.turn(client, str(n), str(n) * 500)
        requests = self.requests()
        copy, own = [n for n, r in enumerate(requests) if is_summary(r)]
        self.assertEqual(own, copy + 1)
        self.assertEqual(self.copies(requests[:own]), [copy])
        self.assertEqual((requests[own]['tools'], requests[own]['instructions']), ([], 'Summarize.'))
        bot = client.request('resume', bot='Bob')['result']
        self.assertIsNotNone(bot['compaction'])
        self.assertEqual(bot['tokens_used'], 550)  # three answers and both summaries
        failed = [m for m in client.saved if m.get('event') == 'compaction_failed']
        self.assertEqual([(m['error'], m.get('fallback')) for m in failed], [('compaction_tool_call', True)])
        self.assertEqual(self.events(client, 'tool_started'), [])
        # The event names the request whose summary went in.
        compacted, = self.events(client, 'compacted')
        self.assertEqual((compacted['data']['request']['form'], compacted['data']['request']['items']),
                         ('own', None))

    def test_a_turn_on_another_model_than_the_summarizer_gets_a_request_of_its_own(self):
        self.model.bodies = []
        self.model.models = ('synthetic-model', 'synthetic-large')
        self.model.routes = []
        client = self.start()
        for n in range(3):
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt=str(n) * 500,
                                  model='openai/synthetic-large')['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = self.requests()
        index, = [n for n, r in enumerate(requests) if is_summary(r)]
        summary = requests[index]
        # The bot's summarizer cannot read the cache of the turn's model,
        # nor take its routing token.
        self.assertEqual({r['model'] for r in requests if not is_summary(r)}, {'synthetic-large'})
        self.assertEqual((summary['model'], summary['instructions'], summary['tools']),
                         ('synthetic-model', 'Summarize.', []))
        self.assertTrue(summary['prompt_cache_key'].endswith('-summary'))
        self.assertIsNone(self.model.routes[index])
        self.assertIsNotNone(client.request('resume', bot='Bob')['result']['compaction'])
        self.assertEqual(self.request_of_its_own(client), self.own_bytes(summary))

    def test_a_turn_back_on_the_summarizer_does_not_copy_a_call_another_model_sent(self):
        self.model.bodies = []
        # The history this turn starts from was sent by the previous turn's
        # model, which the summarizer's cache never saw.
        self.model.models = ('synthetic-model', 'synthetic-large')
        client = self.start()
        for n in range(3):
            model = {'model': 'openai/synthetic-large'} if n < 2 else {}
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt=str(n) * 500,
                                  **model)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = self.requests()
        index, = [n for n, r in enumerate(requests) if is_summary(r)]
        summary = requests[index]
        self.assertEqual(requests[index - 1]['model'], 'synthetic-large')
        self.assertEqual((summary['model'], summary['instructions'], summary['tools']),
                         ('synthetic-model', 'Summarize.', []))
        self.assertEqual(self.request_of_its_own(client), self.own_bytes(summary))

    def test_a_turn_after_one_that_failed_before_a_call_does_not_copy_it(self):
        self.model.bodies = []
        # The previous turn's prompt is stored, but its only request was
        # refused with no usage, so no call sent the history it ends.
        self.model.refused_prompts = {'1' * 600}
        client = self.start()
        for n in range(3):
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt=str(n) * 600)['result']['turn']
            ended = client.finished(turn)['data']
            self.assertEqual(ended['status'], 'failed' if n == 1 else 'completed', ended)
        requests = self.requests()
        index, = [n for n, r in enumerate(requests) if is_summary(r)]
        self.assertEqual(requests[index - 1]['input'][-1]['content'][0]['text'], '1' * 600)
        summary = requests[index]
        self.assertEqual((summary['instructions'], summary['tools']), ('Summarize.', []))
        self.assertEqual(self.request_of_its_own(client), self.own_bytes(summary))

    def test_a_turn_after_one_that_summarized_and_failed_before_a_call_does_not_copy_it(self):
        self.model.bodies = []
        # The previous turn summarized, took a steer, and its only ordinary
        # request was refused: no call sent the view its summary left.
        self.model.refused_prompts = {'1' * 1200}
        client = self.start()
        self.turn(client, '0', '0' * 1200)
        self.requests()
        gate = threading.Event()
        self.model.request_gates = queue.Queue()
        self.model.request_gates.put(gate)
        turn = client.request('submit', bot='Bob', request_id='1', prompt='1' * 1200)['result']['turn']
        self.assertTrue(is_summary(self.model.requests.get(timeout=5)))
        steer = client.request('submit', bot='Bob', request_id='s', prompt='steer:go',
                               delivery='steer', expected_turn=turn)['result']['turn']
        gate.set()
        self.assertEqual(client.finished(turn)['data']['status'], 'failed')
        self.assertEqual(client.finished(steer)['data'].get('into'), turn)
        self.turn(client, '2', '2' * 1200)
        requests = self.requests()
        index, = [n for n, r in enumerate(requests) if is_summary(r)]
        self.assertEqual(requests[index - 1]['input'][-1]['content'][0]['text'], 'steer:go')
        summary = requests[index]
        self.assertEqual((summary['instructions'], summary['tools']), ('Summarize.', []))
        self.assertEqual(self.request_of_its_own(client, 1), self.own_bytes(summary))

    def request_of_its_own(self, client, index=None):
        """The one summary's `request`, or the one `index` names: a request
        of its own, priced."""
        compacted = self.events(client, 'compacted')
        compacted, = compacted if index is None else [compacted[index]]
        request = compacted['data']['request']
        self.assertEqual((request['form'], request['items'], request['estimate']['copy']), ('own', None, None))
        return request['estimate']['own']

    def own_bytes(self, summary):
        """What a Responses request of its own sends, as the summary is
        priced: its instructions, its empty tool list, and its input's
        items as they went, read from the request's body."""
        body, = [b for b in self.model.bodies if json.loads(b) == summary]
        text = body.decode()
        start = text.index('"input":') + len('"input":')
        _, end = json.JSONDecoder().raw_decode(text, start)
        return len(summary['instructions']) + len(json.dumps(summary['tools'])) + len(text[start:end].encode()) - 2

    def test_a_summary_by_another_model_is_a_request_of_its_own(self):
        self.model.bodies = []
        self.model.models = ('synthetic-model', 'synthetic-small')
        client = self.start(compaction_model='openai/synthetic-small')
        for n in range(3):
            self.turn(client, str(n), str(n) * 500)
        summary, = [r for r in self.requests() if is_summary(r)]
        self.assertEqual((summary['model'], summary['instructions'], summary['tools']),
                         ('synthetic-small', 'Summarize.', []))
        self.assertTrue(summary['prompt_cache_key'].endswith('-summary'))
        self.assertIsNotNone(client.request('resume', bot='Bob')['result']['compaction'])
        self.assertEqual(self.request_of_its_own(client), self.own_bytes(summary))
