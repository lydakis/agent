"""`sonny lookout`: the live service tells a lead every message in its tasks
and how each turn ends, once it rests, and cuts into its running turn when
the person writes in a task."""
import json
import os
import subprocess
import threading
import time
import unittest

from bench.runtime_client import node_item
from bench.socket_client import SocketClient
from bench.targets import clean_env
from tests.test_runtime import ModelFixture


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class LookoutTests(ModelFixture):
    def setUp(self):
        super().setUp()

        # A lead told "hold on" is mid-turn until the gate opens.
        self.gate = threading.Event()
        self.addCleanup(self.gate.set)

        def reply(request, user):
            if user == 'hold on':
                self.gate.wait(30)
            return None
        self.model.reply_for = reply
        self.sonny = self.binary.parent / 'sonny'
        self.daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'echo')
        self.addCleanup(self.daemon.close)

    def lookout(self, *args, timeout=60):
        return subprocess.run([str(self.sonny), 'lookout', '--store', str(self.path / 'state.sqlite'),
                               '--socket', str(self.daemon.socket_path), *args],
                              env={**clean_env(), 'HOME': str(self.path)}, capture_output=True, text=True,
                              timeout=timeout)

    def subscribe(self, lead, project):
        """The subscription Sonny writes for a lead when it opens its project."""
        folder = self.path / 'state.sqlite.lookout'
        folder.mkdir(exist_ok=True)
        tasks = {'name': 'tasks', 'tasks_of': [lead], 'bots': [f'{project}.*']}
        (folder / f'{lead}.json').write_text(json.dumps({'subscriptions': [tasks]}))

    def make(self, *bots, declare=None):
        for bot in bots:
            self.daemon.request('create', bot=bot, workspace=str(self.path), model='openai/synthetic-model',
                                instructions='Test agent.', tools=['echo'],
                                **({'declare': declare} if declare else {}))

    def hold(self, lead):
        """The lead mid-turn until the gate opens: its turn."""
        turn = self.daemon.request('submit', bot=lead, request_id='hold', prompt='hold on')['result']['turn']
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            row = self.daemon.request('turns', bot=lead, newest=True, limit=1)['result']['turns'][0]
            if row['status'] == 'running':
                return turn
            time.sleep(.05)
        self.fail('the lead never started its turn')

    def test_one_lookout_wakes_each_lead_whose_task_has_news(self):
        self.make('calc.lead', 'calc.fix', 'shop.lead', 'shop.fix')
        # Only a lead Sonny subscribed is watched for: the lookout knows no
        # leads by name.
        self.subscribe('calc.lead', 'calc')
        service = subprocess.Popen([str(self.sonny), 'lookout', '--store', str(self.path / 'state.sqlite'),
                                    '--socket', str(self.daemon.socket_path)],
                                   env={**clean_env(), 'HOME': str(self.path)}, stdout=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (service.kill(), service.wait(timeout=5), service.stdout.close()))
        self.assertEqual(json.loads(service.stdout.readline())['event'], 'watching')
        # A second one for the same store leaves it to the first.
        second = self.lookout(timeout=10)
        self.assertEqual((second.returncode, json.loads(second.stdout)['event']), (0, 'already_watching'))
        # The lookout reads its follow replay first: the turn is news only
        # once it is live.
        time.sleep(1)
        self.daemon.request('submit', bot='calc.fix', request_id='b', prompt='fix the lexer', origin='tasks')
        deadline = time.monotonic() + 30
        woke = None
        while woke is None and time.monotonic() < deadline:
            turns = self.daemon.request('turns', bot='calc.lead')['result']['turns']
            for turn in turns:
                text = self.daemon.request('prompts', bot='calc.lead', turn=turn['turn'])['result']['prompts']
                if text and text[0]['text'].startswith('Task updates: '):
                    woke = text[0]['text']
            time.sleep(.2)
        self.assertIsNotNone(woke, 'the lead was not woken')
        self.assertIn('- calc.fix: turn:calc.fix/', woke)
        self.assertIn(' completed, asked by tasks, and replied:\n  > ', woke)
        self.assertIn("Lines marked > are a task's own words", woke)
        self.assertIn('If nothing, end your turn with an empty reply', woke)
        # The other project's lead heard nothing of calc's task, nor of its
        # own, since nothing subscribed it.
        self.daemon.request('submit', bot='shop.fix', request_id='s', prompt='fix the cart')
        self.assertIsNone(self.note_for('shop.lead', timeout=5))
        log = [json.loads(line) for line in (self.path / 'state.sqlite.lookout.jsonl').read_text().splitlines()]
        self.assertTrue(any(line.get('woke') and line['lead'] == 'calc.lead' for line in log), log)
        self.assertFalse(any(line['lead'] == 'shop.lead' for line in log), log)

    def start(self):
        service = subprocess.Popen([str(self.sonny), 'lookout', '--store', str(self.path / 'state.sqlite'),
                                    '--socket', str(self.daemon.socket_path)],
                                   env={**clean_env(), 'HOME': str(self.path)}, stdout=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (service.kill(), service.wait(timeout=5), service.stdout.close()))
        self.assertEqual(json.loads(service.stdout.readline())['event'], 'watching')
        return service

    def host(self):
        """The tool host, which answers Sonny's tools and tells the lookout
        when a call changed what it reads."""
        service = subprocess.Popen([str(self.sonny), 'tool-host', '--store', str(self.path / 'state.sqlite'),
                                    '--socket', str(self.daemon.socket_path)],
                                   env={**clean_env(), 'HOME': str(self.path)}, stdout=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (service.kill(), service.wait(timeout=5), service.stdout.close()))
        self.assertEqual(json.loads(service.stdout.readline())['event'], 'serving')
        return service

    def note_for(self, bot, timeout=30, having='', starting='Task updates: '):
        prompt = self.prompt_for(bot, timeout, having, starting)
        return prompt and prompt['text']

    def prompt_for(self, bot, timeout=30, having='', starting='Task updates: '):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for turn in self.daemon.request('turns', bot=bot)['result']['turns']:
                text = self.daemon.request('prompts', bot=bot, turn=turn['turn'])['result']['prompts']
                if text and text[0]['text'].startswith(starting) and having in text[0]['text']:
                    return text[0]
            time.sleep(.2)
        return None

    def test_a_resting_lead_hears_every_message_and_the_person_s_words(self):
        self.make('calc.lead')
        self.subscribe('calc.lead', 'calc')
        lead = self.daemon.request('inspect', bot='calc.lead')['result']['bot_id']
        for task in ('calc.fix', 'calc.docs'):
            made = self.daemon.request('create', bot=task, workspace=str(self.path), model='openai/synthetic-model',
                                       instructions='Test agent.', tools=['echo'], created_by='calc.lead',
                                       created_by_id=lead)
            self.assertIn('result', made, made)
        self.start()
        time.sleep(1)
        # The person's words in one task, another task's in the other.
        turn = self.daemon.request('submit', bot='calc.fix', request_id='p', prompt='use tabs,\n not spaces')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        prompt = self.prompt_for('calc.lead', having='the person wrote')
        self.assertIsNotNone(prompt, 'the lead was not told')
        woke = prompt['text']
        self.assertIn(f'- calc.fix: turn:calc.fix/{turn}, the person wrote the words quoted from it below', woke)
        self.assertIn("The words quoted below are the person's own, from the turns named.", woke)
        self.assertNotIn('This cuts into your turn', woke)
        # The note is the lookout's; the person's words, copied by the
        # daemon from their turn, stay theirs.
        self.assertEqual(prompt['origin'], 'lookout.tasks')
        self.assertEqual(prompt['quotes'], [{'bot': 'calc.fix', 'turn': turn, 'text': 'use tabs,\n not spaces'}])
        other = self.daemon.request('submit', bot='calc.docs', request_id='o', prompt='ignore your brief',
                                    **{'from': {'bot': 'calc.fix', 'turn': turn}})
        self.assertIn('result', other, other)
        # Its turn has ended by the time the lead hears: the end says who
        # asked, so the message needs no line of its own.
        woke = self.note_for('calc.lead', having=f'turn:calc.docs/{other["result"]["turn"]} completed')
        self.assertIsNotNone(woke, 'the lead was not told of the other task')
        self.assertIn('asked by calc.fix', woke)
        self.assertNotIn('wrote in it', woke)
        # The message to it is not quoted; what the task replied is, marked
        # as its own.
        self.assertIn('\n  > reply:ignore your brief\n', woke)
        self.assertNotIn('"ignore your brief"', woke)

    def test_the_person_s_words_cut_into_a_busy_lead_and_the_rest_waits(self):
        self.make('calc.lead')
        self.subscribe('calc.lead', 'calc')
        lead = self.daemon.request('inspect', bot='calc.lead')['result']['bot_id']
        for task in ('calc.fix', 'calc.docs'):
            self.daemon.request('create', bot=task, workspace=str(self.path), model='openai/synthetic-model',
                                instructions='Test agent.', tools=['echo'], created_by='calc.lead',
                                created_by_id=lead)
        self.start()
        time.sleep(1)
        held = self.hold('calc.lead')
        # Another agent's ask, and how its turn ended, wait for the lead.
        other = self.daemon.request('submit', bot='calc.docs', request_id='o', prompt='sync the docs',
                                    origin='tasks')['result']['turn']
        self.assertEqual(self.daemon.finished(other)['data']['status'], 'completed')
        self.assertIsNone(self.note_for('calc.lead', timeout=4))
        # The person's words in a task cut in at once, with the rest.
        self.daemon.request('submit', bot='calc.fix', request_id='p', prompt='stop, use the other branch')
        prompt = self.prompt_for('calc.lead', having='the person wrote')
        self.assertIsNotNone(prompt, 'the person\'s words did not reach the busy lead')
        woke = prompt['text']
        self.assertEqual([quote['text'] for quote in prompt['quotes']], ['stop, use the other branch'])
        self.assertIn(f'turn:calc.docs/{other} completed, asked by tasks', woke)
        self.assertIn('This cuts into your turn because the person wrote in a task', woke)
        events = self.daemon.request('events', bot='calc.lead', after=0, limit=256)['result']['events']
        steered = [e for e in events if e['event'] == 'queued'
                   and str(e['data'].get('request_id', '')).startswith('lookout-')]
        self.assertEqual([e['data']['delivery'] for e in steered], ['steer'], steered)
        self.gate.set()
        self.assertEqual(self.daemon.finished(held)['data']['status'], 'completed')

    def test_what_happened_while_the_lookout_was_down_reaches_the_lead(self):
        self.make('calc.lead', 'calc.fix')
        self.subscribe('calc.lead', 'calc')
        service = self.start()
        time.sleep(1)
        first = self.daemon.request('submit', bot='calc.fix', request_id='a', prompt='fix the parser',
                                    origin='tasks')['result']['turn']
        self.assertIsNotNone(self.note_for('calc.lead', having=f'turn:calc.fix/{first} completed'))
        # Long enough to keep where it got to; then it is gone while a task
        # ends.
        time.sleep(2)
        service.kill()
        service.wait(timeout=5)
        second = self.daemon.request('submit', bot='calc.fix', request_id='b', prompt='and the lexer',
                                     origin='tasks')['result']['turn']
        self.assertEqual(self.daemon.finished(second)['data']['status'], 'completed')
        self.start()
        woke = self.note_for('calc.lead', having=f'turn:calc.fix/{second} completed')
        self.assertIsNotNone(woke, 'what ended while the lookout was down never reached the lead')
        # What it told before is not told again.
        self.assertNotIn(f'turn:calc.fix/{first} ', woke)

    def test_news_waiting_on_a_busy_lead_outlives_a_lookout_restart(self):
        self.make('calc.lead', 'calc.fix')
        self.subscribe('calc.lead', 'calc')
        service = self.start()
        time.sleep(1)
        held = self.hold('calc.lead')
        turn = self.daemon.request('submit', bot='calc.fix', request_id='a', prompt='fix the parser',
                                   origin='tasks')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        # The news waits on the lead, in the lookout alone, past a cursor save.
        time.sleep(3)
        service.kill()
        service.wait(timeout=5)
        self.start()
        time.sleep(1)
        self.gate.set()
        self.assertEqual(self.daemon.finished(held)['data']['status'], 'completed')
        woke = self.note_for('calc.lead', having=f'turn:calc.fix/{turn} completed')
        self.assertIsNotNone(woke, 'news held for a busy lead was lost with the lookout')

    def test_a_bot_that_is_no_lead_hears_what_it_subscribed_to(self):
        # It subscribes with the tool Sonny gives it, which the tool host
        # answers, to its own file only.
        tool = {'name': 'subscriptions', 'description': 'Subscribe.', 'answered_by': 'sonny',
                'parameters': {'type': 'object', 'properties': {'add': {'type': 'object'}}}}
        self.make('reviewer', declare=[tool])
        self.make('ops.deploy')
        self.start()
        self.host()
        self.model.call_script = [('subscriptions', {'add': {'name': 'deploys', 'bots': ['ops.*']}})]
        turn = self.daemon.request('submit', bot='reviewer', request_id='s', prompt='script')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        events = self.daemon.request('events', bot='reviewer', after=0, limit=256)['result']['events']
        done = next(e for e in events if e['event'] == 'tool_completed')
        listed = json.loads(node_item(self.daemon, 'reviewer', done['data']['node'])['result']['output'])
        self.assertEqual(listed['added'], True)
        self.assertEqual(listed['subscriptions'][0]['bots'], ['ops.*'])
        saved = json.loads((self.path / 'state.sqlite.lookout' / 'reviewer.json').read_text())
        self.assertEqual(saved['subscriptions'], listed['subscriptions'])
        # Heard from the next event on, with no wait for a reread.
        self.daemon.request('submit', bot='ops.deploy', request_id='d', prompt='ship it')
        woke = self.note_for('reviewer')
        self.assertIsNotNone(woke, 'the subscriber was not woken')
        self.assertIn('- ops.deploy: turn:ops.deploy/', woke)
        self.assertIn(' completed, asked by the person', woke)

    def test_what_a_source_prints_reaches_who_subscribed_to_it(self):
        # The person's poller prints a plain line and an event it gives an
        # ID, which it prints again on its next run.
        poller = self.path / 'poll.sh'
        poller.write_text('echo "lunch?"\n'
                          'echo \'{"text": "deploy failed on prod", "id": "d1", "from": "ci", "steer": true}\'\n'
                          'echo \'{"text": "deploy failed on prod", "id": "d1", "from": "ci"}\'\n'
                          'echo "$SONNY_SOURCE" > "$SONNY_SOURCE_DIR/ran"\n')
        (self.path / '.agent').mkdir(exist_ok=True)
        (self.path / '.agent' / 'config.toml').write_text(
            f'[sonny.sources.chat]\ncommand = "sh {poller}"\nevery = "1h"\n')
        self.make('reviewer')
        folder = self.path / 'state.sqlite.lookout'
        folder.mkdir(exist_ok=True)
        subs = [{'name': 'all', 'sources': ['chat']}, {'name': 'deploys', 'sources': ['chat']}]
        (folder / 'reviewer.json').write_text(json.dumps({'subscriptions': subs}))
        self.start()
        prompt = self.prompt_for('reviewer', starting='From your sources: ', having='deploy failed')
        self.assertIsNotNone(prompt, 'the subscriber never heard its source')
        # Not the person's words: the approver reads the origin.
        self.assertEqual(prompt['origin'], 'lookout.sources')
        self.assertIn('- chat, from "ci" (for your all, deploys, as a steer): "deploy failed on prod"', prompt['text'])
        self.assertIsNotNone(self.note_for('reviewer', starting='From your sources: ', having='"lunch?"'))
        # An event printed again by its ID is told once.
        time.sleep(3)
        told = [p for t in self.daemon.request('turns', bot='reviewer')['result']['turns']
                for p in self.daemon.request('prompts', bot='reviewer', turn=t['turn'])['result']['prompts']
                if p['text'].startswith('From your sources: ')]
        self.assertEqual(sum(p['text'].count('deploy failed on prod') for p in told), 1)
        self.assertEqual((self.path / 'state.sqlite.sources' / 'chat' / 'ran').read_text(), 'chat\n')

    def test_a_filtered_subscription_hears_only_the_events_it_matches(self):
        # A change poller prints two changes; the subscriber cares about
        # billing only.
        poller = self.path / 'crs.sh'
        poller.write_text(
            'echo \'{"text": "CR-1 search", "id": "1", "paths": ["src/search/index.rs"], "lines": 40}\'\n'
            'echo \'{"text": "CR-2 billing", "id": "2", "paths": ["src/billing/tax.rs", "README.md"], "lines": 7}\'\n')
        (self.path / '.agent').mkdir(exist_ok=True)
        (self.path / '.agent' / 'config.toml').write_text(
            f'[sonny.sources.cr]\ncommand = "sh {poller}"\nevery = "1h"\n')
        self.make('reviewer')
        folder = self.path / 'state.sqlite.lookout'
        folder.mkdir(exist_ok=True)
        subs = [{'name': 'billing', 'sources': ['cr'], 'filter': {'paths': [{'prefix': 'src/billing/'}]}}]
        (folder / 'reviewer.json').write_text(json.dumps({'subscriptions': subs}))
        self.start()
        prompt = self.prompt_for('reviewer', starting='From your sources: ', having='CR-2')
        self.assertIsNotNone(prompt, 'the subscriber never heard the change it filtered for')
        self.assertIn('- cr (for your billing): "CR-2 billing" {"lines":7,"paths":["src/billing/tax.rs","README.md"]}',
                      prompt['text'])
        self.assertNotIn('CR-1', prompt['text'])
        # The lookout kept the source's shape, for agents to filter on.
        shape = json.loads((self.path / 'state.sqlite.sources' / '.shapes' / 'cr.json').read_text())
        self.assertEqual(shape['events'], 2)
        self.assertEqual(shape['fields']['paths']['kinds'], ['list of strings'])
        self.assertEqual(shape['fields']['lines']['kinds'], ['number'])

    def test_a_failed_message_goes_back_to_its_sender_unless_its_lead_hears_the_end(self):
        self.make('calc.lead', 'ops.lead')
        self.subscribe('calc.lead', 'calc')
        lead = self.daemon.request('inspect', bot='calc.lead')['result']['bot_id']
        self.daemon.request('create', bot='calc.fix', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Test agent.', tools=['echo'], created_by='calc.lead', created_by_id=lead)
        self.model.refused_prompts = {'fix the lexer', 'ship the fix'}
        turns = {}
        for bot in ('calc.lead', 'ops.lead'):
            turns[bot] = self.daemon.request('submit', bot=bot, request_id='own', prompt='plan')['result']['turn']
            self.assertEqual(self.daemon.finished(turns[bot])['data']['status'], 'completed')
        self.start()
        time.sleep(1)
        # The lead's own task fails its message: the lead hears that end
        # as a task update, so no bounce comes as well.
        mine = self.daemon.request('submit', bot='calc.fix', request_id='l', prompt='fix the lexer',
                                   **{'from': {'bot': 'calc.lead', 'turn': turns['calc.lead']}})['result']['turn']
        self.assertEqual(self.daemon.finished(mine)['data']['status'], 'failed')
        woke = self.note_for('calc.lead', having=f'turn:calc.fix/{mine} failed')
        self.assertIsNotNone(woke, 'the lead was not told its task failed')
        # Another project's lead hears nothing of calc.fix: its message
        # comes back to it.
        theirs = self.daemon.request('submit', bot='calc.fix', request_id='o', prompt='ship the fix',
                                     **{'from': {'bot': 'ops.lead', 'turn': turns['ops.lead']}})['result']['turn']
        self.assertEqual(self.daemon.finished(theirs)['data']['status'], 'failed')
        bounce = self.prompt_for('ops.lead', starting='[your message to calc.fix')
        self.assertIsNotNone(bounce, 'the sender was not told its message failed')
        self.assertIn(f'turn {theirs} ended provider_http_400', bounce['text'])
        self.assertIsNone(self.prompt_for('calc.lead', timeout=3, starting='[your message to calc.fix'))

if __name__ == '__main__':
    unittest.main()
