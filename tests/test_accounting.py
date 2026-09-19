"""Budgets, turn listings, results, artifact reads by the model, idle exit, schema versions."""
import json
import os
import sqlite3
import subprocess
import time
import unittest

from bench.runtime_client import Client, serve_args
from bench.targets import clean_env
from tests.test_runtime import ModelFixture


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AccountingTests(ModelFixture):
    def test_failed_attempt_usage_limits_retries_and_later_tool_rounds(self):
        client = self.client('echo')
        # Each attempt reports 110 tokens. Check before the retry, after a
        # successful tool call, and allow completion with sufficient budget.
        for budget, calls, status in ((100, 1, 'failed'), (220, 2, 'failed'), (330, 3, 'completed')):
            with self.subTest(budget=budget):
                prior_calls = self.model.requests.qsize()
                bot = f'Budget{budget}'
                client.request('create', bot=bot, workspace=str(self.path), budget_tokens=budget)
                turn = client.request('submit', bot=bot, request_id='run',
                                      prompt=f'tool:billed-retry:{budget}')['result']['turn']
                data = client.finished(turn)['data']
                self.assertEqual(data['status'], status)
                if status == 'failed':
                    self.assertEqual(data['error'], 'budget_exhausted')
                row = client.request('turns', bot=bot)['result']['turns'][0]
                self.assertEqual((row['model_rounds'], row['input_tokens'], row['output_tokens']),
                                 (calls, calls * 100, calls * 10))
                self.assertEqual(row['retries'], int(calls > 1))
                self.assertEqual(self.model.requests.qsize() - prior_calls, calls)
                self.assertEqual(client.request('resume', bot=bot)['result']['tokens_used'], calls * 110)
                self.assertEqual(client.request('stats')['result']['tokens']['input_tokens'],
                                 (prior_calls + calls) * 100)
                page = client.request('events', bot=bot, after=0, limit=256)['result']['events']
                self.assertEqual(sum(e['event'] == 'tool_completed' for e in page), int(calls > 1))

    def tool_output(self, client, bot, call_id):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        node = [e for e in events if e['event'] == 'tool_completed' and e['data']['call_id'] == call_id][-1]['data']['node']
        return client.request('item', bot=bot, node=node)['result']['output']

    def test_budget_refuses_submissions_and_stops_turns_before_the_next_call(self):
        client = self.client('echo,shell,wait')
        self.assertEqual(client.request('create', bot='Zero', workspace=str(self.path), budget_tokens=0)['error'],
                         'invalid_budget')
        # Each synthetic call costs 110 tokens; a 200-token budget allows one call
        # per turn and refuses the third turn outright.
        bot = client.request('create', bot='Bob', workspace=str(self.path), budget_tokens=200)['result']
        self.assertEqual((bot['budget_tokens'], bot['tokens_used']), (200, 0))
        first = client.request('submit', bot='Bob', request_id='1', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        self.assertEqual(client.request('resume', bot='Bob')['result']['tokens_used'], 110)
        # A tool turn needs two calls: the second is refused before it starts,
        # so the tool ran, but the turn fails without an uncertain tool.
        second = client.request('submit', bot='Bob', request_id='2', prompt='tool:x')['result']['turn']
        end = client.finished(second)
        self.assertEqual((end['data']['status'], end['data']['error']), ('failed', 'budget_exhausted'))
        self.assertIn('220 of 200', end['data']['detail'])
        self.assertEqual(client.request('submit', bot='Bob', request_id='3', prompt='again')['error'], 'budget_exhausted')
        listing = client.request('turns', bot='Bob')['result']
        self.assertEqual([t['status'] for t in listing['turns']], ['completed', 'failed'])
        # Turn 2 made one call (100 in, 10 out) before the check refused the second.
        self.assertEqual([t['input_tokens'] for t in listing['turns']], [100, 100])
        self.assertEqual([t['model_rounds'] for t in listing['turns']], [1, 1])
        self.assertTrue(all(t['started_ms'] <= t['finished_ms'] for t in listing['turns']))
        self.assertEqual(listing['turns'][0]['workspace'], str(self.path.resolve()))
        self.assertEqual(listing['turns'][0]['model'], 'openai/synthetic-model')
        # Forks carry their own budget, not the parent's usage.
        checkpoint = [e for e in client.request('events', bot='Bob', after=0, limit=256)['result']['events']
                      if e['event'] == 'turn_finished'][0]['data']['checkpoint']
        fork = client.request('fork', source='Bob', checkpoint=checkpoint, bot='Fork', budget_tokens=500)['result']
        self.assertEqual((fork['budget_tokens'], fork['tokens_used']), (500, 0))
        free = client.request('fork', source='Bob', checkpoint=checkpoint, bot='Free')['result']
        self.assertIsNone(free['budget_tokens'])
        # A resumed task reloads the durable total rather than resetting its budget.
        client.request('create', bot='Parked', workspace=str(self.path), budget_tokens=110)
        parked = client.request('submit', bot='Parked', request_id='park', prompt='wait:proc:999999')['result']['turn']
        self.assertEqual(client.finished(parked)['data']['error'], 'budget_exhausted')
        row = client.request('turns', bot='Parked')['result']['turns'][0]
        self.assertEqual((row['input_tokens'], row['output_tokens'], row['model_rounds']), (100, 10, 1))


    def test_cache_hits_are_recorded_per_turn_per_bot_and_per_daemon(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        cold = client.request('submit', bot='Bob', request_id='1', prompt='hello')['result']['turn']
        client.finished(cold)
        warm = client.request('submit', bot='Bob', request_id='2', prompt='cached:again')['result']['turn']
        client.finished(warm)
        turns = {t['turn']: t for t in client.request('turns', bot='Bob', after=0)['result']['turns']}
        self.assertEqual((turns[cold]['cached_input_tokens'], turns[cold]['cache_hit']), (0, 0.0))
        self.assertEqual((turns[warm]['cached_input_tokens'], turns[warm]['cache_hit']), (40, 0.4))
        bot = client.request('resume', bot='Bob')['result']
        self.assertEqual((bot['input_tokens'], bot['cached_input_tokens'], bot['cache_hit']), (200, 40, 0.2))
        listed = client.request('bots')['result']['bots'][0]
        self.assertEqual((listed['input_tokens'], listed['cache_hit']), (200, 0.2))
        tokens = client.request('stats')['result']['tokens']
        self.assertEqual(tokens, {'input_tokens': 200, 'cached_input_tokens': 40, 'output_tokens': 20, 'cache_hit': 0.2})

    def test_incomplete_usage_is_durable_and_exhausts_budget(self):
        client = self.client('echo')
        client.request('create', bot='Bob', workspace=str(self.path), budget_tokens=100)
        turn = client.request('submit', bot='Bob', request_id='first', prompt='incomplete')['result']['turn']
        end = client.finished(turn)
        self.assertEqual(end['data']['error'], 'provider_incomplete')
        self.assertEqual(client.request('resume', bot='Bob')['result']['tokens_used'], 110)
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual(len([e for e in events if e['event'] == 'usage']), 1)
        self.assertEqual(client.request('stats')['result']['tokens']['input_tokens'], 100)
        self.assertFalse(any(e['event'] == 'message' for e in events))
        client.close()
        client = self.client('echo')
        self.assertEqual(client.request('submit', bot='Bob', request_id='retry', prompt='hello')['error'],
                         'budget_exhausted')
        row = client.request('turns', bot='Bob')['result']['turns'][0]
        self.assertEqual((row['input_tokens'], row['output_tokens'], row['model_rounds']), (100, 10, 1))

    def test_rejected_completion_counts_usage_once_in_daemon_totals(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='cached:reused-call')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['error'], 'storage_error')
        row = client.request('turns', bot='Bob')['result']['turns'][0]
        totals = client.request('stats')['result']['tokens']
        self.assertEqual(totals, {'input_tokens': 200, 'cached_input_tokens': 80,
                                  'output_tokens': 20, 'cache_hit': 0.4})
        self.assertEqual({key: row[key] for key in totals}, totals)

    def test_result_reports_outcomes_and_live_status(self):
        client = self.client('echo')
        client.request('create', bot='Bob', workspace=str(self.path))
        self.assertEqual(client.request('result', bot='Bob', turn=99)['error'], 'turn_not_found')
        turn = client.request('submit', bot='Bob', request_id='w', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        live = client.request('result', bot='Bob', turn=turn)['result']
        self.assertEqual((live['status'], live['finished']), ('running', False))
        client.request('interrupt', bot='Bob', turn=turn)
        client.finished(turn)
        done = client.request('result', bot='Bob', turn=turn)['result']
        self.assertEqual((done['status'], done['error']), ('interrupted', 'cancelled'))
        client.request('create', bot='Other', workspace=str(self.path))
        self.assertEqual(client.request('result', bot='Other', turn=turn)['error'], 'turn_not_found')

    def test_model_reads_its_own_retained_output_through_the_read_tool(self):
        client = self.client('echo,shell,read')
        client.request('create', bot='Bob', workspace=str(self.path))
        big = client.request('submit', bot='Bob', request_id='big',
                             prompt="shell:i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done")['result']['turn']
        self.assertEqual(client.finished(big)['data']['status'], 'completed')
        checkpoint = client.request('resume', bot='Bob')['result']['head']
        client.request('fork', source='Bob', checkpoint=checkpoint, bot='Fork', workspace=str(self.path))
        inherited = client.request('submit', bot='Fork', request_id='inherited',
                                   prompt=f'readart:{big}/shell-1/stdout 19999,5')['result']['turn']
        client.finished(inherited)
        self.assertIn('line-19998', self.tool_output(client, 'Fork', 'readart-1'))
        raw = client.request('artifact', bot='Fork', turn=big, call_id='shell-1', stream='stdout', offset=0, limit=64)
        self.assertIn('line-0', raw['result']['text'])
        result = json.loads(self.tool_output(client, 'Bob', 'shell-1'))
        self.assertIn('read them with the read tool', result['stdout'])
        self.assertEqual(result['artifacts'], [f'{big}/shell-1/stdout'])
        page = client.request('submit', bot='Bob', request_id='page',
                              prompt=f'readart:{big}/shell-1/stdout 19999,5')['result']['turn']
        self.assertEqual(client.finished(page)['data']['status'], 'completed')
        text = self.tool_output(client, 'Bob', 'readart-1')
        self.assertIn(' 19999\tline-19998\n', text)
        self.assertIn(' 20000\tline-19999\n', text)
        self.assertNotIn('line-20000', text)
        missing = client.request('submit', bot='Bob', request_id='missing',
                                 prompt=f'readart:{big}/shell-1/stderr')['result']['turn']
        self.assertEqual(client.finished(missing)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.tool_output(client, 'Bob', 'readart-1'))['error'], 'artifact_not_found')
        client.request('create', bot='Other', workspace=str(self.path))
        foreign = client.request('submit', bot='Other', request_id='foreign',
                                 prompt=f'readart:{big}/shell-1/stdout')['result']['turn']
        self.assertEqual(client.finished(foreign)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.tool_output(client, 'Other', 'readart-1'))['error'], 'turn_not_found')

        # Later source output is not inherited by this historical fork.
        later = client.request('submit', bot='Bob', request_id='later',
                               prompt="shell:printf later")['result']['turn']
        client.finished(later)
        denied = client.request('submit', bot='Fork', request_id='later-denied',
                                prompt=f'readart:{later}/shell-1/stdout')['result']['turn']
        client.finished(denied)
        self.assertEqual(json.loads(self.tool_output(client, 'Fork', 'readart-1'))['error'], 'turn_not_found')

    def test_pruned_artifacts_answer_a_retention_error_to_the_owner_and_its_forks(self):
        client = self.client('echo,shell,read')
        client.request('create', bot='Bob', workspace=str(self.path))
        big = client.request('submit', bot='Bob', request_id='big',
                             prompt="shell:i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done")['result']['turn']
        self.assertEqual(client.finished(big)['data']['status'], 'completed')
        checkpoint = client.request('resume', bot='Bob')['result']['head']
        client.request('fork', source='Bob', checkpoint=checkpoint, bot='Fork', workspace=str(self.path))
        client.request('create', bot='Other', workspace=str(self.path))
        later = client.request('submit', bot='Bob', request_id='later', prompt='later')['result']['turn']
        client.finished(later)
        self.assertIn('result', client.request('prune', bot='Bob', keep_turns=1))
        for bot in ('Bob', 'Fork'):
            self.assertEqual(client.request('artifact', bot=bot, turn=big, call_id='shell-1')['error'], 'artifact_pruned')
            self.assertEqual(client.request('artifact', bot=bot, turn=big, call_id='shell-1', stream='stdout',
                                            offset=0, limit=64)['error'], 'artifact_pruned')
        # Lineage still decides who is told: an unrelated bot and an unanswered call see no turn.
        self.assertEqual(client.request('artifact', bot='Other', turn=big, call_id='shell-1')['error'], 'turn_not_found')
        self.assertEqual(client.request('artifact', bot='Fork', turn=big, call_id='shell-9')['error'], 'turn_not_found')
        read = client.request('submit', bot='Fork', request_id='read',
                              prompt=f'readart:{big}/shell-1/stdout 1,5')['result']['turn']
        self.assertEqual(client.finished(read)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.tool_output(client, 'Fork', 'readart-1'))['error'], 'artifact_pruned')
        # Deleting the producer removes what it owned; the fork's inherited output is still reported as pruned.
        self.assertIn('result', client.request('delete', bot='Bob'))
        self.assertEqual(client.request('artifact', bot='Fork', turn=big, call_id='shell-1')['error'], 'artifact_pruned')
        self.assertEqual(client.request('submit', bot='Bob', request_id='big', prompt='retry')['error'], 'bot_not_found')

    def test_unversioned_and_newer_stores_are_refused_with_clear_codes(self):
        path = self.path / 'old.sqlite'
        with sqlite3.connect(path) as db:
            db.execute('CREATE TABLE bots(name TEXT PRIMARY KEY)')
        old = subprocess.run([str(self.binary), *serve_args(path, self.url)], input='', capture_output=True,
                             text=True, timeout=5, env=clean_env())
        self.assertEqual(old.returncode, 1)
        self.assertIn('store_schema_unsupported', old.stderr)
        newer = self.path / 'newer.sqlite'
        with sqlite3.connect(newer) as db:
            db.execute('PRAGMA user_version = 999')
        result = subprocess.run([str(self.binary), *serve_args(newer, self.url)], input='', capture_output=True,
                                text=True, timeout=5, env=clean_env())
        self.assertEqual(result.returncode, 1)
        self.assertIn('store_schema_newer', result.stderr)


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class IdleExitTests(ModelFixture):
    def test_socket_daemon_exits_when_idle_and_parked_turns_survive(self):
        store = self.path / 'state.sqlite'
        socket = self.path / 'state.sqlite.sock'
        common = ['--store', str(store), '--provider', f'openai=responses,{self.url}',
                  '--model', 'openai/synthetic-model', '--tools', 'echo,shell,wait', '--idle-exit', '1']
        run = subprocess.run([str(self.binary), 'run', *common, '--new', '--bot', 'Bob', 'hello'],
                             env=clean_env(), capture_output=True, text=True, timeout=30, cwd=self.path)
        self.assertEqual(run.returncode, 0, run.stderr)
        with open(f'{store}.log') as log:
            ready = json.loads(log.readline())
        self.assertEqual(ready['limits']['idle_exit_seconds'], 1)
        deadline = time.monotonic() + 6
        while socket.exists() and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertFalse(socket.exists(), 'daemon should exit when idle')
        # Inspection restarts without submitting any model work. Respect --no-spawn.
        creation = ('--model', '--tools')
        inspect = [flag for i, flag in enumerate(common) if flag not in creation and common[i - 1] not in creation]
        stopped = subprocess.run([str(self.binary), 'result', *inspect, '--bot', 'Bob', '--turn', '1', '--no-spawn'],
                                 env=clean_env(), capture_output=True, text=True, timeout=10)
        self.assertEqual(stopped.returncode, 1)
        self.assertIn('daemon_unavailable', stopped.stderr)
        for command in ('result', 'turns'):
            args = ['--turn', '1'] if command == 'result' else []
            inspected = subprocess.run([str(self.binary), command, *inspect, '--bot', 'Bob', *args],
                                       env=clean_env(), capture_output=True, text=True, timeout=10)
            self.assertEqual(inspected.returncode, 0, inspected.stderr)
            self.assertTrue(json.loads(inspected.stdout))
            subprocess.run([str(self.binary), 'shutdown', '--store', str(store)],
                           env=clean_env(), capture_output=True, timeout=5, check=True)
        self.assertEqual(self.model.requests.qsize(), 1)
        # A later command restarts the daemon and continues the same bot.
        again = subprocess.run([str(self.binary), 'run', *inspect, '--bot', 'Bob', 'tool:again'],
                               env=clean_env(), capture_output=True, text=True, timeout=30, cwd=self.path)
        self.assertEqual(again.returncode, 0, again.stderr)
        turns = json.loads(subprocess.run([str(self.binary), 'turns', '--store', str(store), '--bot', 'Bob'],
                                          env=clean_env(), capture_output=True, text=True, timeout=10).stdout)
        self.assertEqual([t['turn'] for t in turns], [1, 2])
        outcome = json.loads(subprocess.run([str(self.binary), 'result', '--store', str(store), '--bot', 'Bob', '--turn', '2'],
                                            env=clean_env(), capture_output=True, text=True, timeout=10).stdout)
        self.assertEqual(outcome['status'], 'completed')
        subprocess.run([str(self.binary), 'shutdown', '--store', str(store)], env=clean_env(), capture_output=True, timeout=5)
