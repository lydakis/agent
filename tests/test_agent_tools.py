"""An agent's own tools for other agents: send_message, list_agents and
pause_agents, made as the calling bot."""
import json
import os
import unittest

from tests.test_runtime import ModelFixture
from bench.runtime_client import node_item

TOOLS = 'echo,wait,send_message,list_agents,pause_agents'


def calls(*called):
    """A prompt the test model answers with these calls, in one response."""
    return 'calls:' + json.dumps([[name, arguments] for name, arguments in called])


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AgentToolsTests(ModelFixture):
    def output(self, client, bot, call_id):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        node = [e for e in events if e['event'] == 'tool_completed' and e['data']['call_id'] == call_id][-1]['data']['node']
        return node_item(client, bot, node)['result']['output']

    def run_calls(self, client, bot, request_id, *called):
        turn = client.request('submit', bot=bot, request_id=request_id, prompt=calls(*called))['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        return turn

    def test_a_message_reads_as_the_calling_turn_and_its_retry_sends_nothing_new(self):
        client = self.client(TOOLS)
        for bot in ('shop.lead', 'shop.fix'):
            client.request('create', bot=bot, workspace=str(self.path))
        turn = self.run_calls(client, 'shop.lead', 'go',
                              ('send_message', {'to': 'shop.fix', 'message': 'hi'}))
        sent = json.loads(self.output(client, 'shop.lead', 'send_message-1'))
        self.assertEqual(sent['status'], 'running', sent)
        bot, message = sent['handle'].rsplit('/', 1)
        self.assertEqual(bot, 'shop.fix')
        client.finished(int(message))
        listed = client.request('messages', bot='shop.fix', after=0)['result']['messages']
        self.assertEqual(listed[0]['from'], {'bot': 'shop.lead', 'turn': turn}, listed[0])
        self.assertEqual(listed[0]['delivery'], 'steer')
        # The same call made again sends nothing new: its id is the call's.
        again = client.request('submit', bot='shop.fix', request_id=f'tool-{turn}-send_message-1',
                               prompt='hi', delivery='steer', **{'from': {'bot': 'shop.lead', 'turn': turn}})
        self.assertTrue(again['result']['duplicate'], again)

    def test_a_message_may_not_interrupt_an_agent_the_caller_did_not_make(self):
        client = self.client(TOOLS)
        for bot in ('shop.lead', 'shop.other'):
            client.request('create', bot=bot, workspace=str(self.path))
        busy = client.request('submit', bot='shop.other', request_id='busy', prompt='wait')['result']['message']
        self.run_calls(client, 'shop.lead', 'go',
                       ('send_message', {'to': 'shop.other', 'message': 'stop', 'interrupt': True}))
        refused = self.output(client, 'shop.lead', 'send_message-1')
        self.assertIn('not_permitted', refused)
        self.assertIn('not an agent you made', refused)
        client.request('interrupt', bot='shop.other', turn=busy)

    def test_an_interrupt_stops_the_turn_of_an_agent_the_caller_made_and_queues_the_message(self):
        client = self.client(TOOLS)
        lead = client.request('create', bot='shop.lead', workspace=str(self.path))['result']
        client.request('create', bot='shop.fix', workspace=str(self.path),
                       created_by='shop.lead', created_by_id=lead['bot_id'])
        busy = client.request('submit', bot='shop.fix', request_id='busy', prompt='wait')['result']['message']
        self.model.requests.get(timeout=3)
        self.run_calls(client, 'shop.lead', 'go',
                       ('send_message', {'to': 'shop.fix', 'message': 'do this instead', 'interrupt': True}))
        sent = json.loads(self.output(client, 'shop.lead', 'send_message-1'))
        self.assertEqual(sent['interrupted_turn'], busy, sent)
        self.assertEqual(client.finished(busy)['data']['status'], 'interrupted')
        message = int(sent['handle'].rsplit('/', 1)[1])
        self.assertEqual(client.finished(message)['data']['status'], 'completed')
        listed = {m['message']: m for m in client.request('messages', bot='shop.fix', after=0)['result']['messages']}
        self.assertEqual(listed[message]['delivery'], 'queue')

    def test_agents_are_listed_by_project_and_the_calls_waiting_on_approval(self):
        client = self.client(TOOLS)
        for bot in ('shop.lead', 'shop.fix', 'web.lead'):
            client.request('create', bot=bot, workspace=str(self.path))
        self.run_calls(client, 'shop.lead', 'go',
                       ('list_agents', {}),
                       ('list_agents', {'name': '*.lead'}),
                       ('list_agents', {'approvals': True}))
        own = json.loads(self.output(client, 'shop.lead', 'list_agents-1'))
        self.assertEqual([a['name'] for a in own['agents']], ['shop.fix', 'shop.lead'])
        self.assertEqual(own['agents'][1]['status'], 'running')
        self.assertTrue(own['agents'][1]['running_turn'])
        self.assertIn('folder', own['agents'][0])
        leads = json.loads(self.output(client, 'shop.lead', 'list_agents-2'))
        self.assertEqual([a['name'] for a in leads['agents']], ['shop.lead', 'web.lead'])
        self.assertEqual(json.loads(self.output(client, 'shop.lead', 'list_agents-3'))['approvals'], [])

    def test_the_cord_names_the_agent_that_pulled_it(self):
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path))
        turn = client.request('submit', bot='shop.lead', request_id='go',
                              prompt=calls(('pause_agents', {'reason': 'emails going out'})))['result']['message']
        paused = client.receive(lambda m: m.get('event') == 'paused')['pause']
        self.assertEqual((paused['reason'], paused['by']), ('emails going out', 'shop.lead'))
        client.request('resume')
        client.finished(turn)
