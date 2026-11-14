"""Turn metadata has one storage rendezvous, including after a historical fork."""
import os
from unittest import skipUnless

from tests.test_runtime import ModelFixture


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class TurnMetadataTests(ModelFixture):
    def test_metadata_job_budget_survives_restart_and_historical_fork(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))

        def complete(bot, prompt):
            before = client.request('stats')['result']['store']['operations']
            turn = client.request('submit', bot=bot, request_id=prompt,
                                  prompt=prompt)['result']['message']
            finished = client.finished(turn)
            self.assertEqual(finished['data']['status'], 'completed')
            after = client.request('stats')['result']['store']['operations']
            counts = {name: after.get(name, {}).get('count', 0)
                      - before.get(name, {}).get('count', 0)
                      for name in ('inspect', 'context')}
            self.assertEqual(counts, {'inspect': 0, 'context': 1})
            return finished['data']['checkpoint'], self.model.requests.get(timeout=3)

        checkpoint, _ = complete('Bob', 'first')
        complete('Bob', 'second')
        self.assertIn('result', client.request('fork', source='Bob', bot='Branch',
                                              checkpoint=checkpoint))
        client.close()
        client = self.client()
        _, request = complete('Branch', 'alternative')
        texts = [item['content'][0]['text'] for item in request['input']]
        self.assertEqual(texts, ['first', 'reply:first', 'alternative'])
