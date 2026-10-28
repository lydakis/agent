"""`sonny lookout` judged by a model the daemon serves, as on Bedrock with no
other key: the live service wakes a lead, and `--replay` runs the same checks
over the store."""
import json
import os
import queue
import subprocess
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

        def reply(request, user):
            if request['model'] != 'judge-model':
                return None
            body = json.loads(user)
            self.asked.put((request, body))
            return json.dumps({q: (0.9 if q in ('result', 'look_0') else 0.1) for q in body['questions']})
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

    def test_a_model_the_daemon_serves_judges_each_check_on_a_fork(self):
        self.make('calc.lead', 'calc.fix')
        self.subscribe('calc.lead', 'calc')
        # Asked for by a program, not by the person, so its result is news.
        turn = self.daemon.request('submit', bot='calc.fix', request_id='a', prompt='fix the parser',
                                   origin='tasks')['result']['turn']
        self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')

        def replay():
            out = self.lookout('--replay', '--judge', 'openai/judge-model')
            self.assertEqual(out.returncode, 0, out.stderr)
            return {line['kind']: line for line in map(json.loads, out.stdout.splitlines())}
        checks = replay()
        self.assertEqual((checks['Said']['band'], checks['Said']['text']), ('Drop', 'fix the parser'))
        self.assertEqual((checks['Ended']['band'], checks['Ended']['reasons']),
                         ('Wake { steer: false }', [['result', 0.9]]))
        request, said = self.asked.get(timeout=5)
        self.assertFalse(request.get('tools'))
        self.assertIn('You answer yes/no questions', json.dumps(request))
        self.assertIn('redirects', said['questions'])
        self.assertEqual(self.asked.get(timeout=5)[1]['input']['event']['text'], 'reply:fix the parser')
        # Each check's fork is deleted; the idle base stays for the next.
        bots = [b['name'] for b in self.daemon.request('bots')['result']['bots'] if b['name'].startswith('lookout.')]
        self.assertEqual(len(bots), 1, bots)
        # The next run forks the same judge; a bot that took its name and is
        # not one is refused, never forked.
        self.assertEqual(replay()['Ended']['band'], 'Wake { steer: false }')
        self.daemon.request('delete', bot=bots[0])
        self.daemon.request('create', bot=bots[0], workspace=str(self.path), model='openai/judge-model',
                            instructions='Do anything.', tools=['echo'])
        self.assertIn('judge_name_taken', replay()['Ended']['failed'])

    def test_a_check_of_the_person_s_words_reads_what_passed_before(self):
        self.make('calc.lead', 'calc.fix')
        self.subscribe('calc.lead', 'calc')
        for n, prompt in enumerate(['fix the parser', 'no, the lexer']):
            turn = self.daemon.request('submit', bot='calc.fix', request_id=f'p{n}', prompt=prompt)['result']['turn']
            self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        out = self.lookout('--replay', '--judge', 'openai/judge-model')
        self.assertEqual(out.returncode, 0, out.stderr)
        bodies = []
        while not self.asked.empty():
            bodies.append(self.asked.get()[1])
        said = [b for b in bodies if b['input']['event'].get('text') == 'no, the lexer']
        self.assertEqual(len(said), 1, bodies)
        self.assertIn('drifts', said[0]['questions'])
        self.assertEqual(said[0]['input']['exchange'], [
            {'from': 'the person', 'text': 'fix the parser'},
            {'from': 'the task', 'text': 'reply:fix the parser'},
        ])

    def test_a_fork_of_the_lead_judges_from_where_the_lead_stood(self):
        self.make('calc.lead', 'calc.fix')
        self.subscribe('calc.lead', 'calc')
        forked = queue.Queue()

        def reply(request, user):
            if user.startswith('This message is from your lookout'):
                body = json.loads(user[user.index('{'):])
                forked.put((request, body))
                return json.dumps({q: (0.9 if q == 'result' else 0.1) for q in body['questions']})
            if request['model'] == 'judge-model':
                body = json.loads(user)
                self.asked.put((request, body))
                return json.dumps({q: 0.1 for q in body['questions']})
            return None
        self.model.reply_for = reply
        for n, (bot, prompt) in enumerate([('calc.lead', 'plan the parser work'), ('calc.fix', 'fix the parser'),
                                           ('calc.lead', 'words after the task ended')]):
            turn = self.daemon.request('submit', bot=bot, request_id=f'r{n}', prompt=prompt, origin='tasks')['result']['turn']
            self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        out = self.lookout('--replay', '--judge', 'lead,openai/judge-model+lead')
        self.assertEqual(out.returncode, 0, out.stderr)
        checks = {(line['judge'], line['kind']): line for line in map(json.loads, out.stdout.splitlines())}
        self.assertEqual(checks[('lead', 'Ended')]['band'], 'Wake { steer: false }')
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
        self.assertEqual(said[-1]['input']['lead'], [
            {'from': 'to the lead', 'text': 'plan the parser work'},
            {'from': 'the lead', 'text': 'reply:plan the parser work'},
        ])
        # Each fork of the lead is deleted once it answers.
        bots = [b['name'] for b in self.daemon.request('bots')['result']['bots']]
        self.assertEqual(sorted(b for b in bots if b.startswith('lookout.')),
                         [b for b in bots if b.startswith('lookout.') and b.count('.') == 1], bots)

    def test_a_fork_of_the_lead_never_reads_what_came_after(self):
        self.make('calc.lead', 'calc.fix')
        self.subscribe('calc.lead', 'calc')

        def reply(request, user):
            if request['model'] == 'judge-model':
                body = json.loads(user)
                self.asked.put((request, body))
                return json.dumps({q: 0.1 for q in body['questions']})
            return None
        self.model.reply_for = reply
        # The task ends before the lead has said a word.
        for n, (bot, prompt) in enumerate([('calc.fix', 'fix the parser'), ('calc.lead', 'later words')]):
            turn = self.daemon.request('submit', bot=bot, request_id=f'r{n}', prompt=prompt)['result']['turn']
            self.assertEqual(self.daemon.finished(turn)['data']['status'], 'completed')
        out = self.lookout('--replay', '--judge', 'lead,openai/judge-model+lead')
        self.assertEqual(out.returncode, 0, out.stderr)
        checks = {(line['judge'], line['kind']): line for line in map(json.loads, out.stdout.splitlines())}
        self.assertEqual(checks[('lead', 'Ended')]['failed'], 'the lead had said nothing yet')
        bodies = []
        while not self.asked.empty():
            bodies.append(self.asked.get()[1])
        self.assertTrue(bodies)
        self.assertTrue(all(b['input']['lead'] == [] for b in bodies), bodies)

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
        self.assertIn('May need you:\n- calc.fix: turn:calc.fix/', woke)
        self.assertIn('result 0.90', woke)
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

    def note_for(self, bot, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for turn in self.daemon.request('turns', bot=bot)['result']['turns']:
                text = self.daemon.request('prompts', bot=bot, turn=turn['turn'])['result']['prompts']
                if text and text[0]['text'].startswith('Task updates: '):
                    return text[0]['text']
            time.sleep(.2)
        return None

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
