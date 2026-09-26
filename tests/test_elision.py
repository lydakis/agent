"""Tool-result elision: one long turn reclaims its own growing context."""
import json
import os
from unittest import skipUnless
from tests.test_runtime import AnthropicModel, ModelFixture, is_summary
from bench.runtime_client import Client
from bench.targets import clean_env

STUB = '[tool result elided from this request:'


def encoded(items):
    return len(json.dumps(items, separators=(',', ':'), ensure_ascii=False).encode()) - 2


def plain(value):
    """A request without its cache markers, which may move freely."""
    if isinstance(value, dict):
        return {k: plain(v) for k, v in value.items() if k != 'cache_control'}
    if isinstance(value, list):
        return [plain(v) for v in value]
    return value


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
        # Half the budget kept verbatim. Thirty-six small results, too small
        # to elide, then two large ones: the turn overflows while both large
        # results, answered, are still inside that tail.
        client = self.client(tools='shell,read', extra=('--context-bytes', '65536', '--compact-keep', '50'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'])
        turn = client.request('submit', bot='Bob', request_id='1',
                              prompt='long:36x40,2x600,10x40')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 65536 for r in requests))
        last = {i['call_id']: i['output'] for i in requests[-1]['input']
                if i.get('type') == 'function_call_output'}
        self.assertTrue(last['long-36'].startswith(STUB) and last['long-37'].startswith(STUB))
        self.assertFalse(last['long-47'].startswith(STUB))

    def test_a_turn_that_fits_only_without_its_note_elides_answered_results(self):
        # An 8000-byte carry-forward note goes ahead of the turn. Small
        # rounds, too small to elide, then two results of about 4.5 KiB
        # inside the kept half: the turn alone fits the budget, but not
        # beside the note, and ordinary elision finds nothing to move.
        client = self.client(tools='shell,read,note',
                             extra=('--context-bytes', '24576', '--compact-keep', '50'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read', 'note'])
        self.model.note_text = 'N' * 8000
        noted = client.request('submit', bot='Bob', request_id='1', prompt='note:')['result']['turn']
        self.assertEqual(client.finished(noted)['data']['status'], 'completed')
        del self.model.note_text
        drain(self.model)
        turn = client.request('submit', bot='Bob', request_id='2',
                              prompt='long:21x1,2x220,4x1')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in requests))
        self.assertTrue(all(any(i.get('role') == 'user' and i['content'][0]['text'].startswith(
            '[carry-forward note') for i in r['input']) for r in requests))
        last = {i['call_id']: i['output'] for i in requests[-1]['input']
                if i.get('type') == 'function_call_output'}
        self.assertTrue(last['long-21'].startswith(STUB) and last['long-22'].startswith(STUB))
        self.assertIn('round 21 line 150', last['long-read'])
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual([e['data']['results'] for e in events if e['event'] == 'elided'], [2])

    def test_a_boundary_that_elided_and_still_cannot_fit_beside_its_note_elides_again(self):
        # An 8000-byte note goes ahead of the turn. At the boundary after
        # the third large result, ordinary elision stubs the first, outside
        # the kept half, but the turn still cannot fit beside the note; the
        # forced move goes on to the second at the same head. Without a
        # summarizer, a refused second move ended the turn with
        # `context_limit`.
        client = self.client(tools='shell,read,note',
                             extra=('--context-bytes', '24576', '--compact-keep', '50'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read', 'note'])
        self.model.note_text = 'N' * 8000
        noted = client.request('submit', bot='Bob', request_id='1', prompt='note:')['result']['turn']
        self.assertEqual(client.finished(noted)['data']['status'], 'completed')
        del self.model.note_text
        drain(self.model)
        turn = client.request('submit', bot='Bob', request_id='2',
                              prompt='long:14x1,1x140,1x380,1x150,2x1')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in drain(self.model)))
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        moves = [e['data'] for e in events if e['event'] == 'elided']
        self.assertEqual(len(moves), 2)
        self.assertEqual(moves[0]['version'], moves[1]['version'])
        self.assertEqual(moves[0]['previous'], moves[1]['previous'])
        self.assertLess(moves[0]['through'], moves[1]['through'])

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
        summaries = [r for r in requests if is_summary(r)]
        self.assertEqual(len(summaries), 1)
        # The summarizer reads the span as the model last saw it.
        span = [i for i in summaries[0]['input'] if i.get('type') == 'function_call_output']
        self.assertEqual(len(span), 11)  # ten rounds and the read
        self.assertTrue(any(i['output'].startswith(STUB) for i in span))
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertEqual([c['covered_turns'] for c in compacted], [[1, 1]])
        self.assertFalse([e for e in events if e['event'] == 'compaction_failed'])

    def test_a_stub_read_back_pages_within_the_room_beside_the_turn(self):
        # One result of about 23 KiB in 24 KiB goes as its stub, and the
        # model reads it back with the default line limit. Whole, with its
        # escaping as a result, the page would not fit beside its call; it
        # stops where it fits.
        client = self.client(tools='shell,read', extra=('--context-bytes', '24576'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'])
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:1x1200,2x1')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in requests))
        read = next(i['output'] for i in requests[-1]['input']
                    if i.get('type') == 'function_call_output' and i['call_id'] == 'long-read')
        self.assertTrue(read.startswith('     1\t{"exit_code":0'), read[:40])
        self.assertIn('; continue with offset=', read)
        self.assertNotIn('round 0 line 1200', read)

    def test_a_paced_summary_copies_the_same_call_when_it_resumes(self):
        # One boundary stubs the first result, then summarizes a copy of the
        # call made before the stub, and that summary is paced once; the
        # daemon restarts while it waits. The retry copies the same call,
        # not the view after the stub.
        client = self.client(tools='shell,read', extra=('--context-bytes', '65536'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'],
                       compaction_instructions='Summarize.')
        self.model.compaction_refusals = 1
        turn = client.request('submit', bot='Bob', request_id='1',
                              prompt='long:1x600,35x40,1x700,2x1')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn)
        client.close(kill=True)
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        extra=('--context-bytes', '65536'), tools='shell,read')
        self.addCleanup(client.close)
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        moves = [e['data'] for e in events if e['event'] in ('elided', 'compacted')]
        self.assertEqual([m['version'] for m in moves[:2]], [moves[0]['version']] * 2)
        summaries = [r for r in drain(self.model) if is_summary(r)]
        self.assertEqual(len(summaries), 2)
        first, retry = summaries
        self.assertTrue(first.get('tools'))
        self.assertFalse(any(i.get('type') == 'function_call_output' and i['output'].startswith(STUB)
                             for i in first['input']))
        self.assertEqual((retry['input'], retry.get('tools')), (first['input'], first.get('tools')))

    def test_a_steer_the_turn_had_no_room_for_goes_in_once_elision_makes_some(self):
        # At a round's end the turn holds two whole results of about 11 KiB,
        # more than the three quarters of 24 KiB a steer may join. The next
        # boundary stubs the older one, and the steer goes in there, before
        # the model's next call, rather than failing when the turn ends.
        client = self.client(tools='shell,read', extra=('--context-bytes', '24576'))
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path),
                                               tools=['shell', 'read'], compaction_instructions='Summarize.'))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:16')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn
                       and m.get('data', {}).get('call_id') == 'long-1')
        correction = 'steer:' + 'x' * 1000
        steer = client.request('submit', bot='Bob', request_id='s', prompt=correction,
                               delivery='steer', expected_turn=turn)['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        outcome = client.finished(steer)['data']
        self.assertEqual((outcome['status'], outcome.get('into')), ('steered', turn), outcome)
        work = [r for r in drain(self.model) if not is_summary(r)]
        steered = [n for n, r in enumerate(work)
                   if any(i.get('role') == 'user' and i['content'][0]['text'] == correction for i in r['input'])]
        first = work[steered[0]]['input']
        at = next(n for n, i in enumerate(first) if i.get('role') == 'user' and i['content'][0]['text'] == correction)
        # It follows a result the model has not answered yet, and the
        # request that first carries it stubs more than the one before.
        self.assertEqual((first[at - 1]['type'], first[at - 1]['output'].startswith(STUB)),
                         ('function_call_output', False))
        stubs = lambda r: sum(i.get('type') == 'function_call_output' and i['output'].startswith(STUB)
                              for i in r['input'])
        self.assertGreater(stubs(work[steered[0]]), stubs(work[steered[0] - 1]))
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in work))

    def test_a_steer_is_admitted_only_with_room_beside_the_summary(self):
        # A large summary and a large steer: once a cut shrinks the turn,
        # the steer would fit the turn's three quarters on its own, but not
        # beside the summary sent ahead of it. It stays queued rather than
        # pushing the view over the budget, and the task finishes.
        self.model.compaction_text = 'S' * 7800
        client = self.client(tools='shell,read', extra=('--context-bytes', '24576'))
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path),
                                               tools=['shell', 'read'], compaction_instructions='Summarize.'))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:40x40')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn
                       and m.get('data', {}).get('call_id') == 'long-10')
        steer = client.request('submit', bot='Bob', request_id='s', prompt='steer:' + 'x' * 11000,
                               delivery='steer', expected_turn=turn)['result']['turn']
        ended = client.finished(turn, timeout=30)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        self.assertEqual(client.finished(steer)['data']['error'], 'stale_turn')
        work = [r for r in drain(self.model) if not is_summary(r)]
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in work))
        # Each cut left the turn room for the steer on its own.
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertGreaterEqual(len(compacted), 2)
        for data in compacted:
            self.assertEqual(data['summary_bytes'], 7800)
            self.assertLessEqual(data['context_after']['bytes'] - 7800 + 11000, 24576 // 4 * 3)

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
        # A move changes the request from its first new stub on. Everything
        # before it, thinking included, is sent as the previous request sent
        # it, so the provider's cache and the model's reasoning both hold.
        moves = [n for n in range(1, len(requests)) if counts[n] > counts[n - 1]]
        self.assertGreaterEqual(len(moves), 2)
        for n in moves:
            before, after = (plain(requests[n - 1]['messages']), plain(requests[n]['messages']))
            first = next(i for i, (a, b) in enumerate(zip(before, after)) if a != b)
            self.assertGreater(stubbed({'messages': [after[first]]}),
                               stubbed({'messages': [before[first]]}))
            self.assertGreater(thinking({'messages': after[:first]}), 0)
