"""Declared tool lists at fleet size, through the daemon's socket alone (no
Sonny): bots that each declare a list the size of Sonny's (about 31.6 KB),
then a client that configures every bot to the list it already holds, as a
host does at each start, once sending the list and once its digest. Also
what a bot's row keeps, what `inspect` and a `bots` listing cost, and the
store's growth. Synthetic model, no spend; needs a release build.

    AGENT_TEST_RUNTIME=1 AGENTS=6,424,2000,5000 [BINARY=PATH] \\
        python3 -m bench.tool_lists
"""
import json
import os
import sqlite3
import time
import unittest

from bench.socket_client import Connection, SocketClient
from tests.test_runtime import ModelFixture, PROTECTING

AGENTS = [int(n) for n in os.environ.get('AGENTS', '6,2000').split(',')]
# Sonny's lead declares about 31.6 KB: this many tools of this size.
TOOLS, DESCRIPTION = 20, 1_450


def sonny_sized():
    return [{'name': f'tool_{i}', 'description': 'd' * DESCRIPTION,
             'parameters': {'type': 'object', 'properties': {'text': {'type': 'string'}}},
             'answered_by': 'client', 'kind': 'agents'} for i in range(TOOLS)]


def rss_kib(pid):
    with open(f'/proc/{pid}/status') as status:
        for line in status:
            if line.startswith('VmRSS:'):
                return int(line.split()[1])
    return None


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ToolLists(ModelFixture):
    def test_fleet(self):
        results = [self.run_fleet(agents) for agents in AGENTS]
        for result in results:
            print(json.dumps(result), flush=True)
        (self.path.parent / 'tool_lists.json').write_text(json.dumps(results, indent=1))

    def run_fleet(self, agents):
        store = self.path / f'tool-lists-{agents}.sqlite'
        # BINARY names another build, such as main's, for a matched run.
        binary = os.environ.get('BINARY', self.binary)
        daemon = SocketClient(binary, store, self.url, 'echo', extra=PROTECTING)
        try:
            client = Connection(daemon.socket_path, retain_durable=False)
            client.tools = ['echo']
            listed = sonny_sized()
            sent = len(json.dumps(listed))
            started = time.perf_counter()
            for i in range(agents):
                made = client.request('create', bot=f'p.b{i}', workspace=str(self.path), declare=listed)
                assert 'result' in made, made
            create_s = time.perf_counter() - started
            # Before lists were stored once there is no digest to send.
            digest = made['result'].get('declared_digest')
            size = lambda: sum(p.stat().st_size for p in store.parent.glob(store.name + '*')
                               if p.suffix in ('.sqlite', '.sqlite-wal') or p.name.endswith('-wal'))

            def each(op, **params):
                started = time.perf_counter()
                for i in range(agents):
                    answer = client.request(op, bot=f'p.b{i}', **params)
                    assert 'result' in answer, answer
                return round((time.perf_counter() - started) * 1000 / agents, 3), answer['result']

            before = size()
            by_list, answer = each('configure', declare=listed)
            assert answer['changed'] == [], answer
            after_list = size()
            by_digest = None
            if digest:
                by_digest, answer = each('configure', declare=digest)
                assert answer['changed'] == [], answer
            after_digest = size()
            plain, _ = each('configure', instructions='Test agent.')
            inspected, record = each('inspect')
            started = time.perf_counter()
            after, rows = None, 0
            while True:
                page = client.request('bots', limit=256, after=after)['result']
                rows += len(page['bots'])
                if not page['next_after']:
                    break
                after = page['next_after']
            listing_ms = (time.perf_counter() - started) * 1000
            rss = rss_kib(daemon.process.pid)
            client.close()
        finally:
            daemon.close()
        kept = sqlite3.connect(store)
        stored = kept.execute('SELECT MAX(length(declared)), COUNT(*) FROM bots').fetchone()
        try:
            lists = kept.execute('SELECT COUNT(*), SUM(length(tools)) FROM tool_lists').fetchone()
        except sqlite3.OperationalError:
            lists = (None, None)
        kept.close()
        return {
            'agents': agents, 'list_bytes': sent, 'create_s': round(create_s, 2),
            'configure_same_list_ms_each': by_list,
            'configure_by_digest_ms_each': by_digest,
            'configure_by_digest_fleet_s': by_digest and round(by_digest * agents / 1000, 2),
            'configure_without_declare_ms_each': plain,
            'digest_request_bytes': len(json.dumps({'op': 'configure', 'bot': f'p.b{agents - 1}', 'declare': digest})),
            'store_growth_same_list_bytes': after_list - before,
            'store_growth_by_digest_bytes': after_digest - after_list,
            'inspect_ms_each': inspected, 'inspect_bytes': len(json.dumps(record)),
            'bots_listing_ms': round(listing_ms, 1), 'listed': rows,
            'stored_declared_bytes_per_bot': stored[0], 'tool_lists': lists[0],
            'tool_lists_bytes': lists[1], 'daemon_rss_kib': rss,
        }


if __name__ == '__main__':
    unittest.main()
