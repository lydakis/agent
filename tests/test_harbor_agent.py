"""The Harbor adapter's command and accounting; needs Harbor's Python 3.12 env."""
import asyncio
import json
import os
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


class Environment:
    """Answers the adapter's execs; the task command runs until cancelled."""

    def __init__(self, bots):
        self.bots = bots
        self.commands = []

    async def exec(self, command, user=None, env=None, cwd=None, timeout_sec=None):
        self.commands.append(command)
        if 'agent run ' in command:
            await asyncio.Event().wait()
        stdout = json.dumps([{'name': bot} for bot in self.bots]) if command.endswith('agent ls') else ''
        return ExecResult(stdout=stdout, return_code=0)


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
        def turn(model, tokens, status='completed'):
            return {'model': model, 'input_tokens': tokens, 'cached_input_tokens': tokens // 2,
                    'output_tokens': tokens // 10, 'model_rounds': 2, 'retries': 1,
                    'paced_ms': 5, 'status': status}
        rates = {'gw/m': {'input_cost_per_token': 1e-6, 'output_cost_per_token': 1e-5,
                          'cache_read_input_token_cost': 1e-7},
                 'gw/small': {'input_cost_per_token': 1e-7, 'output_cost_per_token': 1e-6}}
        with tempfile.TemporaryDirectory() as logs, \
                mock.patch.dict('litellm.model_cost', rates):
            Path(logs, 'turns.json').write_text(json.dumps({
                'task': [turn('gw/m', 900), turn('gw/m', 100, 'cancelled')],
                'helper': [turn('gw/small', 2000)]}))
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
            unpriced = AgentContext()
            with mock.patch.dict('litellm.model_cost', {}, clear=True):
                self.agent(logs).populate_context_post_run(unpriced)
        self.assertEqual((context.n_input_tokens, context.n_cache_tokens, context.n_output_tokens),
                         (3000, 1500, 300))
        usage = context.model_usage
        self.assertEqual((usage['gw/m'].n_input_tokens, usage['gw/small'].n_input_tokens), (1000, 2000))
        self.assertAlmostEqual(usage['gw/m'].cost_usd, 500e-6 + 500e-7 + 100e-5)
        # No cached rate: cached input is priced as input.
        self.assertAlmostEqual(usage['gw/small'].cost_usd, 2000e-7 + 200e-6)
        self.assertAlmostEqual(context.cost_usd, usage['gw/m'].cost_usd + usage['gw/small'].cost_usd)
        self.assertEqual(context.metadata, {'model_rounds': 6, 'retries': 3, 'paced_ms': 15,
                                            'status': ['completed', 'cancelled'], 'bots': 2})
        self.assertIsNone(unpriced.cost_usd)
        self.assertEqual(unpriced.n_output_tokens, 300)

    def test_bookkeeping_records_every_bot_then_waits_for_the_daemon(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            logs, store, bin_ = root / 'logs', root / 'store', root / 'bin'
            for directory in (logs, store, bin_):
                directory.mkdir()
            (store / 'state.sqlite').write_text('db')
            # Prints what the daemon would and logs each call.
            fake = bin_ / 'agent'
            fake.write_text(f"""#!/bin/sh
echo "$*" >> {root}/calls
case "$1" in
  turns) printf '[%s]' "${{#3}}" ;;
  stats) echo '{{"tokens":{{}}}}' ;;
esac
""")
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            with mock.patch.object(harbor_agent.EnvironmentPaths, 'agent_dir', logs), \
                    mock.patch.object(harbor_agent, 'REMOTE_STORE', str(store)):
                command = self.agent(logs)._finish_command(['task', 'it\'s "odd"'])
            subprocess.run(['bash', '-c', command], check=True,
                           env={**os.environ, 'PATH': f'{bin_}:{os.environ["PATH"]}'})
            self.assertEqual(json.loads((logs / 'turns.json').read_text()),
                             {'task': [4], 'it\'s "odd"': [10]})
            self.assertEqual((root / 'calls').read_text().splitlines()[-2:],
                             ['stats --no-spawn', 'shutdown'])
            self.assertEqual((logs / 'state.sqlite').read_text(), 'db')
            empty = self.agent(logs)
            with mock.patch.object(harbor_agent.EnvironmentPaths, 'agent_dir', logs), \
                    mock.patch.object(harbor_agent, 'REMOTE_STORE', str(store)):
                command = empty._finish_command([])
            subprocess.run(['bash', '-c', command], check=True,
                           env={**os.environ, 'PATH': f'{bin_}:{os.environ["PATH"]}'})
            self.assertEqual(json.loads((logs / 'turns.json').read_text()), {})

    def test_a_timed_out_trial_still_stops_the_daemon(self):
        environment = Environment(['task', 'helper'])
        with tempfile.TemporaryDirectory() as logs:
            agent = self.agent(logs)

            async def trial():
                await asyncio.wait_for(agent.run('task', environment, AgentContext()), 0.1)
            with self.assertRaises(TimeoutError):
                asyncio.run(trial())
        run, listing, finish = environment.commands
        self.assertIn('agent run ', run)
        self.assertTrue(listing.endswith('agent ls'))
        self.assertIn("agent turns --bot helper --no-spawn", finish)
        self.assertIn('agent shutdown && cp ', finish)

    def test_unreadable_turn_records_fall_back_to_streamed_usage(self):
        usage = {'input_tokens': 100, 'cached_input_tokens': 40, 'output_tokens': 7}
        with tempfile.TemporaryDirectory() as logs:
            # A turns listing that failed partway.
            Path(logs, 'turns.json').write_text('{"task":')
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
            Path(logs, 'turns.json').write_text('')
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
        self.assertTrue(context.is_empty())


if __name__ == '__main__':
    unittest.main()
