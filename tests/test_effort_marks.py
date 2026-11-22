"""A turn at another effort than its bot's, on a model that takes a change
of effort inside the conversation, keeps the bot's effort on the request
and says the change where the turn starts, so the cache holds."""
import os
import unittest

from bench.runtime_client import Client
from bench.targets import clean_env
from tests.test_runtime import AnthropicModel, Model, ModelFixture

RUNTIME = unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1',
                              'set AGENT_TEST_RUNTIME=1 after a Rust release build')


class Turns:
    def turn(self, client, request_id, **effort):
        turn = client.request('submit', bot='Bob', request_id=request_id, prompt=request_id,
                              **effort)['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        return self.model.requests.get(timeout=3)


@RUNTIME
class ClaudeEffortMarkTests(Turns, ModelFixture):
    handler = AnthropicModel

    def anthropic(self, model):
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell', provider='anthropic', family='anthropic',
                        model=model, key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'})
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path), effort='high'))
        return client

    def test_a_boosted_turn_keeps_the_bots_effort_and_marks_where_it_starts(self):
        client = self.anthropic('claude-opus-5-5')
        plain = self.turn(client, 'one')
        self.assertEqual(plain['output_config'], {'effort': 'high'})
        self.assertNotIn('system', [m['role'] for m in plain['messages']])
        boosted = self.turn(client, 'two', effort='max')
        self.assertEqual(boosted['output_config'], {'effort': 'high'})
        messages = boosted['messages']
        self.assertEqual(messages[-2], {'role': 'system', 'content': [], 'output_config': {'effort': 'max'}})
        self.assertEqual(messages[-1]['role'], 'user')
        # What the plain turn sent is still the boosted one's start.
        self.assertEqual(messages[:len(plain['messages'])], plain['messages'])
        back = self.turn(client, 'three')
        self.assertEqual(back['output_config'], {'effort': 'high'})
        self.assertEqual([m['output_config']['effort'] for m in back['messages'] if m['role'] == 'system'],
                         ['max', 'high'])
        self.assertEqual(back['messages'][-2]['role'], 'system')
        # The boosted turn's request is the start of the next one's: the
        # change stays where it was said.
        self.assertEqual(back['messages'][:len(messages)], messages)

    def test_a_model_without_it_runs_the_turn_at_its_own_effort(self):
        client = self.anthropic('synthetic-claude')
        self.turn(client, 'one')
        boosted = self.turn(client, 'two', effort='max')
        self.assertEqual(boosted['output_config'], {'effort': 'max'})
        back = self.turn(client, 'three')
        self.assertEqual(back['output_config'], {'effort': 'high'})
        self.assertNotIn('system', [m['role'] for m in back['messages']])


@RUNTIME
class ResponsesEffortMarkTests(Turns, ModelFixture):
    handler = Model

    def responses(self, model):
        self.model.models = (model,)
        client = Client(self.binary, self.path / 'state.sqlite', self.url, model=model)
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path), effort='high'))
        return client

    def test_a_boosted_turn_keeps_the_bots_effort_and_updates_the_configuration(self):
        client = self.responses('gpt-6-sol')
        plain = self.turn(client, 'one')
        self.assertEqual(plain['reasoning']['effort'], 'high')
        boosted = self.turn(client, 'two', effort='xhigh')
        self.assertEqual(boosted['reasoning']['effort'], 'high')
        self.assertEqual(boosted['input'][-2], {'type': 'configuration_update', 'reasoning': {'effort': 'xhigh'}})
        self.assertEqual(boosted['input'][-1]['role'], 'user')
        self.assertEqual(boosted['input'][:len(plain['input'])], plain['input'])
        back = self.turn(client, 'three')
        self.assertEqual(back['reasoning']['effort'], 'high')
        self.assertEqual([i['reasoning']['effort'] for i in back['input'] if i.get('type') == 'configuration_update'],
                         ['xhigh', 'high'])
        self.assertEqual(back['input'][:len(boosted['input'])], boosted['input'])

    def test_an_earlier_model_runs_the_turn_at_its_own_effort(self):
        client = self.responses('synthetic-model')
        self.turn(client, 'one')
        boosted = self.turn(client, 'two', effort='xhigh')
        self.assertEqual(boosted['reasoning']['effort'], 'xhigh')
        self.assertFalse([i for i in boosted['input'] if i.get('type') == 'configuration_update'])


if __name__ == '__main__':
    unittest.main()
