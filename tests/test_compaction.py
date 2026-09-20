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
        self.assertTrue(any(m.get('event') == 'compaction_failed' and m.get('error') == 'compaction_span_limit'
                            for m in client.saved))
        self.assertEqual(len(client.request('turns', bot='Bob', after=0, limit=64)['result']['turns']), 24)

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
