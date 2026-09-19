"""Active-steering measurements must prove delivery and equal provider work."""
import os
from pathlib import Path
import unittest

from bench.active_steering import run


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires runtime and loopback')
class ActiveSteeringTests(unittest.TestCase):
    def test_repeated_boundaries_absorb_every_steer_across_storage_batches(self):
        binary = Path('.local/target/release/agent').resolve()
        for items in (1, 48):
            with self.subTest(output_items=items):
                first = run(binary, bots=2, rounds=2, steers=40, output_items=items)
                second = run(binary, bots=2, rounds=2, steers=40, output_items=items)
                self.assertEqual(first['absorbed_steers'], 160)
                self.assertEqual(first['provider_calls'], 6)
                self.assertEqual(first['operations']['absorb']['count'], 8)
                self.assertEqual(first['history_sha256'], second['history_sha256'])
                self.assertEqual(first['request_bytes'], second['request_bytes'])


if __name__ == '__main__':
    unittest.main()
