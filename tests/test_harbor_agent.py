"""The Harbor adapter's command and accounting; needs Harbor's Python 3.12 env."""
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

try:
    from harbor.models.agent.context import AgentContext
    from bench.harbor_agent import Agent
except ImportError:  # Harbor is not a bench dependency; see docs/HARBOR.md
    Agent = None


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
        self.assertTrue(command.endswith('exit $status'))

    def test_a_provider_spec_forwards_its_named_key(self):
        spec = 'gw=responses,https://gw.example.test/v1,GW_KEY'
        with tempfile.TemporaryDirectory() as logs, mock.patch.dict(os.environ, {'GW_KEY': 'k'}):
            agent = self.agent(logs, provider=spec)
            env = agent._env()
            command = agent._command('task')
        self.assertEqual(env['GW_KEY'], 'k')
        self.assertEqual(env['AGENT_STORE'], '/tmp/agent-harbor/state.sqlite')
        self.assertIn(f'--provider {spec} ', command)

    def test_tokens_come_from_daemon_totals(self):
        with tempfile.TemporaryDirectory() as logs:
            Path(logs, 'stats.json').write_text(json.dumps({'tokens': {
                'input_tokens': 900, 'cached_input_tokens': 600, 'output_tokens': 70}}))
            Path(logs, 'turns.json').write_text(json.dumps([{
                'model_rounds': 3, 'retries': 1, 'paced_ms': 5, 'status': 'completed'}]))
            context = AgentContext()
            self.agent(logs).populate_context_post_run(context)
        self.assertEqual((context.n_input_tokens, context.n_cache_tokens, context.n_output_tokens),
                         (900, 600, 70))
        self.assertEqual(context.metadata, {'model_rounds': 3, 'retries': 1, 'paced_ms': 5,
                                            'status': ['completed']})

    def test_a_timed_out_trial_counts_streamed_usage(self):
        usage = {'input_tokens': 100, 'cached_input_tokens': 40, 'output_tokens': 7}
        with tempfile.TemporaryDirectory() as logs:
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
