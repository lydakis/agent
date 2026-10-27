"""`sonny lookout` judged by a model the daemon serves, as on Bedrock with no
other key: the live service wakes a lead, and `--replay` runs the same checks
over the store."""
import json
import os
import queue
import subprocess
import time
import unittest

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

    def make(self, *bots):
        for bot in bots:
            self.daemon.request('create', bot=bot, workspace=str(self.path), model='openai/synthetic-model',
                                instructions='Test agent.', tools=['echo'])

    def test_a_model_the_daemon_serves_judges_each_check_on_a_fork(self):
        self.make('calc.lead', 'calc.fix')
        turn = self.daemon.request('submit', bot='calc.fix', request_id='a', prompt='fix the parser')['result']['turn']
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
        self.assertIn('is a bot that is not a lookout judge', replay()['Ended']['failed'])

    def test_one_lookout_wakes_each_lead_whose_task_has_news(self):
        self.make('calc.lead', 'calc.fix', 'shop.lead')
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
        self.daemon.request('submit', bot='calc.fix', request_id='b', prompt='fix the lexer')
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
        # The other project's lead heard nothing of calc's task.
        self.assertEqual(self.daemon.request('turns', bot='shop.lead')['result']['turns'], [])
        log = [json.loads(line) for line in (self.path / 'state.sqlite.lookout.jsonl').read_text().splitlines()]
        self.assertTrue(any(line.get('woke') and line['lead'] == 'calc.lead' for line in log), log)

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
        self.make('reviewer', 'ops.deploy')
        store = ['--store', str(self.path / 'state.sqlite')]
        # A bot changes only its own subscriptions.
        refused = subprocess.run([str(self.sonny), 'lookout', 'add', 'deploys', '--for', 'reviewer', *store,
                                  '--bot', 'ops.*'],
                                 env={**clean_env(), 'HOME': str(self.path), 'AGENT_BOT': 'ops.deploy'},
                                 capture_output=True, text=True, timeout=10)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('only its own', refused.stderr)
        added = subprocess.run([str(self.sonny), 'lookout', 'add', 'deploys', *store, '--bot', 'ops.*', '--on', 'ended',
                                '--look-for', 'a finished deploy', '--then', 'tell the person'],
                               env={**clean_env(), 'HOME': str(self.path), 'AGENT_BOT': 'reviewer'},
                               capture_output=True, text=True, timeout=10)
        self.assertEqual((added.returncode, json.loads(added.stdout)['added']), (0, True), added.stderr)
        listed = subprocess.run([str(self.sonny), 'lookout', 'ls', '--for', 'reviewer', *store],
                                env={**clean_env(), 'HOME': str(self.path)}, capture_output=True, text=True, timeout=10)
        self.assertEqual(json.loads(listed.stdout)['subscriptions'][0]['bots'], ['ops.*'])
        self.start()
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
