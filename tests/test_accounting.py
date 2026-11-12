"""Budgets, turn listings, results, artifact reads by the model, idle exit, schema versions."""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from bench.runtime_client import node_item, poll, serve_args
from bench.targets import clean_env
from tests.test_runtime import ModelFixture

ROOT = Path(__file__).resolve().parent.parent


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AccountingTests(ModelFixture):
    def test_failed_attempt_usage_counts_and_a_spent_budget_retries_without_tools(self):
        client = self.client('echo')
        # The first attempt fails and reports 100 in, 10 out: the bot's first
        # call, all of it new. Its retry re-reads what it sent and is charged
        # its output; a call after it, its output and its share for the
        # tool's result it sends new. A budget that attempt spent sends the retry without
        # tool calls, and the turn ends on its reply; one the retry leaves
        # room in lets the turn run as asked.
        for budget, calls, limit in ((100, 2, 'budget'), (220, 3, None)):
            with self.subTest(budget=budget):
                prior_calls = self.model.requests.qsize()
                bot = f'Budget{budget}'
                client.request('create', bot=bot, workspace=str(self.path), budget_tokens=budget,
                               budget_counts='work')
                turn = client.request('submit', bot=bot, request_id='run',
                                      prompt=f'tool:billed-retry:{budget}')['result']['message']
                data = client.finished(turn)['data']
                self.assertEqual((data['status'], data.get('limit')), ('completed', limit))
                row = client.request('turns', bot=bot)['result']['turns'][0]
                self.assertEqual((row['model_rounds'], row['input_tokens'], row['output_tokens']),
                                 (calls, calls * 100, calls * 10))
                work = row['work_tokens']
                if calls == 2:
                    self.assertEqual(work, 120)
                else:
                    self.assertTrue(130 < work < 220, work)
                self.assertEqual(row['retries'], 1)
                self.assertEqual(self.model.requests.qsize() - prior_calls, calls)
                bot_row = client.request('inspect', bot=bot)['result']
                self.assertEqual((bot_row['tokens_used'], bot_row['work_tokens']), (calls * 110, work))
                self.assertEqual(client.request('stats')['result']['tokens']['input_tokens'],
                                 (prior_calls + calls) * 100)
                page = client.request('events', bot=bot, after=0, limit=256)['result']['events']
                self.assertEqual(sum(e['event'] == 'tool_completed' for e in page), int(budget > 100))

    def tool_output(self, client, bot, call_id):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        node = [e for e in events if e['event'] == 'tool_completed' and e['data']['call_id'] == call_id][-1]['data']['node']
        return node_item(client, bot, node)['result']['output']

    def test_budget_refuses_submissions_and_ends_turns_on_a_reply(self):
        client = self.client('echo,shell,wait')
        self.assertEqual(client.request('create', bot='Zero', workspace=str(self.path), budget_tokens=0)['error'],
                         'invalid_budget')
        self.assertEqual(client.request('create', bot='Odd', workspace=str(self.path),
                                        budget_counts='work')['error'], 'invalid_budget')
        # Each synthetic call sends 100 and writes 10. Counting work, the
        # bot's first call is all new; a later one is charged its output and
        # the share of its request it sends new (a prompt, a tool's result),
        # the history it sends again free.
        bot = client.request('create', bot='Bob', workspace=str(self.path), budget_tokens=115,
                             budget_counts='work')['result']
        self.assertEqual((bot['budget_tokens'], bot['budget_counts'], bot['tokens_used'], bot['work_tokens']),
                         (115, 'work', 0, 0))
        first = client.request('submit', bot='Bob', request_id='1', prompt='hello')['result']['message']
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        self.assertEqual(client.request('inspect', bot='Bob')['result']['work_tokens'], 110)
        # A tool turn's first call spends the budget: its tool runs, and the
        # call after goes without tool calls, so the turn ends on a reply,
        # completed at its budget rather than failing.
        second = client.request('submit', bot='Bob', request_id='2', prompt='tool:x')['result']['message']
        end = client.finished(second)['data']
        self.assertEqual((end['status'], end['error'], end['limit']), ('completed', None, 'budget'))
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual(sum(e['event'] == 'tool_completed' and e['turn'] == second for e in events), 1)
        refused = client.request('submit', bot='Bob', request_id='3', prompt='again')
        self.assertEqual(refused['error'], 'budget_exhausted')
        work = refused['work_tokens']
        self.assertTrue(130 < work < 200, work)
        self.assertIn(f'{work} of 115 tokens of work done', refused['detail'])
        self.assertEqual(refused['budget_counts'], 'work')
        listing = client.request('turns', bot='Bob')['result']
        self.assertEqual([t['status'] for t in listing['turns']], ['completed', 'completed'])
        self.assertEqual([t.get('limit') for t in listing['turns']], [None, 'budget'])
        self.assertEqual([t['input_tokens'] for t in listing['turns']], [100, 200])
        self.assertEqual([t['work_tokens'] for t in listing['turns']], [110, work - 110])
        self.assertEqual([t['model_rounds'] for t in listing['turns']], [1, 2])
        self.assertTrue(all(t['started_ms'] <= t['finished_ms'] for t in listing['turns']))
        self.assertEqual(listing['turns'][0]['workspace'], str(self.path.resolve()))
        self.assertEqual(listing['turns'][0]['model'], 'openai/synthetic-model')
        # Forks carry their own budget, not the parent's work.
        checkpoint = [e for e in client.request('events', bot='Bob', after=0, limit=256)['result']['events']
                      if e['event'] == 'turn_finished'][0]['data']['checkpoint']
        fork = client.request('fork', source='Bob', checkpoint=checkpoint, bot='Fork', budget_tokens=500,
                              budget_counts='work')['result']
        self.assertEqual((fork['budget_tokens'], fork['budget_counts'], fork['tokens_used'], fork['work_tokens']),
                         (500, 'work', 0, 0))
        free = client.request('fork', source='Bob', checkpoint=checkpoint, bot='Free')['result']
        self.assertEqual((free['budget_tokens'], free['budget_counts']), (None, 'tokens'))
        # A resumed task reloads the durable work rather than resetting it:
        # the call after its wait is its last.
        client.request('create', bot='Parked', workspace=str(self.path), budget_tokens=110,
                       budget_counts='work')
        parked = client.request('submit', bot='Parked', request_id='park', prompt='wait:proc:999999')['result']['message']
        end = client.finished(parked)['data']
        self.assertEqual((end['status'], end['limit']), ('completed', 'budget'))
        row = client.request('turns', bot='Parked')['result']['turns'][0]
        self.assertEqual(row['model_rounds'], 2)
        self.assertTrue(120 < row['work_tokens'] < 200, row)

    def test_a_turns_own_cap_ends_it_on_a_reply(self):
        client = self.client('echo,shell,wait')
        client.request('create', bot='Bob', workspace=str(self.path))
        submit = lambda key, prompt, **more: client.request(
            'submit', bot='Bob', request_id=key, prompt=prompt, **more)
        self.assertEqual(submit('zero', 'hello', budget_tokens=0)['error'], 'invalid_budget')
        self.assertEqual(submit('huge', 'hello', budget_tokens=2**63)['error'], 'invalid_budget')
        self.assertEqual(submit('steer', 'hello', budget_tokens=10, delivery='steer')['error'],
                         'invalid_delivery')
        # The bot's first call, 110 tokens, spends a cap of 110: its tool
        # still runs, and the turn ends on the reply after it.
        capped = submit('capped', 'tool:x', budget_tokens=110)['result']['message']
        end = client.finished(capped)['data']
        self.assertEqual((end['status'], end['limit']), ('completed', 'turn_budget'))
        self.assertEqual(submit('capped', 'tool:x', budget_tokens=220)['error'], 'idempotency_conflict')
        # The cap is the turn's: the next turn, capped or not, starts from
        # its own zero.
        bot = client.request('inspect', bot='Bob')['result']
        self.assertEqual(bot['tokens_used'], 220)
        self.assertTrue(120 < bot['work_tokens'] < 200, bot)
        for key, more in (('free', {}), ('roomy', {'budget_tokens': 220})):
            turn = submit(key, 'tool:x', **more)['result']['message']
            end = client.finished(turn)['data']
            self.assertEqual((end['status'], end.get('limit')), ('completed', None))
        listing = client.request('turns', bot='Bob')['result']['turns']
        self.assertEqual([t['budget_tokens'] for t in listing], [110, None, 220])
        self.assertEqual([t['model_rounds'] for t in listing], [2, 2, 2])
        work = [t['work_tokens'] for t in listing]
        self.assertEqual(work[0], bot['work_tokens'])
        self.assertTrue(all(20 < w < 100 for w in work[1:]), work)
        # A turn that resumes after parking counts what it did before: its
        # first call spent its cap, so the call after its wait is its last.
        parked = submit('park', 'wait:proc:999999', budget_tokens=10)['result']['message']
        end = client.finished(parked)['data']
        self.assertEqual((end['status'], end['limit']), ('completed', 'turn_budget'))
        # Under both caps the bot's own is named: it outlasts the turn.
        client.request('create', bot='Both', workspace=str(self.path), budget_tokens=110)
        both = client.request('submit', bot='Both', request_id='1', prompt='tool:x',
                              budget_tokens=110)['result']['message']
        self.assertEqual(client.finished(both)['data']['limit'], 'budget')
        # A cap spent before the turn's first call fails it: there is
        # nothing to report.
        self.assertEqual(client.request('submit', bot='Both', request_id='2', prompt='hello')['error'],
                         'budget_exhausted')

    def test_a_history_past_a_turns_cap_still_answers_it_cold(self):
        # The provider counts a request by its size, and caches nothing: a
        # bot whose history is many times a turn's cap, its cache long gone,
        # gets a message under that cap. Counting work, the turn is charged
        # what it adds, the message and the reply, not the history it sends
        # again; counting tokens, the default, the history spends the cap.
        self.model.bytes_per_token = 4
        # A reply as short as the ten tokens the provider reports for it.
        self.model.reply_text = 'noted'
        client = self.client('echo')
        client.request('create', bot='Elder', workspace=str(self.path))
        for i in range(4):
            turn = client.request('submit', bot='Elder', request_id=f'h{i}',
                                  prompt='history ' + 'x' * 40_000)['result']['message']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        history = client.request('turns', bot='Elder', after=0, limit=8)['result']['turns'][-1]
        self.assertGreater(history['input_tokens'], 40_000)
        invited = client.request('submit', bot='Elder', request_id='invite', prompt='tool:what do you think?',
                                 budget_tokens=10_000, budget_counts='work')['result']['message']
        end = client.finished(invited)['data']
        self.assertEqual((end['status'], end.get('limit')), ('completed', None))
        row = client.request('turns', bot='Elder', after=invited - 1, limit=1)['result']['turns'][0]
        # Two calls, each sending the whole history; charged a few hundred.
        self.assertEqual(row['model_rounds'], 2)
        self.assertGreater(row['input_tokens'], 80_000)
        self.assertLess(row['work_tokens'], 1_000)
        self.assertEqual(row['budget_counts'], 'work')
        counted = client.request('submit', bot='Elder', request_id='counted', prompt='tool:and now?',
                                 budget_tokens=10_000)['result']['message']
        end = client.finished(counted)['data']
        self.assertEqual((end['status'], end.get('limit')), ('completed', 'turn_budget'))
        row = client.request('turns', bot='Elder', after=counted - 1, limit=1)['result']['turns'][0]
        self.assertEqual(row['budget_counts'], 'tokens')

    def test_cache_hits_are_recorded_per_turn_per_bot_and_per_daemon(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        cold = client.request('submit', bot='Bob', request_id='1', prompt='hello')['result']['message']
        client.finished(cold)
        warm = client.request('submit', bot='Bob', request_id='2', prompt='cached:again')['result']['message']
        client.finished(warm)
        turns = {t['turn']: t for t in client.request('turns', bot='Bob', after=0)['result']['turns']}
        self.assertEqual((turns[cold]['cached_input_tokens'], turns[cold]['cache_hit']), (0, 0.0))
        self.assertEqual((turns[warm]['cached_input_tokens'], turns[warm]['cache_hit']), (40, 0.4))
        bot = client.request('inspect', bot='Bob')['result']
        self.assertEqual((bot['input_tokens'], bot['cached_input_tokens'], bot['cache_hit']), (200, 40, 0.2))
        listed = client.request('bots')['result']['bots'][0]
        self.assertEqual((listed['input_tokens'], listed['cache_hit']), (200, 0.2))
        tokens = client.request('stats')['result']['tokens']
        tokens = {key: tokens[key] for key in ('input_tokens', 'cached_input_tokens', 'output_tokens', 'cache_hit')}
        self.assertEqual(tokens, {'input_tokens': 200, 'cached_input_tokens': 40, 'output_tokens': 20, 'cache_hit': 0.2})

    def test_calls_are_timed_and_stats_say_what_the_daemon_holds(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        # The model takes half a second before it answers.
        turn = client.request('submit', bot='Bob', request_id='1', prompt='slow')['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        usage = next(e['data'] for e in events if e['event'] == 'usage')
        took, began = usage['model_ms'], usage['first_token_ms']
        self.assertTrue(500 <= began <= took < 5_000, usage)
        # The daemon's own time before the send is far below the model's.
        self.assertLess(usage.get('daemon_ms', 0), 500, usage)
        model = client.request('usage')['result']['models'][0]
        self.assertEqual((model['timed_rounds'], model['model_ms'], model['first_token_ms']), (1, took, began))
        self.assertEqual(model['timed_output_tokens'], model['output_tokens'])
        stats = client.request('stats')['result']
        self.assertEqual((stats['tokens']['timed_rounds'], stats['tokens']['model_ms']), (1, took))
        process = stats['process']
        self.assertTrue(process['rss_bytes'] > 0 and process['threads'] >= 1, process)
        self.assertTrue(process['cpu_user_ms'] >= 0 and process['uptime_ms'] > 0, process)
        # Each kind of request, by how long it held the daemon.
        submit = stats['commands']['ops']['submit']
        self.assertEqual((submit['count'], submit['failed'], sum(submit['held'])), (1, 0, 1))
        self.assertEqual(len(submit['held']), len(stats['commands']['buckets_us']) + 1)

    def test_a_cut_reply_s_usage_is_durable_and_the_spent_budget_wraps_its_turn_up(self):
        client = self.client('echo')
        client.request('create', bot='Bob', workspace=str(self.path), budget_tokens=100)
        turn = client.request('submit', bot='Bob', request_id='first', prompt='incomplete')['result']['message']
        end = client.finished(turn)
        # The model's cap cut the reply short: it is dropped, its usage kept,
        # and with the budget spent the turn's one more call, told why, ends it.
        self.assertEqual((end['data']['status'], end['data']['limit']), ('completed', 'budget'))
        self.assertEqual(client.request('inspect', bot='Bob')['result']['tokens_used'], 220)
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual(len([e for e in events if e['event'] == 'usage']), 2)
        self.assertEqual([e['data'] for e in events if e['event'] == 'dropped'],
                         [{'limit': 'max_tokens', 'output_tokens': 10}])
        self.assertEqual(client.request('stats')['result']['tokens']['input_tokens'], 200)
        self.assertEqual(len([e for e in events if e['event'] == 'message']), 1)
        client.close()
        client = self.client('echo')
        self.assertEqual(client.request('submit', bot='Bob', request_id='retry', prompt='hello')['error'],
                         'budget_exhausted')
        row = client.request('turns', bot='Bob')['result']['turns'][0]
        self.assertEqual((row['input_tokens'], row['output_tokens'], row['model_rounds']), (200, 20, 2))

    def test_rejected_completion_counts_usage_once_in_daemon_totals(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='cached:reused-call')['result']['message']
        end = client.finished(turn)['data']
        # The second round reuses the first round's call id.
        self.assertEqual((end['error'], end['detail']), ('invalid_tool_call_id', None))
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual(next(e['data'] for e in events if e['event'] == 'turn_finished'), end)
        row = client.request('turns', bot='Bob')['result']['turns'][0]
        totals = client.request('stats')['result']['tokens']
        totals = {key: totals[key] for key in ('input_tokens', 'cached_input_tokens', 'output_tokens', 'cache_hit')}
        self.assertEqual(totals, {'input_tokens': 200, 'cached_input_tokens': 80,
                                  'output_tokens': 20, 'cache_hit': 0.4})
        self.assertEqual({key: row[key] for key in totals}, totals)

    def test_wait_reports_outcomes_and_turns_report_live_status(self):
        client = self.client('echo')
        client.request('create', bot='Bob', workspace=str(self.path))
        self.assertEqual(poll(client, 'Bob', 99)['error'], 'turn_not_found')
        turn = client.request('submit', bot='Bob', request_id='w', prompt='wait')['result']['message']
        self.model.requests.get(timeout=3)
        self.assertEqual(poll(client, 'Bob', turn)['result'], {'message': turn, 'finished': False})
        live = client.request('turns', bot='Bob', after=turn - 1, limit=1)['result']['turns'][0]
        self.assertEqual((live['turn'], live['status']), (turn, 'running'))
        client.request('interrupt', bot='Bob', turn=turn)
        client.finished(turn)
        done = poll(client, 'Bob', turn)['result']
        self.assertEqual((done['status'], done['error']), ('interrupted', 'cancelled'))
        client.request('create', bot='Other', workspace=str(self.path))
        self.assertEqual(poll(client, 'Other', turn)['error'], 'turn_not_found')

    def test_model_reads_its_own_retained_output_through_the_read_tool(self):
        client = self.client('echo,shell,read')
        client.request('create', bot='Bob', workspace=str(self.path))
        big = client.request('submit', bot='Bob', request_id='big',
                             prompt="shell:i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done")['result']['message']
        self.assertEqual(client.finished(big)['data']['status'], 'completed')
        checkpoint = client.request('inspect', bot='Bob')['result']['head']
        client.request('fork', source='Bob', checkpoint=checkpoint, bot='Fork', workspace=str(self.path))
        inherited = client.request('submit', bot='Fork', request_id='inherited',
                                   prompt=f'readart:{big}/shell-1/stdout 19999,5')['result']['message']
        client.finished(inherited)
        self.assertIn('line-19998', self.tool_output(client, 'Fork', 'readart-1'))
        raw = client.request('artifact', bot='Fork', turn=big, call_id='shell-1', stream='stdout', offset=0, limit=64)
        self.assertIn('line-0', raw['result']['text'])
        result = json.loads(self.tool_output(client, 'Bob', 'shell-1'))
        self.assertIn('read them with the read tool', result['stdout'])
        self.assertEqual(result['artifacts'], [f'{big}/shell-1/stdout'])
        page = client.request('submit', bot='Bob', request_id='page',
                              prompt=f'readart:{big}/shell-1/stdout 19999,5')['result']['message']
        self.assertEqual(client.finished(page)['data']['status'], 'completed')
        text = self.tool_output(client, 'Bob', 'readart-1')
        self.assertIn(' 19999\tline-19998\n', text)
        self.assertIn(' 20000\tline-19999\n', text)
        self.assertNotIn('line-20000', text)
        missing = client.request('submit', bot='Bob', request_id='missing',
                                 prompt=f'readart:{big}/shell-1/stderr')['result']['message']
        self.assertEqual(client.finished(missing)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.tool_output(client, 'Bob', 'readart-1'))['error'], 'artifact_not_found')
        client.request('create', bot='Other', workspace=str(self.path))
        foreign = client.request('submit', bot='Other', request_id='foreign',
                                 prompt=f'readart:{big}/shell-1/stdout')['result']['message']
        self.assertEqual(client.finished(foreign)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.tool_output(client, 'Other', 'readart-1'))['error'], 'turn_not_found')

        # Later source output is not inherited by this historical fork.
        later = client.request('submit', bot='Bob', request_id='later',
                               prompt="shell:printf later")['result']['message']
        client.finished(later)
        denied = client.request('submit', bot='Fork', request_id='later-denied',
                                prompt=f'readart:{later}/shell-1/stdout')['result']['message']
        client.finished(denied)
        self.assertEqual(json.loads(self.tool_output(client, 'Fork', 'readart-1'))['error'], 'turn_not_found')

    def test_pruned_artifacts_answer_a_retention_error_to_the_owner_and_its_forks(self):
        client = self.client('echo,shell,read')
        client.request('create', bot='Bob', workspace=str(self.path))
        big = client.request('submit', bot='Bob', request_id='big',
                             prompt="shell:i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done")['result']['message']
        self.assertEqual(client.finished(big)['data']['status'], 'completed')
        checkpoint = client.request('inspect', bot='Bob')['result']['head']
        client.request('fork', source='Bob', checkpoint=checkpoint, bot='Fork', workspace=str(self.path))
        client.request('create', bot='Other', workspace=str(self.path))
        later = client.request('submit', bot='Bob', request_id='later', prompt='later')['result']['message']
        client.finished(later)
        self.assertIn('result', client.request('prune', bot='Bob', keep_turns=1))
        for bot in ('Bob', 'Fork'):
            self.assertEqual(client.request('artifact', bot=bot, turn=big, call_id='shell-1', stream='stdout',
                                            offset=0, limit=64)['error'], 'artifact_pruned')
        # Lineage still decides who is told: an unrelated bot and an unanswered call see no turn.
        self.assertEqual(client.request('artifact', bot='Other', turn=big, call_id='shell-1', stream='stdout')['error'], 'turn_not_found')
        self.assertEqual(client.request('artifact', bot='Fork', turn=big, call_id='shell-9', stream='stdout')['error'], 'turn_not_found')
        read = client.request('submit', bot='Fork', request_id='read',
                              prompt=f'readart:{big}/shell-1/stdout 1,5')['result']['message']
        self.assertEqual(client.finished(read)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.tool_output(client, 'Fork', 'readart-1'))['error'], 'artifact_pruned')
        # Deleting the producer removes what it owned; the fork's inherited output is still reported as pruned.
        self.assertIn('result', client.request('delete', bot='Bob'))
        self.assertEqual(client.request('artifact', bot='Fork', turn=big, call_id='shell-1', stream='stdout')['error'], 'artifact_pruned')
        self.assertEqual(client.request('submit', bot='Bob', request_id='big', prompt='retry')['error'], 'bot_not_found')

    def test_cut_outputs_go_whole_to_the_outputs_folder_and_leave_with_their_turn(self):
        # Outside the temporary directories, which a sandbox may write.
        (ROOT / '.local').mkdir(exist_ok=True)
        parent = Path(tempfile.mkdtemp(dir=ROOT / '.local'))
        self.addCleanup(shutil.rmtree, parent)
        outputs = parent / 'outputs'
        client = self.client('echo,shell,read')
        lines = "shell:i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done"
        expected = ''.join(f'line-{i}\n' for i in range(20000))

        def run(bot, request_id, prompt=lines):
            turn = client.request('submit', bot=bot, request_id=request_id, prompt=prompt)['result']['message']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            return turn

        client.request('create', bot='Bob', workspace=str(self.path), settings={'outputs': str(outputs)})
        big = run('Bob', 'big')
        result = json.loads(self.tool_output(client, 'Bob', 'shell-1'))
        self.assertEqual(result['artifacts'], [f'{big}/shell-1/stdout'])
        file = Path(result['files'][0])
        self.assertEqual(file, outputs / f'{big}-shell-1-stdout')
        self.assertEqual(file.read_text(), expected)
        self.assertEqual((outputs.stat().st_mode & 0o777, file.stat().st_mode & 0o777), (0o700, 0o600))
        # One copy: the store names the file and holds none of it.
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            self.assertEqual(db.execute('SELECT length(data),file FROM artifacts WHERE turn=?', (big,)).fetchall(),
                             [(0, str(file))])
        page = client.request('artifact', bot='Bob', turn=big, call_id='shell-1', stream='stdout',
                              offset=7, limit=12)['result']
        self.assertEqual((page['text'], page['next_offset']), (expected[7:19], 19))
        # Bytes that are not UTF-8 come back as replacement characters, up
        # to three times as many bytes, and still read whole.
        wide = run('Bob', 'wide', "shell:head -c 400000 /dev/zero | tr '\\0' '\\377'")
        page = client.request('artifact', bot='Bob', turn=wide, call_id='shell-1', stream='stdout',
                              offset=1199997, limit=8)['result']
        self.assertEqual((page['text'], page['done']), ('\ufffd', True))
        read = run('Bob', 'read', f'readart:{big}/shell-1/stdout 1,2')
        self.assertIn('line-1', self.tool_output(client, 'Bob', 'readart-1'))
        # Pruning the turn removes its file; reading it says so.
        run('Bob', 'later', 'later')
        self.assertIn('result', client.request('prune', bot='Bob', keep_turns=1))
        self.assertEqual(os.listdir(outputs), [])
        self.assertFalse(file.exists())
        self.assertEqual(client.request('artifact', bot='Bob', turn=big, call_id='shell-1', stream='stdout')['error'],
                         'artifact_pruned')
        self.assertLess(big, read)
        if not (shutil.which('bwrap') or sys.platform == 'darwin'):
            return
        # A sandbox that may write the folder keeps its output in the store,
        # so no command can put a link there for the daemon to follow.
        client.request('create', bot='Kid', workspace=str(parent),
                       settings={'outputs': str(outputs), 'sandbox': 'workspace-write'})
        kid = run('Kid', 'big')
        result = json.loads(self.tool_output(client, 'Kid', 'shell-1'))
        self.assertEqual((result['artifacts'], 'files' in result), ([f'{kid}/shell-1/stdout'], False))
        self.assertEqual(os.listdir(outputs), [])
        self.assertEqual(client.request('artifact', bot='Kid', turn=kid, call_id='shell-1', stream='stdout',
                                        offset=0, limit=10)['result']['text'], expected[:10])
        # One that cannot writes there, and deleting the bot removes the file.
        client.request('create', bot='Ann', workspace=str(self.path),
                       settings={'outputs': str(outputs), 'sandbox': 'workspace-write'})
        ann = run('Ann', 'big')
        file = Path(json.loads(self.tool_output(client, 'Ann', 'shell-1'))['files'][0])
        self.assertEqual(file.read_text(), expected)
        self.assertIn('result', client.request('delete', bot='Ann'))
        self.assertFalse(file.exists())
        self.assertNotEqual(ann, kid)

    def test_a_batch_stopped_before_its_results_are_recorded_leaves_no_output_file(self):
        (ROOT / '.local').mkdir(exist_ok=True)
        parent = Path(tempfile.mkdtemp(dir=ROOT / '.local'))
        self.addCleanup(shutil.rmtree, parent)
        outputs = parent / 'outputs'
        client = self.client('echo,shell')
        client.request('create', bot='Bob', workspace=str(self.path), settings={'outputs': str(outputs)})
        big = 'i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done'

        def batch(request_id, first):
            calls = [['shell', {'command': first}], ['shell', {'command': big}]]
            return client.request('submit', bot='Bob', request_id=request_id,
                                  prompt='calls:' + json.dumps(calls))['result']['message']

        def until(done, what):
            deadline = time.monotonic() + 30
            while not done():
                self.assertLess(time.monotonic(), deadline, what)
                time.sleep(0.02)

        # A result recorded in its batch keeps its file.
        kept = batch('kept', 'sleep .2')
        self.assertEqual(client.finished(kept)['data']['status'], 'completed')
        self.assertEqual(os.listdir(outputs), [f'{kept}-shell-2-stdout'])
        # The second call's file is written while its result waits behind
        # the first, still running; the turn stops before either is
        # recorded, so no record names the file and it goes with the turn.
        stopped = batch('stopped', 'sleep 30')
        written = outputs / f'{stopped}-shell-2-stdout'
        until(written.exists, 'the second call wrote its output')
        time.sleep(0.2)  # its result reaches the queue behind the first
        self.assertIn('result', client.request('interrupt', bot='Bob', turn=stopped))
        self.assertEqual(client.finished(stopped)['data']['status'], 'interrupted')
        until(lambda: not written.exists(), 'the stopped batch removed its output file')
        self.assertEqual(os.listdir(outputs), [f'{kept}-shell-2-stdout'])
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            self.assertEqual(db.execute('SELECT count(*) FROM artifacts WHERE turn=?', (stopped,)).fetchone(), (0,))

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
    def test_the_person_s_fallbacks_go_to_the_daemon_and_a_bot_keeps_only_its_own(self):
        self.model.models = ('synthetic-model', 'synthetic-other')
        (self.path / '.agent').mkdir()
        (self.path / '.agent/config.toml').write_text(
            '[fallbacks]\n"openai/synthetic-model" = "openai/synthetic-backup"\n')
        store = self.path / 'state.sqlite'
        common = ['--store', str(store), '--provider', f'openai=responses,{self.url}', '--detach']
        env = {**clean_env(), 'HOME': str(self.path)}

        def made(bot, *flags):
            run = subprocess.run([str(self.binary), 'run', *common, '--new', '--bot', bot, *flags, 'hello'],
                                 env=env, capture_output=True, text=True, timeout=30, cwd=self.path)
            self.assertEqual(run.returncode, 0, run.stderr)
            inspected = subprocess.run([str(self.binary), 'inspect', '--store', str(store), '--bot', bot],
                                       env=env, capture_output=True, text=True, timeout=10)
            return json.loads(inspected.stdout)['fallback']

        self.addCleanup(subprocess.run, [str(self.binary), 'shutdown', '--store', str(store)],
                        env=env, capture_output=True, timeout=5)
        # The daemon reads [fallbacks] at start; a bot keeps only the --fallback it was given.
        self.assertIsNone(made('Bob', '--model', 'openai/synthetic-model'))
        self.assertEqual(made('Ann', '--model', 'openai/synthetic-model', '--fallback', 'openai/other'),
                         'openai/other')
        with open(f'{store}.log') as log:
            ready = json.loads(log.readline())
        self.assertEqual(ready['fallbacks'], {'openai/synthetic-model': 'openai/synthetic-backup'}, ready)

    def test_socket_daemon_exits_when_idle_and_parked_turns_survive(self):
        store = self.path / 'state.sqlite'
        socket = self.path / 'state.sqlite.sock'
        common = ['--store', str(store), '--provider', f'openai=responses,{self.url}',
                  '--model', 'openai/synthetic-model', '--tools', 'echo,shell,wait', '--idle-exit', '1s']
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
        stopped = subprocess.run([str(self.binary), 'turns', *inspect, '--bot', 'Bob', '--no-spawn'],
                                 env=clean_env(), capture_output=True, text=True, timeout=10)
        self.assertEqual(stopped.returncode, 1)
        self.assertIn('daemon_unavailable', stopped.stderr)
        inspected = subprocess.run([str(self.binary), 'turns', *inspect, '--bot', 'Bob'],
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
        self.assertEqual([(t['turn'], t['status']) for t in turns], [(1, 'completed'), (2, 'completed')])
        subprocess.run([str(self.binary), 'shutdown', '--store', str(store)], env=clean_env(), capture_output=True, timeout=5)
