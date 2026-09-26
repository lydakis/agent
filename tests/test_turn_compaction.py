"""Compaction inside a running turn: one long task summarizes its own
earlier rounds and finishes, its prompt kept whole."""
import json
import os
from unittest import skipUnless
from tests.test_runtime import AnthropicModel, ModelFixture
from tests.test_elision import drain, encoded
from bench.runtime_client import Client
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
        client = self.client(tools='shell,read', extra=('--context-bytes', '24576'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read'],
                       compaction_instructions='Summarize.')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:40')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = client.request('item', bot='Bob', node=ended['data']['checkpoint'])['result']
        self.assertIn('done after 40 rounds', json.dumps(answer))
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['input']) <= 24576 for r in requests))
        work = [r for r in requests if r.get('instructions') != 'Summarize.']
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
                        extra=('--context-bytes', '24576'))
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path), reasoning='low',
                                               compaction_instructions='Summarize.'))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='long:30')['result']['turn']
        ended = client.finished(turn)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        answer = client.request('item', bot='Bob', node=ended['data']['checkpoint'])['result']
        self.assertIn('done after 30 rounds', json.dumps(answer))
        self.assertEqual(self.model.binding_errors, [])
        requests = drain(self.model)
        self.assertTrue(all(encoded(r['messages']) <= 24576 for r in requests))
        summaries = [r for r in requests if r.get('system', [{}])[0].get('text') == 'Summarize.']
        self.assertGreaterEqual(len(summaries), 2)
        compacted = [e['data'] for e in all_events(client, 'Bob') if e['event'] == 'compacted']
        self.assertTrue(compacted and all(c['pinned'] for c in compacted))
