"""Tool-result elision: one long turn reclaims its own growing context."""
import json
import os
from unittest import skipUnless
from tests.test_runtime import AnthropicModel, ModelFixture
from bench.runtime_client import Client
from bench.targets import clean_env

STUB = '[tool result elided from this request:'


def encoded(items):
    return len(json.dumps(items, separators=(',', ':'), ensure_ascii=False).encode()) - 2


def drain(model):
    out = []
    while not model.requests.empty():
        out.append(model.requests.get())
    return out


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class ElisionTests(ModelFixture):
    def test_a_long_turn_outgrows_its_budget_by_eliding_answered_results(self):
        # About 11 KiB a round for 24 rounds: four budgets of tool output in
        # one turn, with no summarizer configured.
        client = self.client(tools='shell,read', extra=('--context-bytes', '65536'))
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path),
                                               tools=['shell', 'read']))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:24')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        requests = drain(self.model)
        self.assertEqual(len(requests), 26)  # 24 shell rounds, the read, the answer
        self.assertTrue(all(encoded(r['input']) <= 65536 for r in requests))
        # The model sees every result whole in the request that answers it.
        for request in requests[1:]:
            newest = request['input'][-1]
            self.assertEqual(newest['type'], 'function_call_output')
            self.assertFalse(newest['output'].startswith(STUB))
        last = requests[-1]['input']
        results = [i for i in last if i.get('type') == 'function_call_output']
        stubs = [i for i in results if i['output'].startswith(STUB)]
        self.assertGreaterEqual(len(stubs), 18)
        # Every call and result keeps its place and call id.
        self.assertEqual([i['call_id'] for i in last if i.get('type') == 'function_call'],
                         [f'long-{n}' for n in range(24)] + ['long-read'])
        self.assertEqual([i['call_id'] for i in results], [f'long-{n}' for n in range(24)] + ['long-read'])
        # What a stub names reads back whole, middle included.
        read = results[-1]['output']
        self.assertNotIn('round 0 line 300', stubs[0]['output'])
        self.assertIn('round 0 line 300', read)
        # Each move is a durable event, and the floor only moves forward.
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        moves = [e['data'] for e in events if e['event'] == 'elided']
        self.assertGreaterEqual(len(moves), 3)
        self.assertEqual([m['through'] for m in moves], sorted({m['through'] for m in moves}))
        self.assertEqual(sum(m['results'] for m in moves), len(stubs))
        # The stored transcript itself keeps every result whole.
        node = next(e['data']['node'] for e in events
                    if e['event'] == 'tool_completed' and e['data']['call_id'] == 'long-0')
        self.assertIn('round 0 line 300', client.request('item', bot='Bob', node=node)['result']['output'])
        # A historical fork from inside the turn sees what the source saw
        # there, and continues on its own.
        fork_at = next(e['data']['node'] for e in events
                       if e['event'] == 'tool_completed' and e['data']['call_id'] == 'long-11')
        self.assertIn('result', client.request('fork', source='Bob', bot='Branch', checkpoint=fork_at,
                                               workspace=str(self.path)))
        bound = [m['version'] for m in moves if m['version'] <= fork_at]
        self.assertEqual(client.request('resume', bot='Branch')['result']['elision'],
                         bound[-1] if bound else None)
        turn = client.request('submit', bot='Branch', request_id='2', prompt='long:14')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        branch = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 65536 for r in branch))

    def test_a_turn_over_budget_elides_answered_results_inside_the_keep_target(self):
        # Half the budget kept verbatim. Forty small results, too small to
        # elide, then two large ones: the turn overflows while both large
        # results, answered, are still inside that tail.
        client = self.client(tools='shell,read', extra=('--context-bytes', '65536', '--compact-keep', '50'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'])
        turn = client.request('submit', bot='Bob', request_id='1',
                              prompt='long:40x40,2x600,10x40')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 65536 for r in requests))
        last = {i['call_id']: i['output'] for i in requests[-1]['input']
                if i.get('type') == 'function_call_output'}
        self.assertTrue(last['long-40'].startswith(STUB) and last['long-41'].startswith(STUB))
        self.assertFalse(last['long-51'].startswith(STUB))

    def test_a_bot_without_read_never_elides(self):
        # A stub names a read the model could not make.
        client = self.client(tools='shell,read', extra=('--context-bytes', '65536'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'])
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:24')['result']['turn']
        ended = client.finished(turn)
        self.assertNotEqual(ended['data']['status'], 'completed', ended)
        self.assertIn('context_limit', json.dumps(ended['data']))
        requests = drain(self.model)
        self.assertFalse([i for r in requests for i in r['input']
                          if i.get('type') == 'function_call_output' and i['output'].startswith(STUB)])
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertFalse([e for e in events if e['event'] == 'elided'])

    def test_compaction_summarizes_elided_results_as_their_stubs(self):
        # Two long turns in 32 KiB: the second compacts the first, whose
        # results as stored are several budgets but as sent are stubs.
        client = self.client(tools='shell,read', extra=('--context-bytes', '32768'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'],
                       compaction_instructions='Summarize.')
        for n in range(2):
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt='long:10')['result']['turn']
            ended = client.finished(turn)
            self.assertEqual(ended['data']['status'], 'completed', ended)
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 32768 for r in requests))
        summaries = [r for r in requests if r.get('instructions') == 'Summarize.']
        self.assertEqual(len(summaries), 1)
        # The summarizer reads the span as the model last saw it.
        span = [i for i in summaries[0]['input'] if i.get('type') == 'function_call_output']
        self.assertEqual(len(span), 11)  # ten rounds and the read
        self.assertTrue(any(i['output'].startswith(STUB) for i in span))
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertEqual([c['covered_turns'] for c in compacted], [[1, 1]])
        self.assertFalse([e for e in events if e['event'] == 'compaction_failed'])


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class AnthropicElisionTests(ModelFixture):
    handler = AnthropicModel

    def test_stubs_keep_replayed_thinking_bound_to_what_the_model_saw(self):
        # The synthetic endpoint refuses any replayed thinking block whose
        # earlier conversation changed, as the strict check does.
        self.model.bind_thinking = True
        self.model.binding_errors = []
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell,read', provider='anthropic', family='anthropic',
                        model='synthetic-claude', key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'},
                        extra=('--context-bytes', '65536'))
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path), reasoning='low'))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:16')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        self.assertEqual(self.model.binding_errors, [])
        requests = drain(self.model)
        self.assertEqual(len(requests), 17)
        self.assertTrue(all(encoded(r['messages']) <= 65536 for r in requests))

        def stubbed(request):
            return sum(isinstance(b.get('content'), str) and b['content'].startswith(STUB)
                       for m in request['messages'] for b in m['content'])

        def thinking(request):
            return sum(b['type'] == 'thinking' for m in request['messages'] for b in m['content'])

        counts = [stubbed(r) for r in requests]
        self.assertGreater(counts[-1], 0)
        # Thinking written after a move is replayed until the next one.
        self.assertTrue(any(stubbed(r) and thinking(r) for r in requests))
