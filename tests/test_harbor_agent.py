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
        db.execute('CREATE TABLE bots(name TEXT, provider TEXT, model TEXT)')
        db.execute('CREATE TABLE turns(id INTEGER PRIMARY KEY, bot TEXT, model TEXT, status TEXT, '
                   'input_tokens INT, cached_input_tokens INT, output_tokens INT, '
                   'model_rounds INT, retries INT, paced_ms INT)')
        db.executemany('INSERT INTO bots VALUES (?,?,?)',
                       {(t[0], 'gw', 'm') for t in turns})
        db.executemany('INSERT INTO turns(bot,model,status,input_tokens,cached_input_tokens,'
                       'output_tokens,model_rounds,retries,paced_ms) VALUES (?,?,?,?,?,?,2,1,5)',
                       [(bot, model, status, n, n // 2, n // 10) for bot, model, status, n in turns])


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
                                            'status': ['completed', 'interrupted'], 'bots': 2})
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
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
        usage = context.model_usage
        self.assertEqual((usage['gw/m'].n_input_tokens, usage['gw/m'].n_cache_tokens, usage['gw/m'].n_output_tokens),
                         (600, 400, 70))
        self.assertEqual((usage['gw/backup'].n_input_tokens, usage['gw/backup'].n_cache_tokens,
                          usage['gw/backup'].n_output_tokens), (400, 100, 30))
        # Totals are unchanged; only the split moves.
        self.assertEqual((context.n_input_tokens, context.n_output_tokens), (1000, 100))

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
        client.request('shutdown')
        client.close()
        context = AgentContext()
        Agent(logs_dir=self.path, model_name='openai/synthetic-model').populate_context_post_run(context)
        self.assertGreater(context.n_input_tokens, 0)
        self.assertEqual((context.n_input_tokens, context.n_output_tokens, context.metadata['status']),
                         (listed[0]['input_tokens'], listed[0]['output_tokens'], ['completed']))
        self.assertEqual(list(context.model_usage), ['openai/synthetic-model'])


if __name__ == '__main__':
    unittest.main()
