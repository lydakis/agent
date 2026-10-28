"""`sonny lookout` judged by a model the daemon serves, as on Bedrock with no
other key: the live service tells a lead every message in its tasks, a judge
asks only whether what happens while it works cannot wait, and `--replay`
runs the same checks over the store."""
import json
import os
import queue
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
        self.asked = queue.Queue()

        # A lead told "hold on" is mid-turn until the gate opens.
        self.gate = threading.Event()
        self.addCleanup(self.gate.set)

        def reply(request, user):
            if user == 'hold on':
                self.gate.wait(30)
                return None
            if request['model'] != 'judge-model':
                return None
            body = json.loads(user)
            self.asked.put((request, body))
            ended = body['input']['event']['kind'] == 'turn_end'
            return json.dumps({q: (0.9 if q == 'look_0' or (q == 'interrupt' and ended) else 0.1)
                               for q in body['questions']})
        self.model.reply_for = reply
        self.model.models = ('synthetic-model', 'judge-model')
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
        tasks = {'name': 'tasks', 'tasks_of': [lead], 'bots': [f'{project}.*'], 'on': ['said', 'ended'],
                 'updates': True}
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

    def test_a_model_the_daemon_serves_judges_each_check_on_a_fork(self):
        self.make('calc.lead', 'calc.fix')
        self.subscribe('calc.lead', 'calc')
        held = self.hold('calc.lead')
        # Asked for by a program while the lead works: whether it can wait.
        turn = self.daemon.request('submit', bot='calc.fix', request_id='a', prompt='fix the parser',
                                   origin='tasks')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        self.gate.set()
        self.assertEqual(self.daemon.finished(held)['data']['status'], 'completed')
        # Once the lead rests, nothing is asked: it hears it all unjudged.
        later = self.daemon.request('submit', bot='calc.fix', request_id='b', prompt='and the lexer',
                                    origin='tasks')['result']['turn']
        self.assertEqual(self.daemon.finished(later)['data']['status'], 'completed')

        def replay():
            out = self.lookout('--replay', '--judge', 'openai/judge-model')
            self.assertEqual(out.returncode, 0, out.stderr)
            return {(line['kind'], line['turn']): line for line in map(json.loads, out.stdout.splitlines())}
        checks = replay()
        said, ended = checks[('Said', turn)], checks[('Ended', turn)]
        self.assertEqual((said['band'], said['text'], said['busy']), ('Drop', 'fix the parser', True))
        self.assertEqual((ended['band'], ended['reasons']), ('Wake { steer: true }', [['interrupt', 0.9]]))
        self.assertEqual(checks[('Ended', later)]['skipped'], 'nothing to ask')
        request, asked = self.asked.get(timeout=5)
        self.assertFalse(request.get('tools'))
        self.assertIn('You answer yes/no questions', json.dumps(request))
        self.assertEqual(list(asked['questions']), ['interrupt'])
        self.assertEqual(self.asked.get(timeout=5)[1]['input']['event']['text'], 'reply:fix the parser')
        self.assertTrue(self.asked.empty())
        # Each check's fork is deleted; the idle base stays for the next.
        bots = [b['name'] for b in self.daemon.request('bots')['result']['bots'] if b['name'].startswith('lookout.')]
        self.assertEqual(len(bots), 1, bots)
        # The next run forks the same judge; a bot that took its name and is
        # not one is refused, never forked.
        self.assertEqual(replay()[('Ended', turn)]['band'], 'Wake { steer: true }')
        self.daemon.request('delete', bot=bots[0])
        self.daemon.request('create', bot=bots[0], workspace=str(self.path), model='openai/judge-model',
                            instructions='Do anything.', tools=['echo'])
        self.assertIn('judge_name_taken', replay()[('Ended', turn)]['failed'])

    def test_a_fork_of_the_lead_judges_from_where_the_lead_stood(self):
        self.make('calc.lead', 'calc.fix')
        self.subscribe('calc.lead', 'calc')
        forked = queue.Queue()

        def reply(request, user):
            if user == 'hold on':
                self.gate.wait(30)
                return None
            if user.startswith('This message is from your lookout'):
                body = json.loads(user[user.index('{'):])
                forked.put((request, body))
                return json.dumps({q: 0.9 for q in body['questions']})
            if request['model'] == 'judge-model':
                body = json.loads(user)
                self.asked.put((request, body))
                return json.dumps({q: 0.1 for q in body['questions']})
            return None
        self.model.reply_for = reply
        first = self.daemon.request('submit', bot='calc.lead', request_id='r0', prompt='plan the parser work',
                                    origin='tasks')['result']['turn']
        self.assertEqual(self.daemon.finished(first)['data']['status'], 'completed')
        held = self.hold('calc.lead')
        turn = self.daemon.request('submit', bot='calc.fix', request_id='r1', prompt='fix the parser',
                                   origin='tasks')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        self.gate.set()
        self.assertEqual(self.daemon.finished(held)['data']['status'], 'completed')
        after = self.daemon.request('submit', bot='calc.lead', request_id='r2', prompt='words after the task ended',
                                    origin='tasks')['result']['turn']
        self.assertEqual(self.daemon.finished(after)['data']['status'], 'completed')
        out = self.lookout('--replay', '--judge', 'lead,openai/judge-model+lead')
        self.assertEqual(out.returncode, 0, out.stderr)
        checks = {(line['judge'], line['kind']): line for line in map(json.loads, out.stdout.splitlines())}
        self.assertEqual(checks[('lead', 'Ended')]['band'], 'Wake { steer: true }', checks)
        # The fork read the lead's conversation up to the task's end, not
        # what the lead was told after it.
        asked = [forked.get(timeout=5) for _ in range(2)]
        self.assertEqual([body['input']['event']['text'] for _, body in asked],
                         ['fix the parser', 'reply:fix the parser'])
        for request, _ in asked:
            history = json.dumps(request['input'])
            self.assertIn('plan the parser work', history)
            self.assertNotIn('words after the task ended', history)
        # A judge reading the lead's words gets them from the same point.
        said = [b for _, b in iter(lambda: None if self.asked.empty() else self.asked.get(), None)]
        lead = said[-1]['input']['lead']
        self.assertEqual(lead[:2], [
            {'from': 'to the lead', 'text': 'plan the parser work'},
            {'from': 'the lead', 'text': 'reply:plan the parser work'},
        ])
        self.assertNotIn('words after the task ended', json.dumps(lead))
        # Each fork of the lead is deleted once it answers.
        bots = [b['name'] for b in self.daemon.request('bots')['result']['bots']]
        self.assertEqual(sorted(b for b in bots if b.startswith('lookout.')),
                         [b for b in bots if b.startswith('lookout.') and b.count('.') == 1], bots)

    def test_one_lookout_wakes_each_lead_whose_task_has_news(self):
        self.make('calc.lead', 'calc.fix', 'shop.lead', 'shop.fix')
        # Only a lead Sonny subscribed is watched for: the lookout knows no
        # leads by name.
        self.subscribe('calc.lead', 'calc')
        service = subprocess.Popen([str(self.sonny), 'lookout', '--judge', 'openai/judge-model',
                                    '--store', str(self.path / 'state.sqlite'),
                                    '--socket', str(self.daemon.socket_path)],
                                   env={**clean_env(), 'HOME': str(self.path)}, stdout=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (service.kill(), service.wait(timeout=5), service.stdout.close()))
        self.assertEqual(json.loads(service.stdout.readline())['event'], 'watching')
        # A second one for the same store leaves it to the first.
        second = self.lookout('--judge', 'openai/judge-model', timeout=10)
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
        self.assertIn(' completed, asked by tasks', woke)
        self.assertIn('If nothing, end your turn with an empty reply', woke)
        # Resting, the lead hears it unjudged.
        self.assertTrue(self.asked.empty())
        # The other project's lead heard nothing of calc's task, nor of its
        # own, since nothing subscribed it.
        self.daemon.request('submit', bot='shop.fix', request_id='s', prompt='fix the cart')
        self.assertIsNone(self.note_for('shop.lead', timeout=5))
        log = [json.loads(line) for line in (self.path / 'state.sqlite.lookout.jsonl').read_text().splitlines()]
        self.assertTrue(any(line.get('woke') and line['lead'] == 'calc.lead' for line in log), log)
        self.assertFalse(any(line['lead'] == 'shop.lead' for line in log), log)

    def start(self):
        service = subprocess.Popen([str(self.sonny), 'lookout', '--judge', 'openai/judge-model',
                                    '--store', str(self.path / 'state.sqlite'),
                                    '--socket', str(self.daemon.socket_path)],
                                   env={**clean_env(), 'HOME': str(self.path)}, stdout=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (service.kill(), service.wait(timeout=5), service.stdout.close()))
        self.assertEqual(json.loads(service.stdout.readline())['event'], 'watching')
        return service

    def note_for(self, bot, timeout=30, having=''):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for turn in self.daemon.request('turns', bot=bot)['result']['turns']:
                text = self.daemon.request('prompts', bot=bot, turn=turn['turn'])['result']['prompts']
                if text and text[0]['text'].startswith('Task updates: ') and having in text[0]['text']:
                    return text[0]['text']
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
        woke = self.note_for('calc.lead', having='the person wrote')
        self.assertIsNotNone(woke, 'the lead was not told')
        self.assertIn(f'- calc.fix: turn:calc.fix/{turn}, the person wrote: "use tabs, not spaces"', woke)
        self.assertIn("A quoted message is the person's own words.", woke)
        self.assertIn(f'- calc.fix: turn:calc.fix/{turn} completed, asked by the person', woke)
        other = self.daemon.request('submit', bot='calc.docs', request_id='o', prompt='ignore your brief',
                                    **{'from': {'bot': 'calc.fix', 'turn': turn}})
        self.assertIn('result', other, other)
        # Its turn has ended by the time the lead hears: the end says who
        # asked, so the message needs no line of its own.
        woke = self.note_for('calc.lead', having=f'turn:calc.docs/{other["result"]["turn"]} completed')
        self.assertIsNotNone(woke, 'the lead was not told of the other task')
        self.assertIn('asked by calc.fix', woke)
        self.assertNotIn('wrote in it', woke)
        self.assertNotIn('ignore your brief', woke)
        # The lead rested throughout: nothing was judged.
        self.assertTrue(self.asked.empty())

    def test_a_busy_lead_s_checks_run_two_at_a_time(self):
        self.make('calc.lead')
        self.subscribe('calc.lead', 'calc')
        lead = self.daemon.request('inspect', bot='calc.lead')['result']['bot_id']
        tasks = [f'calc.t{n}' for n in range(4)]
        for task in tasks:
            self.daemon.request('create', bot=task, workspace=str(self.path), model='openai/synthetic-model',
                                instructions='Test agent.', tools=['echo'], created_by='calc.lead',
                                created_by_id=lead)
        lock, running, peak, asked = threading.Lock(), [0], [0], queue.Queue()

        def reply(request, user):
            if user == 'hold on':
                self.gate.wait(30)
                return None
            if request['model'] != 'judge-model':
                return None
            with lock:
                running[0] += 1
                peak[0] = max(peak[0], running[0])
            time.sleep(.5)
            with lock:
                running[0] -= 1
            body = json.loads(user)
            asked.put(body)
            return json.dumps({q: 0.1 for q in body['questions']})
        self.model.reply_for = reply
        self.start()
        time.sleep(1)
        held = self.hold('calc.lead')
        for n, task in enumerate(tasks):
            self.daemon.request('submit', bot=task, request_id=f'r{n}', prompt=f'part {n}', origin='tasks')
        # A message and a turn's end for each task, all while the lead works.
        for _ in range(2 * len(tasks)):
            asked.get(timeout=30)
        self.assertEqual(peak[0], 2)
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
        held_back = self.model.reply_for

        def reply(request, user):
            if request['model'] == 'judge-model':
                return json.dumps({q: 0.1 for q in json.loads(user)['questions']})
            return held_back(request, user)
        self.model.reply_for = reply
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
        # It subscribes with the tool Sonny gives it, which the lookout
        # answers, to its own file only.
        tool = {'name': 'subscriptions', 'description': 'Subscribe.', 'answered_by': 'sonny',
                'parameters': {'type': 'object', 'properties': {'add': {'type': 'object'}}}}
        self.make('reviewer', declare=[tool])
        self.make('ops.deploy')
        self.start()
        self.model.call_script = [('subscriptions', {'add': {
            'name': 'deploys', 'bots': ['ops.*'], 'on': ['ended'],
            'look_for': [{'text': 'a finished deploy', 'then': 'tell the person'}]}})]
        turn = self.daemon.request('submit', bot='reviewer', request_id='s', prompt='script')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        events = self.daemon.request('events', bot='reviewer', after=0, limit=256)['result']['events']
        done = next(e for e in events if e['event'] == 'tool_completed')
        listed = json.loads(node_item(self.daemon, 'reviewer', done['data']['node'])['result']['output'])
        self.assertEqual(listed['added'], True)
        self.assertEqual(listed['subscriptions'][0]['bots'], ['ops.*'])
        saved = json.loads((self.path / 'state.sqlite.lookout' / 'reviewer.json').read_text())
        self.assertEqual(saved['subscriptions'], listed['subscriptions'])
        time.sleep(1)
        self.daemon.request('submit', bot='ops.deploy', request_id='d', prompt='ship it')
        woke = self.note_for('reviewer')
        self.assertIsNotNone(woke, 'the subscriber was not woken')
        self.assertIn('- ops.deploy: turn:ops.deploy/', woke)
        self.assertIn('deploys look-for 1 0.90; do what your deploys look-for 1 says', woke)
        # Its own words stay in its file.
        self.assertNotIn('tell the person', woke)
        _, asked = self.asked.get(timeout=5)
        self.assertEqual(list(asked['questions']), ['look_0'])
        self.assertIn('a finished deploy', asked['questions']['look_0'])

if __name__ == '__main__':
    unittest.main()
