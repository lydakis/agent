"""The Harbor adapter's command and accounting; needs Harbor's Python 3.12 env."""
import asyncio
import json
import os
import socket
import sqlite3
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

try:
    from harbor.environments.base import ExecResult
    from harbor.models.agent.context import AgentContext
    from bench import harbor_agent
    from bench.harbor_agent import Agent
except ImportError:  # Harbor is not a bench dependency; see docs/HARBOR.md
    Agent = None
from tests.test_runtime import ModelFixture


class Environment:
    """Answers the adapter's execs; the task command runs until cancelled."""

    def __init__(self):
        self.commands = []

    async def exec(self, command, user=None, env=None, cwd=None, timeout_sec=None):
        self.commands.append(command)
        if 'agent run ' in command:
            await asyncio.Event().wait()
        return ExecResult(stdout='', return_code=0)


def store(path, turns):
    """The columns of a store's bots and turns that accounting reads."""
    with sqlite3.connect(path) as db:
        db.execute('CREATE TABLE bots(name TEXT, provider TEXT, model TEXT, reasoning TEXT, '
                   'fallbacks INT)')
        db.execute('CREATE TABLE turns(id INTEGER PRIMARY KEY, bot TEXT, model TEXT, status TEXT, '
                   'input_tokens INT, cached_input_tokens INT, output_tokens INT, '
                   'model_rounds INT, retries INT, paced_ms INT)')
        # The task bot as the adapter creates it; others as a model might.
        db.executemany('INSERT INTO bots VALUES (?,?,?,?,?)',
                       {(t[0], 'gw', 'm', *(('high', 1) if t[0] == 'task' else (None, 0)))
                        for t in turns})
        db.executemany('INSERT INTO turns(bot,model,status,input_tokens,cached_input_tokens,'
                       'output_tokens,model_rounds,retries,paced_ms) VALUES (?,?,?,?,?,?,2,1,5)',
                       [(bot, model, status, n, n // 2, n // 10) for bot, model, status, n in turns])


def counted(logs, input_tokens):
    """The daemon's own input count, as `agent stats` saves it before shutdown."""
    Path(logs, 'stats.json').write_text(json.dumps({'tokens': {'input_tokens': input_tokens}}))


@unittest.skipIf(Agent is None, 'harbor is not installed')
class HarborAgentTest(unittest.TestCase):
    def agent(self, logs, **kwargs):
        return Agent(logs_dir=Path(logs), model_name='gw/m', **kwargs)

    def test_command_quotes_the_instruction_and_keeps_the_turn_status(self):
        with tempfile.TemporaryDirectory() as logs:
            command = self.agent(logs, reasoning='high', max_output_tokens=4096)._command(
                "it's \"quoted\"; rm -rf /\n$(x)")
        self.assertIn("-- 'it'\"'\"'s \"quoted\"; rm -rf /\n$(x)' < /dev/null", command)
        self.assertIn('--reasoning high --max-output-tokens 4096 --', command)
        self.assertTrue(command.endswith('| tee /logs/agent/agent.jsonl; exit ${PIPESTATUS[0]}'))

    def test_a_provider_spec_forwards_its_named_key(self):
        spec = 'gw=responses,https://gw.example.test/v1,GW_KEY'
        with tempfile.TemporaryDirectory() as logs, mock.patch.dict(os.environ, {'GW_KEY': 'k'}):
            agent = self.agent(logs, provider=spec)
            env = agent._env()
            command = agent._command('task')
        self.assertEqual(env['GW_KEY'], 'k')
        self.assertEqual(env['AGENT_STORE'], '/tmp/agent-harbor/state.sqlite')
        self.assertIn(f'--provider {spec} ', command)

    def test_the_login_is_uploaded_only_when_the_daemon_signs_in(self):
        with tempfile.TemporaryDirectory() as logs:
            def signs_in(**kwargs):
                return Agent(logs_dir=Path(logs), model_name='chatgpt/m', **kwargs)._chatgpt
            self.assertTrue(signs_in())
            self.assertTrue(signs_in(provider=f'chatgpt=responses,{harbor_agent.CHATGPT_URL}'))
            self.assertFalse(signs_in(provider='chatgpt=responses,https://gw.example.test/v1,GW_KEY'))
            self.assertFalse(signs_in(provider='chatgpt=responses,http://127.0.0.1:9/v1'))
            self.assertFalse(signs_in(provider=f'chatgpt=responses,{harbor_agent.CHATGPT_URL},K'))
            self.assertFalse(self.agent(logs)._chatgpt)

    def test_a_chatgpt_model_signs_in_with_the_codex_login(self):
        with tempfile.TemporaryDirectory() as logs:
            auth = Path(logs, 'auth.json')
            auth.write_text(json.dumps({'OPENAI_API_KEY': None, 'tokens': {
                'id_token': 'i', 'access_token': 'a', 'refresh_token': 'r', 'account_id': 'w'}}))
            agent = Agent(logs_dir=Path(logs), model_name='chatgpt/m', codex_auth=str(auth))
            command, env = agent._command('task'), agent._env()
            # The container gets what a request carries, never the refresh token.
            self.assertEqual(agent._chatgpt_login(), {'access_token': 'a', 'account_id': 'w'})
            auth.write_text(json.dumps({'tokens': {'access_token': 'a'}}))
            with self.assertRaisesRegex(ValueError, 'no ChatGPT login'):
                agent._chatgpt_login()
        self.assertIn('--model chatgpt/m --provider chatgpt --', command)
        self.assertEqual(env['CODEX_HOME'], '/installed-agent/codex')
        with tempfile.TemporaryDirectory() as logs:
            self.assertNotIn('CODEX_HOME', self.agent(logs)._env())

    def test_each_model_is_priced_at_its_own_rates(self):
        rates = {'gw/m': {'input_cost_per_token': 1e-6, 'output_cost_per_token': 1e-5,
                          'cache_read_input_token_cost': 1e-7},
                 'gw/small': {'input_cost_per_token': 1e-7, 'output_cost_per_token': 1e-6}}
        with tempfile.TemporaryDirectory() as logs, \
                mock.patch.dict('litellm.model_cost', rates):
            store(Path(logs, 'state.sqlite'), [('task', 'gw/m', 'completed', 900),
                                               ('helper', 'gw/small', 'cancelled', 2000),
                                               ('task', None, 'interrupted', 100)])
            counted(logs, 0)
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
            unpriced = AgentContext()
            with mock.patch.dict('litellm.model_cost', {}, clear=True):
                self.agent(logs).populate_context_post_run(unpriced)
        self.assertEqual((context.n_input_tokens, context.n_cache_tokens, context.n_output_tokens),
                         (3000, 1500, 300))
        usage = context.model_usage
        # A turn with no model of its own ran on its bot's.
        self.assertEqual((usage['gw/m'].n_input_tokens, usage['gw/small'].n_input_tokens), (1000, 2000))
        self.assertAlmostEqual(usage['gw/m'].cost_usd, 500e-6 + 500e-7 + 100e-5)
        # No cached rate: cached input is priced as input.
        self.assertAlmostEqual(usage['gw/small'].cost_usd, 2000e-7 + 200e-6)
        self.assertAlmostEqual(context.cost_usd, usage['gw/m'].cost_usd + usage['gw/small'].cost_usd)
        self.assertEqual(context.metadata, {'model_rounds': 6, 'retries': 3, 'paced_ms': 15,
                                            'status': ['completed', 'interrupted'], 'bots': 2,
                                            'bot_settings': {
                                                'task': {'reasoning': 'high', 'fallbacks': True},
                                                'helper': {'reasoning': None, 'fallbacks': False}},
                                            'requested_model': 'gw/m', 'served_calls': {}})
        self.assertIsNone(unpriced.cost_usd)
        self.assertEqual(unpriced.n_output_tokens, 300)

    def test_a_fallback_attempt_is_priced_at_the_model_that_ran_it(self):
        rates = {'gw/m': {'input_cost_per_token': 1e-6, 'output_cost_per_token': 1e-5},
                 'gw/backup': {'input_cost_per_token': 2e-6, 'output_cost_per_token': 2e-5}}
        with tempfile.TemporaryDirectory() as logs, \
                mock.patch.dict('litellm.model_cost', rates):
            path = Path(logs, 'state.sqlite')
            store(path, [('task', 'gw/m', 'completed', 1000)])
            attempts = [{'model': 'm', 'input_tokens': 300, 'output_tokens': 20, 'cached_input_tokens': 0},
                        {'model': 'backup', 'input_tokens': 400, 'output_tokens': 30, 'cached_input_tokens': 100}]
            with sqlite3.connect(path) as db:
                db.execute('CREATE TABLE events(id INTEGER PRIMARY KEY, bot TEXT, turn INT, kind TEXT, data TEXT)')
                db.execute("INSERT INTO events(bot,turn,kind,data) VALUES ('task',1,'usage',?)",
                           (json.dumps({'input_tokens': 700, 'output_tokens': 50, 'cached_input_tokens': 100,
                                        'models': attempts}),))
                db.execute("INSERT INTO events(bot,turn,kind,data) VALUES ('task',1,'usage',?)",
                           (json.dumps({'input_tokens': 300, 'output_tokens': 50, 'cached_input_tokens': 400}),))
            counted(logs, 1000)
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
            # The daemon counted more than the store holds: a bot was deleted.
            counted(logs, 1500)
            short = AgentContext()
            self.agent(logs).populate_context_post_run(short)
        usage = context.model_usage
        self.assertEqual((usage['gw/m'].n_input_tokens, usage['gw/m'].n_cache_tokens, usage['gw/m'].n_output_tokens),
                         (600, 400, 70))
        self.assertEqual((usage['gw/backup'].n_input_tokens, usage['gw/backup'].n_cache_tokens,
                          usage['gw/backup'].n_output_tokens), (400, 100, 30))
        # Totals are unchanged; only the split moves.
        self.assertEqual((context.n_input_tokens, context.n_output_tokens), (1000, 100))
        # The trial names what it asked for and every model that answered.
        self.assertEqual((context.metadata['requested_model'], context.metadata['served_calls'],
                          context.metadata['bot_settings']['task']['fallbacks']),
                         ('gw/m', {'gw/m': 2, 'gw/backup': 1}, True))
        self.assertNotIn('unrecorded_input_tokens', context.metadata)
        self.assertEqual((short.metadata['served_calls'], short.metadata['unrecorded_input_tokens']),
                         (None, 500))

    def test_cache_writes_are_priced_at_the_write_rate(self):
        rates = {'gw/m': {'input_cost_per_token': 1e-6, 'output_cost_per_token': 1e-5,
                          'cache_read_input_token_cost': 1e-7, 'cache_creation_input_token_cost': 1.25e-6},
                 'gw/backup': {'input_cost_per_token': 2e-6, 'output_cost_per_token': 2e-5,
                               'cache_creation_input_token_cost': 2.5e-6}}
        with tempfile.TemporaryDirectory() as logs, \
                mock.patch.dict('litellm.model_cost', rates):
            path = Path(logs, 'state.sqlite')
            store(path, [('task', 'gw/m', 'completed', 1000)])
            with sqlite3.connect(path) as db:
                db.execute('CREATE TABLE events(id INTEGER PRIMARY KEY, bot TEXT, turn INT, kind TEXT, data TEXT)')
                usage = [{'input_tokens': 600, 'output_tokens': 50, 'cached_input_tokens': 300,
                          'cache_write_tokens': 200},
                         {'input_tokens': 400, 'output_tokens': 50, 'cached_input_tokens': 200,
                          'cache_write_tokens': 100,
                          'models': [{'model': 'm', 'input_tokens': 100, 'output_tokens': 0,
                                      'cached_input_tokens': 0, 'cache_write_tokens': 0},
                                     {'model': 'backup', 'input_tokens': 300, 'output_tokens': 50,
                                      'cached_input_tokens': 200, 'cache_write_tokens': 100}]}]
                for data in usage:
                    db.execute("INSERT INTO events(bot,turn,kind,data) VALUES ('task',1,'usage',?)",
                               (json.dumps(data),))
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
        usage = context.model_usage
        # gw/m: 700 input of which 300 cached and 200 written; the fallback's
        # write is priced at the backup model's own write rate.
        self.assertAlmostEqual(usage['gw/m'].cost_usd, 200 * 1e-6 + 200 * 1.25e-6 + 300 * 1e-7 + 50 * 1e-5)
        self.assertAlmostEqual(usage['gw/backup'].cost_usd, 100 * 2.5e-6 + 200 * 2e-6 + 50 * 2e-5)

    def test_hour_long_cache_writes_are_priced_at_the_hour_rate(self):
        rates = {'gw/m': {'input_cost_per_token': 1e-6, 'output_cost_per_token': 1e-5,
                          'cache_creation_input_token_cost': 1.25e-6,
                          'cache_creation_input_token_cost_above_1hr': 2e-6},
                 'gw/plain': {'input_cost_per_token': 1e-6, 'output_cost_per_token': 1e-5}}
        for model, hour_rate in (('gw/m', 2e-6), ('gw/plain', 2e-6)):
            with self.subTest(model=model), tempfile.TemporaryDirectory() as logs, \
                    mock.patch.dict('litellm.model_cost', rates):
                path = Path(logs, 'state.sqlite')
                store(path, [('task', model, 'completed', 1000)])
                with sqlite3.connect(path) as db:
                    db.execute('CREATE TABLE events(id INTEGER PRIMARY KEY, bot TEXT, turn INT, kind TEXT, data TEXT)')
                    db.execute("INSERT INTO events(bot,turn,kind,data) VALUES ('task',1,'usage',?)",
                               (json.dumps({'input_tokens': 1000, 'output_tokens': 100, 'cached_input_tokens': 500,
                                            'cache_write_tokens': 400, 'cache_write_1h_tokens': 300}),))
                context = AgentContext()
                self.agent(logs).populate_context_post_run(context)
                write_rate = rates[model].get('cache_creation_input_token_cost', 1e-6)
                self.assertAlmostEqual(context.model_usage[model].cost_usd,
                                       100 * 1e-6 + 100 * write_rate + 300 * hour_rate + 500 * 1e-6 + 100 * 1e-5)

    def test_a_summary_is_priced_at_the_summarizers_provider_and_model(self):
        rates = {'gw/m': {'input_cost_per_token': 1e-6, 'output_cost_per_token': 1e-5},
                 'other/cheap': {'input_cost_per_token': 1e-7, 'output_cost_per_token': 1e-6,
                                 'cache_creation_input_token_cost': 2e-7}}
        with tempfile.TemporaryDirectory() as logs, \
                mock.patch.dict('litellm.model_cost', rates):
            path = Path(logs, 'state.sqlite')
            store(path, [('task', 'gw/m', 'completed', 1000)])
            summary = {'model': 'cheap', 'provider': 'other', 'input_tokens': 400, 'output_tokens': 40,
                       'cached_input_tokens': 0, 'cache_write_tokens': 100}
            with sqlite3.connect(path) as db:
                db.execute('CREATE TABLE events(id INTEGER PRIMARY KEY, bot TEXT, turn INT, kind TEXT, data TEXT)')
                db.execute("INSERT INTO events(bot,turn,kind,data) VALUES ('task',1,'usage',?)",
                           (json.dumps({**summary, 'purpose': 'compaction', 'models': [summary]}),))
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
        usage = context.model_usage
        self.assertEqual(sorted(usage), ['gw/m', 'other/cheap'])
        self.assertAlmostEqual(usage['other/cheap'].cost_usd, 300 * 1e-7 + 100 * 2e-7 + 40 * 1e-6)
        self.assertEqual(usage['gw/m'].n_input_tokens, 600)

    def test_a_turn_served_only_by_the_fallback_needs_no_price_for_the_requested_model(self):
        rates = {'gw/backup': {'input_cost_per_token': 2e-6, 'output_cost_per_token': 2e-5}}
        with tempfile.TemporaryDirectory() as logs, \
                mock.patch.dict('litellm.model_cost', rates, clear=True):
            path = Path(logs, 'state.sqlite')
            store(path, [('task', 'gw/m', 'completed', 1000)])
            with sqlite3.connect(path) as db:
                db.execute('CREATE TABLE events(id INTEGER PRIMARY KEY, bot TEXT, turn INT, kind TEXT, data TEXT)')
                db.execute("INSERT INTO events(bot,turn,kind,data) VALUES ('task',1,'usage',?)",
                           (json.dumps({'models': [{'model': 'backup', 'input_tokens': 1000, 'output_tokens': 100,
                                                    'cached_input_tokens': 500}]}),))
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
        self.assertEqual(list(context.model_usage), ['gw/backup'])
        self.assertAlmostEqual(context.cost_usd, 500 * 2e-6 + 500 * 2e-6 + 100 * 2e-5)

    def test_finishing_stops_the_daemon_before_copying_the_store(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            logs, remote, bin_ = root / 'logs', root / 'store', root / 'bin'
            for directory in (logs, remote, bin_):
                directory.mkdir()
            (remote / 'state.sqlite').write_text('db')
            fake = bin_ / 'agent'
            fake.write_text(f'#!/bin/sh\necho "$*" >> {root}/calls\n')
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            with mock.patch.object(harbor_agent.EnvironmentPaths, 'agent_dir', logs), \
                    mock.patch.object(harbor_agent, 'REMOTE_STORE', str(remote)):
                command = Agent._finish_command()
            env = {**os.environ, 'PATH': f'{bin_}:{os.environ["PATH"]}'}
            # The daemon never started: nothing to stop or save.
            subprocess.run(['bash', '-c', command], check=True, env=env)
            self.assertFalse((root / 'calls').exists())
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(str(remote / 'state.sqlite.sock'))
                subprocess.run(['bash', '-c', command], check=True, env=env)
            self.assertEqual((root / 'calls').read_text().splitlines(), ['stats --no-spawn', 'shutdown'])
            self.assertEqual((logs / 'state.sqlite').read_text(), 'db')

    def test_a_timed_out_trial_still_stops_the_daemon(self):
        environment = Environment()
        with tempfile.TemporaryDirectory() as logs:
            agent = self.agent(logs)

            async def trial():
                await asyncio.wait_for(agent.run('task', environment, AgentContext()), 0.1)
            with self.assertRaises(TimeoutError):
                asyncio.run(trial())
        run, finish = environment.commands
        self.assertIn('agent run ', run)
        self.assertIn('agent shutdown && cp ', finish)

    def test_unreadable_turn_records_fall_back_to_streamed_usage(self):
        usage = {'input_tokens': 100, 'cached_input_tokens': 40, 'output_tokens': 7}
        with tempfile.TemporaryDirectory() as logs:
            # A store the daemon never finished writing.
            Path(logs, 'state.sqlite').write_text('not a database')
            Path(logs, 'agent.jsonl').write_text('\n'.join(
                [json.dumps({'event': 'usage', 'data': usage})] * 2
                + [json.dumps({'event': 'text_delta', 'text': 'x'}), 'agent: diagnostic',
                   '{"event":"usage","data":{"input']))
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
        self.assertEqual((context.n_input_tokens, context.n_cache_tokens, context.n_output_tokens),
                         (200, 80, 14))
        # The stream is the task bot's alone, so what served delegated bots,
        # and how they were set up, is unknown rather than absent.
        self.assertEqual(context.metadata, {'requested_model': 'gw/m', 'served_calls': None})

    def test_streamed_usage_keeps_each_calls_model_split(self):
        rates = {'gw/m': {'input_cost_per_token': 1e-6, 'output_cost_per_token': 1e-5},
                 'other/cheap': {'input_cost_per_token': 1e-7, 'output_cost_per_token': 1e-6,
                                 'cache_creation_input_token_cost': 2e-7}}
        answer = {'input_tokens': 100, 'cached_input_tokens': 40, 'output_tokens': 7}
        split = {'model': 'cheap', 'provider': 'other', 'input_tokens': 400, 'output_tokens': 40,
                 'cached_input_tokens': 0, 'cache_write_tokens': 100}
        summary = {**split, 'purpose': 'compaction', 'models': [split]}
        with tempfile.TemporaryDirectory() as logs, \
                mock.patch.dict('litellm.model_cost', rates):
            Path(logs, 'agent.jsonl').write_text('\n'.join(
                json.dumps({'event': 'usage', 'data': data}) for data in (answer, summary)))
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
        usage = context.model_usage
        self.assertEqual((usage['gw/m'].n_input_tokens, usage['other/cheap'].n_input_tokens), (100, 400))
        self.assertAlmostEqual(usage['other/cheap'].cost_usd, 300 * 1e-7 + 100 * 2e-7 + 40 * 1e-6)

    def test_a_daemon_that_never_started_reports_nothing(self):
        with tempfile.TemporaryDirectory() as logs:
            Path(logs, 'agent.jsonl').write_text('')
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
        self.assertTrue(context.is_empty())


@unittest.skipIf(Agent is None, 'harbor is not installed')
@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class HarborStoreTest(ModelFixture):
    def test_accounting_reads_a_real_store(self):
        client = self.client()
        client.request('create', bot='task', workspace=str(self.path))
        turn = client.request('submit', bot='task', request_id='r', prompt='hello')['result']['turn']
        client.finished(turn)
        listed = client.request('turns', bot='task', after=0, limit=8)['result']['turns']
        Path(self.path, 'stats.json').write_text(json.dumps(client.request('stats')['result']))
        client.request('shutdown')
        client.close()
        context = AgentContext()
        Agent(logs_dir=self.path, model_name='openai/synthetic-model').populate_context_post_run(context)
        self.assertGreater(context.n_input_tokens, 0)
        self.assertEqual((context.n_input_tokens, context.n_output_tokens, context.metadata['status']),
                         (listed[0]['input_tokens'], listed[0]['output_tokens'], ['completed']))
        self.assertEqual(list(context.model_usage), ['openai/synthetic-model'])
        self.assertEqual(context.metadata['bot_settings'],
                         {'task': {'reasoning': None, 'fallbacks': False}})
        # The daemon's count and the stored usage events agree.
        self.assertEqual(context.metadata['served_calls'],
                         {'openai/synthetic-model': listed[0]['model_rounds']})
        self.assertNotIn('unrecorded_input_tokens', context.metadata)

    def test_a_deleted_helper_leaves_what_served_unknown(self):
        client = self.client()
        for bot in ('task', 'helper'):
            client.request('create', bot=bot, workspace=str(self.path))
            turn = client.request('submit', bot=bot, request_id=bot, prompt='hello')['result']['turn']
            client.finished(turn)
        helper = client.request('turns', bot='helper', after=0, limit=8)['result']['turns'][0]
        client.request('delete', bot='helper')
        Path(self.path, 'stats.json').write_text(json.dumps(client.request('stats')['result']))
        client.request('shutdown')
        client.close()
        context = AgentContext()
        Agent(logs_dir=self.path, model_name='openai/synthetic-model').populate_context_post_run(context)
        self.assertIsNone(context.metadata['served_calls'])
        self.assertEqual(context.metadata['unrecorded_input_tokens'], helper['input_tokens'])


if __name__ == '__main__':
    unittest.main()
