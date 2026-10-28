"""Reading another agent's conversation with the history tool."""
import json
import os
import unittest

from bench.runtime_client import node_item
from tests.test_runtime import ModelFixture


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class SiblingTests(ModelFixture):
    def output(self, client, bot, call_id):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        done = [e for e in events if e['event'] == 'tool_completed' and e['data']['call_id'] == call_id][-1]
        return json.loads(node_item(client, bot, done['data']['node'])['result']['output'])

    def test_an_agent_lists_and_reads_another_agents_turns(self):
        client = self.client('echo,history')
        for bot in ('Ann', 'Bob'):
            client.request('create', bot=bot, workspace=str(self.path))
        turns = []
        for n, prompt in enumerate(('first look', 'second look')):
            turns.append(client.request('submit', bot='Ann', request_id=str(n), prompt=prompt)['result']['turn'])
            self.assertEqual(client.finished(turns[-1])['data']['status'], 'completed')
        self.model.call_script = [('history', {'bot': 'Ann'}), ('history', {'bot': 'Ann', 'turn': 1}),
                                  ('history', {'bot': 'Nobody'}), ('history', {})]
        turn = client.request('submit', bot='Bob', request_id='b', prompt='script')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        # Newest first, numbered as Ann's history numbers them.
        listed = self.output(client, 'Bob', 'script-0')
        self.assertEqual(listed['bot'], 'Ann')
        self.assertEqual([(t['turn'], t['handle'], t['status'], t['prompt']) for t in listed['turns']],
                         [(2, f'turn:Ann/{turns[1]}', 'completed', 'second look'),
                          (1, f'turn:Ann/{turns[0]}', 'completed', 'first look')])
        page = self.output(client, 'Bob', 'script-1')
        self.assertEqual((page['turn'], page['done']), (1, True))
        self.assertIn('first look', page['text'])
        self.assertNotIn('second look', page['text'])
        self.assertEqual(self.output(client, 'Bob', 'script-2')['error'], 'bot_not_found')
        # Its own turns are in its context: only a turn of them is read.
        self.assertEqual(self.output(client, 'Bob', 'script-3')['error'], 'invalid_tool_arguments')


if __name__ == '__main__':
    unittest.main()
