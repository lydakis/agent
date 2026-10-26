"""`agent lookout` judged by a model the daemon serves, as on Bedrock with no
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
            return json.dumps({q: (0.9 if q == 'result' else 0.1) for q in body['questions']})
        self.model.reply_for = reply
        self.model.models = ('synthetic-model', 'judge-model')
        self.daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'echo')
        self.addCleanup(self.daemon.close)

    def lookout(self, *args, timeout=60):
        return subprocess.run([str(self.binary), 'lookout', '--store', str(self.path / 'state.sqlite'),
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
        service = subprocess.Popen([str(self.binary), 'lookout', '--judge', 'openai/judge-model',
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
                if text and text[0]['text'].startswith('Lookout:'):
                    woke = text[0]['text']
            time.sleep(.2)
        self.assertIsNotNone(woke, 'the lead was not woken')
        self.assertIn('calc.fix, turn:calc.fix/', woke)
        self.assertIn('result 0.90', woke)
        # The other project's lead heard nothing of calc's task.
        self.assertEqual(self.daemon.request('turns', bot='shop.lead')['result']['turns'], [])
        log = [json.loads(line) for line in (self.path / 'state.sqlite.lookout.jsonl').read_text().splitlines()]
        self.assertTrue(any(line.get('woke') and line['lead'] == 'calc.lead' for line in log), log)


if __name__ == '__main__':
    unittest.main()
