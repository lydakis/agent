"""An agent's own tools for other agents: send_message, list_agents,
pause_agents, spawn_agent and close_agent, made as the calling bot."""
import hashlib
import json
import os
import shutil
import sys
import time
import unittest

from tests.test_runtime import ModelFixture
from bench.runtime_client import PROTECTING, Client, node_item
from bench.targets import clean_env

TOOLS = 'echo,wait,send_message,list_agents,pause_agents,spawn_agent,close_agent'


def calls(*called):
    """A prompt the test model answers with these calls, in one response."""
    return 'calls:' + json.dumps([[name, arguments] for name, arguments in called])


def grouped(bot, **settings):
    """A bot's settings as Sonny makes them: its namespace is its name up to the first dot."""
    return {'namespace': bot.split('.', 1)[0], **settings}


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
            client.request('create', bot=bot, workspace=str(self.path), settings=grouped(bot))
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
        # The same call made again sends nothing new: its id is the call's,
        # by a digest of the whole call id.
        call = hashlib.sha256(b'send_message-1').hexdigest()[:32]
        again = client.request('submit', bot='shop.fix', request_id=f'tool-{turn}-{call}',
                               prompt='hi', delivery='steer', **{'from': {'bot': 'shop.lead', 'turn': turn}})
        self.assertTrue(again['result']['duplicate'], again)

    def test_a_message_always_steers_and_names_no_delivery(self):
        client = self.client(TOOLS)
        for bot in ('shop.lead', 'shop.fix'):
            client.request('create', bot=bot, workspace=str(self.path), settings=grouped(bot))
        self.run_calls(client, 'shop.lead', 'go',
                       ('send_message', {'to': 'shop.fix', 'message': 'later', 'delivery': 'queue'}))
        self.assertIn('invalid_tool_arguments', self.output(client, 'shop.lead', 'send_message-1'))
        self.assertEqual(client.request('messages', bot='shop.fix', after=0)['result']['messages'], [])

    def test_a_message_may_not_interrupt_an_agent_the_caller_did_not_make(self):
        client = self.client(TOOLS)
        for bot in ('shop.lead', 'shop.other'):
            client.request('create', bot=bot, workspace=str(self.path), settings=grouped(bot))
        busy = client.request('submit', bot='shop.other', request_id='busy', prompt='wait')['result']['message']
        self.run_calls(client, 'shop.lead', 'go',
                       ('send_message', {'to': 'shop.other', 'message': 'stop', 'interrupt': True}))
        refused = self.output(client, 'shop.lead', 'send_message-1')
        self.assertIn('not_permitted', refused)
        self.assertIn('not made by this bot', refused)
        client.request('interrupt', bot='shop.other', turn=busy)

    def test_an_interrupt_stops_the_turn_of_an_agent_the_caller_made_and_queues_the_message(self):
        client = self.client(TOOLS)
        lead = client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))['result']
        client.request('create', bot='shop.fix', workspace=str(self.path), settings=grouped('shop.fix'),
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

    def test_a_message_still_in_line_is_replaced_and_one_taken_in_is_followed(self):
        import threading
        self.model.release_headers = threading.Event()
        self.model.all_streaming = threading.Event()
        self.model.all_streaming.set()
        self.addCleanup(self.model.release_headers.set)
        client = self.client(TOOLS)
        for bot in ('shop.lead', 'web.lead', 'shop.fix'):
            client.request('create', bot=bot, workspace=str(self.path), settings=grouped(bot))
        busy = client.request('submit', bot='shop.fix', request_id='busy', prompt='gate')['result']['message']
        self.model.requests.get(timeout=3)
        self.run_calls(client, 'shop.lead', 'first',
                       ('send_message', {'to': 'shop.fix', 'message': 'upload as picked'}))
        first = json.loads(self.output(client, 'shop.lead', 'send_message-1'))
        self.assertEqual(first['status'], 'queued', first)
        self.run_calls(client, 'shop.lead', 'second',
                       ('send_message', {'to': 'shop.fix', 'message': 'reversed order',
                                         'replaces': first['handle']}))
        second = json.loads(self.output(client, 'shop.lead', 'send_message-1'))
        self.assertEqual(second['replaced'], first['handle'], second)
        old, new = (int(sent['handle'].rsplit('/', 1)[1]) for sent in (first, second))
        self.assertEqual(client.finished(old)['data']['status'], 'withdrawn')
        # Another agent may not replace it, nor may it name another bot's.
        self.run_calls(client, 'web.lead', 'theirs',
                       ('send_message', {'to': 'shop.fix', 'message': 'mine instead',
                                         'replaces': second['handle']}),
                       ('send_message', {'to': 'web.lead', 'message': 'elsewhere',
                                         'replaces': second['handle']}))
        self.assertIn('not_permitted', self.output(client, 'web.lead', 'send_message-1'))
        self.assertIn('invalid_tool_arguments', self.output(client, 'web.lead', 'send_message-2'))
        listed = {m['message']: m for m in client.request('messages', bot='shop.fix', after=0)['result']['messages']}
        self.assertEqual(listed[old]['status'], 'withdrawn')
        self.assertEqual(len(listed), 3, listed)
        # Once taken in, by the running turn it steered, it stays, and the
        # new one follows it.
        self.model.release_headers.set()
        client.finished(busy)
        self.assertEqual(client.finished(new)['data']['status'], 'taken')
        self.run_calls(client, 'shop.lead', 'third',
                       ('send_message', {'to': 'shop.fix', 'message': 'one more thing',
                                         'replaces': second['handle']}))
        third = json.loads(self.output(client, 'shop.lead', 'send_message-1'))
        self.assertEqual((third['taken'], third['taken_turn']), (second['handle'], busy), third)
        client.finished(int(third['handle'].rsplit('/', 1)[1]))

    def test_agents_are_listed_by_project_and_the_calls_waiting_on_approval(self):
        client = self.client(TOOLS)
        for bot in ('shop.lead', 'shop.fix', 'web.lead'):
            client.request('create', bot=bot, workspace=str(self.path), settings=grouped(bot))
        self.run_calls(client, 'shop.lead', 'go',
                       ('list_agents', {}),
                       ('list_agents', {'name': '*.lead'}),
                       ('list_agents', {'approvals': True}))
        own = json.loads(self.output(client, 'shop.lead', 'list_agents-1'))
        self.assertEqual([a['name'] for a in own['bots']], ['shop.fix', 'shop.lead'])
        self.assertEqual(own['bots'][1]['status'], 'running')
        self.assertTrue(own['bots'][1]['running_turn'])
        self.assertIn('workspace', own['bots'][0])
        leads = json.loads(self.output(client, 'shop.lead', 'list_agents-2'))
        self.assertEqual([a['name'] for a in leads['bots']], ['shop.lead', 'web.lead'])
        self.assertEqual(json.loads(self.output(client, 'shop.lead', 'list_agents-3'))['approvals'], [])

    def test_a_call_waiting_on_a_gate_is_listed_with_its_arguments_and_tags(self):
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))
        client.request('create', bot='shop.fix', workspace=str(self.path), settings=grouped('shop.fix'), tools=['shell'],
                       approve=['shell'], approver='manual')
        held = client.request('submit', bot='shop.fix', request_id='held',
                              prompt=calls(('shell', {'command': 'echo held-call'})))['result']['message']
        client.receive(lambda m: m.get('event') == 'approval_requested' and m.get('turn') == held)
        self.run_calls(client, 'shop.lead', 'go', ('list_agents', {'approvals': True}))
        [waiting] = json.loads(self.output(client, 'shop.lead', 'list_agents-1'))['approvals']
        self.assertEqual((waiting['bot'], waiting['turn'], waiting['tool'], waiting['waiting_on']),
                         ('shop.fix', held, 'shell', ['manual']), waiting)
        self.assertIn('echo held-call', waiting['arguments'])
        client.request('interrupt', bot='shop.fix', turn=held)

    def test_the_cord_names_the_agent_that_pulled_it(self):
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))
        turn = client.request('submit', bot='shop.lead', request_id='go',
                              prompt=calls(('pause_agents', {'reason': 'emails going out'})))['result']['message']
        paused = client.receive(lambda m: m.get('event') == 'paused')['pause']
        self.assertEqual((paused['reason'], paused['by']), ('emails going out', 'shop.lead'))
        client.request('resume')
        client.finished(turn)
        # The tool's own session closed though the pause parked the turn
        # waiting on it: only this test's connection is left.
        for _ in range(50):
            if client.request('stats')['result']['sessions'] == 1:
                break
            time.sleep(0.05)
        self.assertEqual(client.request('stats')['result']['sessions'], 1)

    def test_an_agent_made_with_spawn_agent_works_in_the_callers_project_folder_and_tools(self):
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead', speed='fast'))
        turn = self.run_calls(client, 'shop.lead', 'go',
                              ('spawn_agent', {'name': 'review', 'message': 'look it over'}))
        made = json.loads(self.output(client, 'shop.lead', 'spawn_agent-1'))
        self.assertEqual(made['name'], 'shop.review', made)
        bot, message = made['handle'].rsplit('/', 1)
        self.assertEqual(bot, 'shop.review')
        self.assertEqual(client.finished(int(message))['data']['status'], 'completed')
        lead = client.request('inspect', bot='shop.lead')['result']
        review = client.request('inspect', bot='shop.review')['result']
        self.assertEqual((review['created_by'], review['workspace'], review['tools'], review['model']),
                         ('shop.lead', lead['workspace'], lead['tools'], lead['model']))
        # Its speed and namespace too, as its other settings.
        self.assertEqual((review['settings']['speed'], review['settings']['namespace']), ('fast', 'shop'),
                         review['settings'])
        listed = client.request('messages', bot='shop.review', after=0)['result']['messages']
        self.assertEqual(listed[0]['from'], {'bot': 'shop.lead', 'turn': turn}, listed[0])
        # The same call made again makes and sends nothing new.
        call = hashlib.sha256(b'spawn_agent-1').hexdigest()[:32]
        again = client.request('submit', bot='shop.review', request_id=f'tool-{turn}-{call}',
                               prompt='look it over', delivery='queue',
                               **{'from': {'bot': 'shop.lead', 'turn': turn}})
        self.assertTrue(again['result']['duplicate'], again)

    def test_a_whole_name_stays_whole_and_an_effort_asked_for_is_kept(self):
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))
        self.run_calls(client, 'shop.lead', 'go',
                       ('spawn_agent', {'name': 'other.look', 'message': 'q', 'effort': 'low'}))
        made = json.loads(self.output(client, 'shop.lead', 'spawn_agent-1'))
        self.assertEqual(made['name'], 'other.look', made)
        look = client.request('inspect', bot='other.look')['result']
        self.assertEqual((look['created_by'], look.get('effort')), ('shop.lead', 'low'), look)

    def test_a_new_agent_takes_what_run_new_takes(self):
        (self.path / 'AGENTS.md').write_text('Folder rule: tabs.\n')
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'), tools=TOOLS.split(','))
        self.run_calls(client, 'shop.lead', 'go',
                       ('spawn_agent', {'name': 'plain', 'message': 'q'}),
                       ('spawn_agent', {'name': 'told', 'message': 'q', 'agents': True}),
                       ('spawn_agent', {'name': 'set', 'message': 'q', 'instructions': 'Only this.',
                                        'tools': ['echo', 'wait'], 'settings': {'max_rounds': 3},
                                        'budget_tokens': 50000}),
                       ('spawn_agent', {'name': 'both', 'message': 'q', 'instructions': 'x', 'agents': True}),
                       ('spawn_agent', {'name': 'copy', 'message': 'q', 'fork': True, 'tools': ['echo']}),
                       ('spawn_agent', {'name': 'copy', 'message': 'q', 'fork': True, 'allow': ['echo']}))
        plain = client.request('inspect', bot='shop.plain')['result']
        told = client.request('inspect', bot='shop.told')['result']
        self.assertNotIn('Folder rule', plain['instructions'])
        self.assertIn('Folder rule: tabs.', told['instructions'])
        self.assertTrue(told['instructions'].startswith(plain['instructions'][:40]), told['instructions'][:200])
        kept = client.request('inspect', bot='shop.set')['result']
        self.assertEqual((kept['instructions'], kept['tools'], kept['settings']['max_rounds'], kept['budget_tokens']),
                         ('Only this.', ['echo', 'wait'], 3, 50000))
        self.assertIn('instructions sets them', self.output(client, 'shop.lead', 'spawn_agent-4'))
        self.assertIn('give it allow', self.output(client, 'shop.lead', 'spawn_agent-5'))
        copy = client.request('inspect', bot='shop.copy')['result']
        self.assertEqual(copy['allowed'], ['echo'], copy)

    def test_a_spawn_sent_again_after_its_folder_changed_keeps_the_agent_it_made(self):
        (self.path / 'AGENTS.md').write_text('First rule.\n')
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))
        spawn = ('spawn_agent', {'name': 'look', 'message': 'q', 'agents': True, 'request_id': 'once'})
        self.run_calls(client, 'shop.lead', 'go', spawn)
        (self.path / 'AGENTS.md').write_text('Second rule.\n')
        # Sent again from another turn: the agent the first call made stands
        # though its folder now composes otherwise; only the message, from
        # another turn, is refused as a different request.
        self.run_calls(client, 'shop.lead', 'again', spawn)
        events = client.request('events', bot='shop.lead', after=0, limit=256)['result']['events']
        node = [e for e in events if e['event'] == 'tool_completed'][-1]['data']['node']
        again = node_item(client, 'shop.lead', node)['result']['output']
        self.assertIn('from differs from the request that sent message', again)
        look = client.request('inspect', bot='shop.look')['result']
        self.assertIn('First rule.', look['instructions'])
        # One that sets differently what it set itself is another request.
        self.run_calls(client, 'shop.lead', 'other',
                       ('spawn_agent', {'name': 'look', 'message': 'q', 'agents': True,
                                        'request_id': 'once', 'budget_tokens': 9000}))
        events = client.request('events', bot='shop.lead', after=0, limit=256)['result']['events']
        node = [e for e in events if e['event'] == 'tool_completed'][-1]['data']['node']
        other = node_item(client, 'shop.lead', node)['result']['output']
        self.assertIn('budget_tokens differs from the request that made shop.look', other)

    def test_a_new_agent_takes_the_running_turns_own_effort(self):
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'), effort='low')
        turn = client.request('submit', bot='shop.lead', request_id='go', effort='high',
                              prompt=calls(('spawn_agent', {'name': 'look', 'message': 'q'})))['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        look = client.request('inspect', bot='shop.look')['result']
        self.assertEqual(look.get('effort'), 'high', look)

    def test_a_caller_kept_to_some_tools_makes_agents_kept_to_them(self):
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))
        client.request('fork', source='shop.lead', bot='shop.narrow', allow=['echo', 'spawn_agent'])
        self.run_calls(client, 'shop.narrow', 'go',
                       ('spawn_agent', {'name': 'helper', 'message': 'q'}),
                       ('spawn_agent', {'name': 'asked', 'message': 'q', 'tools': ['echo', 'send_message']}))
        helper = client.request('inspect', bot='shop.helper')['result']
        asked = client.request('inspect', bot='shop.asked')['result']
        self.assertEqual((helper['tools'], asked['tools']), (['echo', 'spawn_agent'], ['echo']))

    @unittest.skipUnless(shutil.which('bwrap') or sys.platform == 'darwin', 'sandboxes need bubblewrap')
    def test_a_profile_starts_its_agent_in_its_sandbox_and_never_looser(self):
        agents = self.path / '.agents' / 'agents'
        agents.mkdir(parents=True)
        (agents / 'reviewer.md').write_text('---\nsandbox: read-only\n---\nReview it.\n')
        (agents / 'careful.md').write_text('---\nextends: reviewer\n---\nBe careful.\n')
        (agents / 'loose.md').write_text('---\nsandbox: danger-full-access\n---\nDo anything.\n')
        client = self.client(TOOLS)
        made = client.request('create', bot='shop.lead', workspace=str(self.path),
                              settings=grouped('shop.lead', sandbox='workspace-write', network=True,
                                               writable=[str(self.path)]))
        self.assertIn('result', made, made)
        self.run_calls(client, 'shop.lead', 'go',
                       ('spawn_agent', {'name': 'review', 'message': 'q', 'profile': 'reviewer'}),
                       ('spawn_agent', {'name': 'careful', 'message': 'q', 'profile': 'careful'}),
                       ('spawn_agent', {'name': 'loose', 'message': 'q', 'profile': 'loose'}),
                       ('spawn_agent', {'name': 'asked', 'message': 'q', 'profile': 'loose',
                                        'settings': {'sandbox': 'workspace-write'}}))
        # Read-only: nothing written, no network, its maker's folders gone.
        for name in ('shop.review', 'shop.careful'):
            settings = client.request('inspect', bot=name)['result']['settings']
            self.assertEqual((settings['sandbox'], settings.get('network', False), settings.get('writable') or []),
                             ('read-only', False, []), settings)
        # Looser than its maker is refused, saying so; nothing is made.
        refused = self.output(client, 'shop.lead', 'spawn_agent-3')
        self.assertIn('invalid_setting', refused)
        self.assertIn("profile loose: sandbox: the profile's danger-full-access is looser than workspace-write", refused)
        self.assertEqual(client.request('inspect', bot='shop.loose')['error'], 'bot_not_found')
        # The call's own sandbox stands over the profile's.
        asked = client.request('inspect', bot='shop.asked')['result']['settings']
        self.assertEqual(asked['sandbox'], 'workspace-write', asked)
        # Sent again after its profile went looser: the agent it made stands,
        # as when its folder's files change; only the message, from another
        # turn, is another request.
        spawn = ('spawn_agent', {'name': 'once', 'message': 'q', 'profile': 'reviewer', 'request_id': 'once'})
        self.run_calls(client, 'shop.lead', 'first', spawn)
        (agents / 'reviewer.md').write_text('---\nsandbox: danger-full-access\n---\nReview it.\n')
        self.run_calls(client, 'shop.lead', 'again', spawn)
        events = client.request('events', bot='shop.lead', after=0, limit=256)['result']['events']
        node = [e for e in events if e['event'] == 'tool_completed'][-1]['data']['node']
        again = node_item(client, 'shop.lead', node)['result']['output']
        self.assertIn('from differs from the request that sent message', again)
        self.assertEqual(client.request('inspect', bot='shop.once')['result']['settings']['sandbox'], 'read-only')

    def test_a_turn_parked_across_a_restart_spawns_with_a_profile_once_it_resumes(self):
        agents = self.path / '.agents' / 'agents'
        agents.mkdir(parents=True)
        (agents / 'helper.md').write_text('---\ndescription: a helper\n---\nHelp.\n')
        # Parked on a wait, then the spawn once the wait ends.
        def respond(request, user):
            if not user.startswith('resume:'):
                return None
            done = sum(i.get('type') == 'function_call' for i in request['input'])
            name = user.split(':')[1]
            return ([('wait', {'handles': [f'shop.slow{name}/1'], 'timeout_ms': 60000})] if done == 0 else
                    [('spawn_agent', {'name': f'helper{name}', 'message': 'q', 'profile': 'helper'})]
                    if done == 1 else 'done')
        self.model.respond = respond
        # The daemon dies with the turn parked, or is stopped.
        for name, kill in (('crash', True), ('stop', False)):
            client = self.client(TOOLS)
            client.request('create', bot=f'shop.slow{name}', workspace=str(self.path))
            client.request('submit', bot=f'shop.slow{name}', request_id='slow', prompt='wait')
            client.request('create', bot=f'shop.lead{name}', workspace=str(self.path),
                           settings=grouped('shop.lead'))
            turn = client.request('submit', bot=f'shop.lead{name}', request_id='go',
                                  prompt=f'resume:{name}')['result']['message']
            client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn, timeout=10)
            client.close(kill=kill)
            client = self.client(TOOLS)
            self.assertEqual(client.finished(turn, timeout=15)['data']['status'], 'completed')
            made = json.loads(self.output(client, f'shop.lead{name}', 'spawn_agent-1'))
            self.assertEqual(made['name'], f'shop.helper{name}', made)
            helper = client.request('inspect', bot=f'shop.helper{name}')['result']
            self.assertIn('Help.', helper['instructions'])
            client.close()

    def test_a_file_that_would_wait_on_another_program_never_holds_a_spawn(self):
        # A FIFO where composing reads (the person's memory index, a
        # profile) would hold a read until something wrote to it: the call
        # fails back at once, saying which file, and its audit row says so.
        home = self.path / 'home'
        (home / '.agents' / 'memory').mkdir(parents=True)
        os.mkfifo(home / '.agents' / 'memory' / 'MEMORY.md')
        (self.path / '.agents' / 'agents').mkdir(parents=True)
        os.mkfifo(self.path / '.agents' / 'agents' / 'stuck.md')
        client = Client(self.binary, self.path / 'state.sqlite', self.url, TOOLS,
                        extra=PROTECTING, env={**clean_env(), 'HOME': str(home)})
        self.addCleanup(client.close)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))
        self.run_calls(client, 'shop.lead', 'go',
                       ('spawn_agent', {'name': 'told', 'message': 'q', 'agents': True}),
                       ('spawn_agent', {'name': 'stuck', 'message': 'q', 'profile': 'stuck'}))
        told = json.loads(self.output(client, 'shop.lead', 'spawn_agent-1'))
        self.assertEqual(told['error'], 'instructions_unreadable', told)
        self.assertIn('MEMORY.md: it is not a regular file', told['detail'])
        stuck = json.loads(self.output(client, 'shop.lead', 'spawn_agent-2'))
        self.assertEqual(stuck['error'], 'profile_not_found', stuck)
        for name in ('shop.told', 'shop.stuck'):
            self.assertEqual(client.request('inspect', bot=name)['error'], 'bot_not_found')
        rows = client.request('audit', bot='shop.lead')['result']['rows']
        self.assertEqual({row['call_id']: row['outcome'] for row in rows},
                         {'spawn_agent-1': 'failed', 'spawn_agent-2': 'failed'})

    def test_an_agent_keeps_the_profile_it_was_made_in(self):
        agents = self.path / '.agents' / 'agents'
        agents.mkdir(parents=True)
        (agents / 'reviewer.md').write_text('Review it.\n')
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))
        self.run_calls(client, 'shop.lead', 'go',
                       ('spawn_agent', {'name': 'review', 'message': 'q', 'profile': 'reviewer'}),
                       ('spawn_agent', {'name': 'plain', 'message': 'q'}))
        self.assertEqual(client.request('inspect', bot='shop.review')['result']['profile'], 'reviewer')
        self.assertNotIn('profile', client.request('inspect', bot='shop.plain')['result'])
        # A fork keeps its source's; the listing an agent reads names it.
        client.request('fork', source='shop.review', bot='shop.review-copy')
        self.run_calls(client, 'shop.lead', 'list', ('list_agents', {}))
        rows = {row['name']: row for row in json.loads(self.output(client, 'shop.lead', 'list_agents-1'))['bots']}
        self.assertEqual((rows['shop.review']['profile'], rows['shop.review-copy']['profile']),
                         ('reviewer', 'reviewer'))
        self.assertNotIn('profile', rows['shop.plain'])
        # Named again by its client, or let go.
        client.request('configure', bot='shop.plain', profile='task')
        self.assertEqual(client.request('inspect', bot='shop.plain')['result']['profile'], 'task')
        client.request('configure', bot='shop.plain', profile=None)
        self.assertNotIn('profile', client.request('inspect', bot='shop.plain')['result'])
        bad = client.request('configure', bot='shop.plain', profile='../x')
        self.assertEqual(bad['error'], 'invalid_name', bad)

    def test_a_fork_takes_no_profile_and_a_missing_profile_is_refused(self):
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))
        self.run_calls(client, 'shop.lead', 'go',
                       ('spawn_agent', {'name': 'ask', 'message': 'q', 'fork': True, 'profile': 'task'}),
                       ('spawn_agent', {'name': 'odd', 'message': 'q', 'profile': 'nobody'}),
                       ('spawn_agent', {'name': 'ask', 'message': 'what did we decide?', 'fork': True}))
        self.assertIn('a fork keeps', self.output(client, 'shop.lead', 'spawn_agent-1'))
        self.assertIn('profile_not_found', self.output(client, 'shop.lead', 'spawn_agent-2'))
        made = json.loads(self.output(client, 'shop.lead', 'spawn_agent-3'))
        self.assertEqual(made['name'], 'shop.ask', made)
        lead = client.request('inspect', bot='shop.lead')['result']
        fork = client.request('inspect', bot='shop.ask')['result']
        self.assertEqual((fork['created_by'], fork['instructions']), ('shop.lead', lead['instructions']))

    def test_a_closed_agent_leaves_the_list_until_a_message_opens_it(self):
        client = self.client(TOOLS)
        client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))
        self.run_calls(client, 'shop.lead', 'go',
                       ('spawn_agent', {'name': 'fix', 'message': 'hi'}))
        made = json.loads(self.output(client, 'shop.lead', 'spawn_agent-1'))
        client.finished(int(made['handle'].rsplit('/', 1)[1]))
        self.run_calls(client, 'shop.lead', 'close',
                       ('close_agent', {'name': 'shop.fix'}),
                       ('list_agents', {}),
                       ('list_agents', {'closed': True}))
        closed = json.loads(self.output(client, 'shop.lead', 'close_agent-1'))['closed'][0]
        self.assertEqual((closed['bot'], closed['was_open']), ('shop.fix', True), closed)
        self.assertNotIn('stopped_turn', closed)
        own = json.loads(self.output(client, 'shop.lead', 'list_agents-2'))
        self.assertEqual([a['name'] for a in own['bots']], ['shop.lead'])
        every = json.loads(self.output(client, 'shop.lead', 'list_agents-3'))
        self.assertEqual([a['name'] for a in every['bots']], ['shop.fix', 'shop.lead'])
        self.assertTrue(every['bots'][0]['closed_ms'])
        # Its history stays whole, and a message opens it again.
        turn = client.request('submit', bot='shop.fix', request_id='again', prompt='more')['result']['message']
        client.finished(turn)
        self.assertNotIn('closed_ms', client.request('inspect', bot='shop.fix')['result'])

    def test_closing_stops_a_running_turn_and_is_refused_for_an_agent_the_caller_did_not_make(self):
        client = self.client(TOOLS)
        lead = client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))['result']
        client.request('create', bot='shop.fix', workspace=str(self.path), settings=grouped('shop.fix'),
                       created_by='shop.lead', created_by_id=lead['bot_id'])
        client.request('create', bot='shop.other', workspace=str(self.path), settings=grouped('shop.other'))
        busy = client.request('submit', bot='shop.fix', request_id='busy', prompt='wait')['result']['message']
        self.model.requests.get(timeout=3)
        self.run_calls(client, 'shop.lead', 'go',
                       ('close_agent', {'name': 'shop.fix'}),
                       ('close_agent', {'name': 'shop.other'}))
        closed = json.loads(self.output(client, 'shop.lead', 'close_agent-1'))['closed'][0]
        self.assertEqual(closed['stopped_turn'], busy, closed)
        self.assertEqual(client.finished(busy)['data']['status'], 'interrupted')
        self.assertTrue(client.request('inspect', bot='shop.fix')['result']['closed_ms'])
        refused = self.output(client, 'shop.lead', 'close_agent-2')
        self.assertIn('not_permitted', refused)
        self.assertNotIn('closed_ms', client.request('inspect', bot='shop.other')['result'])

    def test_a_pattern_closes_the_quiet_agents_the_caller_made_and_refuses_the_rest(self):
        client = self.client(TOOLS)
        lead = client.request('create', bot='shop.lead', workspace=str(self.path), settings=grouped('shop.lead'))['result']
        for name in ('shop.fix', 'shop.look', 'shop.busy'):
            client.request('create', bot=name, workspace=str(self.path), settings=grouped(name),
                           created_by='shop.lead', created_by_id=lead['bot_id'])
        client.request('create', bot='shop.other', workspace=str(self.path), settings=grouped('shop.other'))
        busy = client.request('submit', bot='shop.busy', request_id='busy', prompt='wait')['result']['message']
        self.model.requests.get(timeout=3)
        self.run_calls(client, 'shop.lead', 'go',
                       ('close_agent', {'name': 'shop.*', 'no_turn_since_ms': int(time.time() * 1000) + 60_000}),
                       ('close_agent', {'names': ['shop.busy', 'shop.gone']}),
                       ('close_agent', {'names': ['shop.lead']}))
        answer = json.loads(self.output(client, 'shop.lead', 'close_agent-1'))
        # The running one and the lead, whose turn asks, have a turn now.
        self.assertEqual([c['bot'] for c in answer['closed']], ['shop.fix', 'shop.look'], answer)
        self.assertEqual([(r['bot'], r['error']) for r in answer['refused']],
                         [('shop.other', 'not_permitted')], answer)
        named = json.loads(self.output(client, 'shop.lead', 'close_agent-2'))
        self.assertEqual(named['closed'][0]['stopped_turn'], busy, named)
        self.assertEqual(named['refused'], [{'bot': 'shop.gone', 'error': 'bot_not_found'}])
        self.assertEqual(client.finished(busy)['data']['status'], 'interrupted')
        self.assertNotIn('closed_ms', client.request('inspect', bot='shop.other')['result'])
        # The tool never closes its own agent, named alone or in a list.
        self.assertIn('not_permitted', self.output(client, 'shop.lead', 'close_agent-3'))
        self.assertNotIn('closed_ms', client.request('inspect', bot='shop.lead')['result'])
        # `*` is a pattern: a person's connection closes every open bot.
        every = client.request('close', bot='*')['result']
        self.assertEqual([c['bot'] for c in every['closed']], ['shop.lead', 'shop.other'], every)
