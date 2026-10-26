"""Sonny's lookout judged by a model the daemon serves, as on Bedrock with no
other key: `sonny lookout-replay` runs the live checks over the store."""
import json
import os
from pathlib import Path
import queue
import subprocess
import unittest

from bench.socket_client import SocketClient
from bench.targets import clean_env
from tests.test_runtime import ModelFixture


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class LookoutTests(ModelFixture):
    def test_a_model_the_daemon_serves_judges_each_check_on_a_fork(self):
        asked = queue.Queue()

        def reply(request, user):
            if request['model'] != 'judge-model':
                return None
            body = json.loads(user)
            asked.put((request, body))
            return json.dumps({q: (0.9 if q == 'result' else 0.1) for q in body['questions']})
        self.model.reply_for = reply
        self.model.models = ('synthetic-model', 'judge-model')
        daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'echo')
        self.addCleanup(daemon.close)
        for bot in ('calc.lead', 'calc.fix'):
            daemon.request('create', bot=bot, workspace=str(self.path), model='openai/synthetic-model',
                           instructions='Test agent.', tools=['echo'])
        turn = daemon.request('submit', bot='calc.fix', request_id='a', prompt='fix the parser')['result']['turn']
        self.assertEqual(daemon.finished(turn)['data']['status'], 'completed')
        sonny = Path(__file__).resolve().parent.parent / '.local/target/release/sonny'
        out = subprocess.run([str(sonny), 'lookout-replay', '--judge', 'openai/judge-model',
                              '--store', str(self.path / 'state.sqlite'), '--socket', str(daemon.socket_path)],
                             env={**clean_env(), 'HOME': str(self.path)}, capture_output=True, text=True, timeout=60)
        self.assertEqual(out.returncode, 0, out.stderr)
        checks = {line['kind']: line for line in map(json.loads, out.stdout.splitlines())}
        self.assertEqual((checks['Said']['band'], checks['Said']['text']), ('Drop', 'fix the parser'))
        self.assertEqual((checks['Ended']['band'], checks['Ended']['reasons']),
                         ('Wake { steer: false }', [['result', 0.9]]))
        request, said = asked.get(timeout=5)
        self.assertFalse(request.get('tools'))
        self.assertIn('You answer yes/no questions', json.dumps(request))
        self.assertIn('redirects', said['questions'])
        self.assertEqual(asked.get(timeout=5)[1]['input']['event']['text'], 'reply:fix the parser')
        # Each check's fork is deleted; the idle base stays for the next.
        bots = [b['name'] for b in daemon.request('bots')['result']['bots'] if b['name'].startswith('lookout.')]
        self.assertEqual(len(bots), 1, bots)


if __name__ == '__main__':
    unittest.main()
