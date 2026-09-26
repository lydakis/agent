"""Quality scores must use complete records and honest context boundaries."""
import os
import queue
import unittest
from unittest.mock import patch

from bench.context_eval import COMPACTION, INSTRUCTIONS, MARKER, context_state, page_rows, run_condition, summarize
from bench.targets import clean_env
from tests.test_runtime import ModelFixture


class PagedClient:
    def __init__(self, op, rows, page_size):
        self.op, self.rows, self.page_size = op, rows, page_size
        self.calls = []

    def request(self, op, **params):
        assert op == self.op
        self.calls.append(params['after'])
        key = 'cursor' if op == 'events' else 'turn'
        pending = [r for r in self.rows if r[key] > params['after']]
        rows = pending[:min(params['limit'], self.page_size)]
        after = rows[-1][key] if rows else params['after']
        cursor = {'next_cursor': after} if op == 'events' else {
            'next_after': after if len(pending) > len(rows) else None}
        return {'result': {op: rows, **cursor}}


class ContextEvalTests(unittest.TestCase):
    def test_bots_get_the_clients_current_policy_text(self):
        # Read from client/src/policy.rs, so an edit there reaches the bench.
        self.assertTrue(INSTRUCTIONS.startswith('To delegate a subtask'))
        self.assertIn('contact your creator only to ask something you need', INSTRUCTIONS)
        self.assertIn('"$AGENT_BIN" run --detach', INSTRUCTIONS)
        self.assertTrue(COMPACTION.endswith('Reply with the summary only.'))
        self.assertFalse(any('\\' in text or '\n' in text or '  ' in text for text in (INSTRUCTIONS, COMPACTION)))

    def test_history_beyond_first_page_and_short_byte_limited_pages(self):
        rows = [{'cursor': i, 'turn': 1, 'event': 'message', 'data': {}} for i in range(1, 257)]
        rows.append({'cursor': 257, 'turn': 14, 'event': 'tool_started', 'data': {'name': 'history'}})
        for page_size in (256, 31):
            with self.subTest(page_size=page_size):
                client = PagedClient('events', rows, page_size)
                events = list(page_rows(client, 'events', 'Bob'))
                self.assertEqual(events, rows)
                self.assertEqual(sum(e['data'].get('name') == 'history' for e in events), 1)
                self.assertEqual(list(page_rows(client, 'events', 'Bob', after=257)), [])

    def test_usage_includes_turns_beyond_first_page(self):
        rows = [{'turn': i * 3, 'input_tokens': i, 'output_tokens': 1} for i in range(1, 71)]
        client = PagedClient('turns', rows, 64)
        turns = list(page_rows(client, 'turns', 'Bob'))
        self.assertEqual(sum(t['input_tokens'] for t in turns), sum(range(1, 71)))
        self.assertEqual(sum(t['output_tokens'] for t in turns), 70)
        self.assertEqual(client.calls, [0, 192])

    def test_context_crossing_during_action_is_not_scored_as_omitted(self):
        self.assertEqual(context_state(0, 1, 'completed'), 'transition')
        self.assertEqual(context_state(1, 3, 'completed'), 'omitted')
        self.assertEqual(context_state(0, 0, 'completed'), 'retained')
        self.assertEqual(context_state(1, 1, 'failed'), 'unknown')

    def test_summary_separates_context_conditions_and_missing_files(self):
        def bot(state, honored):
            return {'files': [{'turn': 1, 'honored': honored, 'context': state}],
                    'final': {'honored': honored, 'context': state}, 'history_calls_final': 0,
                    'history_calls': 1, 'input_tokens': 2, 'output_tokens': 3,
                    'omitted_turns_after_final': 1}
        block = {'condition': 'omitted', 'wall_s': 1,
                 'bots': {str(i): bot(state, honored) for i, (state, honored) in enumerate([
                     ('omitted', True), ('omitted', None), ('transition', True), ('unknown', False)])}}
        result = summarize(block)
        self.assertEqual(result['final_honored_by_context']['omitted'], '1/2')
        self.assertEqual(result['final_honored_by_context']['transition'], '1/1')
        self.assertEqual(result['final_missing_file'], 1)
        self.assertEqual(result['history_used_any_turn'], '4/4')


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary and loopback')
class ContextEvalRuntimeTests(ModelFixture):
    def test_end_to_end_distinguishes_boundary_crossing_from_later_omission(self):
        rule = 'RULE_SENTINEL ' + 'x' * 2500
        # The tool output pushes the window over its bound only AFTER the
        # model has issued the action. No real provider or paid call is used.
        action = f"shell:printf '%s\\n' '{MARKER}' > summary.txt; printf '%s' '" + 'y' * 1100 + "'"
        for fillers, expected in ((0, 'transition'), (1, 'omitted')):
            with self.subTest(fillers=fillers), patch('bench.context_eval.RULE', rule), \
                    patch('bench.context_eval.filler', return_value=action.replace('summary.txt', 'item_1.txt')), \
                    patch('bench.context_eval.FINAL', action if not fillers else
                          f"shell:printf '%s\\n' '{MARKER}' > summary.txt"):
                block = run_condition(self.binary, 'openai', 'responses', self.url, None,
                                      clean_env(), 'synthetic-model', 'omitted', 1, fillers,
                                      self.path, omitted_bytes=8192)
                final = block['bots']['omitted-0']['final']
                self.assertEqual(final['status'], 'completed')
                self.assertTrue(final['honored'])
                self.assertEqual(final['context'], expected)
                requests = []
                while True:
                    try:
                        requests.append(self.model.requests.get_nowait())
                    except queue.Empty:
                        break
                def has_rule(request):
                    return any(item.get('role') == 'user' and
                               item.get('content', [{}])[0].get('text', '').startswith('RULE_SENTINEL')
                               for item in request['input'])
                self.assertTrue(has_rule(requests[1]))
                self.assertFalse(has_rule(requests[2]))
                if fillers:
                    self.assertFalse(has_rule(requests[3]))


if __name__ == '__main__':
    unittest.main()
