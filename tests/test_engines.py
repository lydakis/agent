"""Optional real-engine checks: AGENT_BENCH_TEST_ENGINES=1 enables Pi, Codex and Rust."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest

from bench.config import workload
from bench.runner import run_once
from bench.matrix import guards
from bench.targets import clean_env, engine_protocol, engine_target, validate_responses_workload


class EngineGuardTests(unittest.TestCase):
    def test_full_history_resends_have_their_own_size_limit(self):
        config = workload('bench/workloads/smoke.json')
        validate_responses_workload(config)
        config.update(turns=1000, history_bytes=65536)
        with self.assertRaises(ValueError):
            validate_responses_workload(config)

    def test_environment_allowlist_excludes_auth_proxy_and_node_injection(self):
        from unittest.mock import patch
        with patch.dict(os.environ, {'OPENAI_API_KEY': 'synthetic', 'NODE_OPTIONS': '--inspect',
                                     'HTTPS_PROXY': 'synthetic', 'PATH': '/synthetic'}):
            env = clean_env()
        self.assertNotIn('OPENAI_API_KEY', env)
        self.assertNotIn('NODE_OPTIONS', env)
        self.assertNotIn('HTTPS_PROXY', env)
        self.assertEqual(env['PATH'], '/synthetic')

    @unittest.skipUnless(shutil.which('node'), 'Node.js is needed for the adapter check')
    def test_codex_shutdown_failure_is_not_a_successful_run(self):
        root = Path(__file__).resolve().parent.parent
        config = workload(root / 'bench/workloads/smoke.json')
        config.update(concurrency=1, turns=1, chunks=1, chunk_bytes=8)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            backend = path / 'fake-codex'
            backend.write_text(f'#!{sys.executable}\n' + '''
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if 'id' not in m:
        continue
    result = {}
    if m['method'] == 'thread/start': result = {'thread': {'id': 'bob'}}
    if m['method'] == 'turn/start': result = {'turn': {'id': 'turn-0'}}
    print(json.dumps({'id': m['id'], 'result': result}), flush=True)
    if m['method'] == 'turn/start':
        print(json.dumps({'method': 'item/agentMessage/delta', 'params': {
            'threadId': 'bob', 'turnId': 'turn-0', 'delta': 'xxxxxxxx'}}), flush=True)
        print(json.dumps({'method': 'turn/completed', 'params': {
            'threadId': 'bob', 'turn': {'id': 'turn-0', 'status': 'completed'}}}), flush=True)
sys.exit(7)
''')
            backend.chmod(0o700)
            env = {**clean_env(), 'AGENT_BENCH_PORT': '1',
                   'AGENT_BENCH_WORKLOAD': json.dumps(config),
                   'AGENT_BENCH_EXECUTABLE': str(backend), 'AGENT_BENCH_STATE': directory,
                   'AGENT_BENCH_WORKSPACE': directory}
            completed = subprocess.run(['node', str(root / 'bench/adapters/codex.mjs')],
                                       env=env, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL, timeout=5)
            self.assertNotEqual(completed.returncode, 0)

    def run_fake_claude(self, mode):
        root = Path(__file__).resolve().parent.parent
        config = workload(root / 'bench/workloads/smoke.json')
        config.update(concurrency=2, turns=2, chunks=2, chunk_bytes=4)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            backend = path / 'fake-claude'
            # Speaks the stream-json subset the adapter relies on.
            backend.write_text(f'#!{sys.executable}\n' + '''
import json, os, sys
mode = os.environ['FAKE_MODE']
assert 'AGENT_BENCH_WORKLOAD' not in os.environ and os.environ['ANTHROPIC_BASE_URL']
def send(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    if m['type'] == 'control_request':
        send({'type': 'control_response', 'response': {
            'subtype': 'success', 'request_id': m['request_id'], 'response': {}}})
        continue
    send({'type': 'system', 'subtype': 'init', 'session_id': 's', 'tools': []})
    kind = 'tool_use' if mode == 'tool' else 'text'
    send({'type': 'stream_event', 'parent_tool_use_id': None, 'event': {
        'type': 'content_block_start', 'index': 0, 'content_block': {'type': kind}}})
    for _ in range(2):
        send({'type': 'stream_event', 'parent_tool_use_id': None, 'event': {
            'type': 'content_block_delta', 'index': 0,
            'delta': {'type': 'text_delta', 'text': 'xxxx'}}})
    send({'type': 'result', 'subtype': 'success', 'is_error': False,
          'stop_reason': 'end_turn', 'session_id': 's'})
sys.exit(7 if mode == 'exit' else 0)
''')
            backend.chmod(0o700)
            env = {**clean_env(), 'AGENT_BENCH_PORT': '1', 'AGENT_BENCH_WORKLOAD': json.dumps(config),
                   'AGENT_BENCH_EXECUTABLE': str(backend), 'AGENT_BENCH_STATE': directory,
                   'AGENT_BENCH_WORKSPACE': directory, 'FAKE_MODE': mode}
            completed = subprocess.run(['node', str(root / 'bench/adapters/claude-code.mjs')],
                                       env=env, capture_output=True, text=True, timeout=10)
        return completed.returncode, [json.loads(line) for line in completed.stdout.splitlines()]

    @unittest.skipUnless(shutil.which('node'), 'Node.js is needed for the adapter check')
    def test_claude_code_adapter_drives_one_process_per_agent_and_rejects_tools(self):
        code, events = self.run_fake_claude('ok')
        self.assertEqual(code, 0)
        self.assertEqual(events[0], {'event': 'ready'})
        self.assertEqual(sum(e['event'] == 'turn_end' for e in events), 4)
        self.assertEqual(sum(e['event'] == 'chunk' for e in events), 8)
        for mode in ('tool', 'exit'):
            with self.subTest(mode=mode):
                self.assertNotEqual(self.run_fake_claude(mode)[0], 0)

    def test_process_per_agent_engines_get_per_agent_guards(self):
        self.assertEqual(guards('codex', 32), (512, 16))
        self.assertEqual(guards('claude-code', 1), (512, 16))
        self.assertEqual(guards('claude-code', 32), (512 * 32, 16 * 32))


@unittest.skipUnless(os.environ.get('AGENT_BENCH_TEST_ENGINES') == '1',
                     'set AGENT_BENCH_TEST_ENGINES=1 with pinned Pi and Codex installed')
class EngineIntegrationTests(unittest.TestCase):
    def check_engine(self, engine):
        root = Path(__file__).resolve().parent.parent
        command, metadata, executable = engine_target(engine, root)
        config = workload(root / 'bench/workloads/smoke.json')
        rss_limit, process_limit = guards(engine, config['concurrency'])
        options = SimpleNamespace(timeout=20, interval=.1, discovery_interval=.25,
                                  rss_limit_mib=rss_limit, process_limit=process_limit,
                                  protocol=engine_protocol(engine), executable=executable,
                                  driver='daemon' if engine == 'rust' else None)
        # All native state is synthetic; no personal configuration is passed.
        with tempfile.TemporaryDirectory(dir=root / '.local') as directory:
            path = Path(directory)
            (path / 'workload.json').write_text(json.dumps(config))
            result = run_once(command, config, options, path, 0)
        self.assertEqual(result['status'], 'ok', result)
        self.assertEqual(result['provider']['completed_requests'], 12)
        self.assertEqual(result['provider']['peak_active_requests'], 4)
        self.assertEqual(result['provider']['invalid_requests'], 0)
        self.assertEqual(result['provider']['output_text_bytes'], 61440)
        self.assertEqual(result['events']['completed_turns'], 12)
        self.assertGreater(result['provider']['response_body_bytes'], 61440)
        if engine == 'claude-code':
            # Adapter plus one CLI per agent; short git probes may also be sampled.
            self.assertGreaterEqual(result['target']['processes'], 5)
            self.assertEqual(result['provider']['preconnect_requests'], 4)
        else:
            self.assertEqual(result['target']['processes'], 2 if engine == 'codex' else 1)
        self.assertEqual(metadata['durability'], 'sqlite_full' if engine == 'rust' else 'ephemeral')
        if engine == 'fx':
            self.assertEqual(result['provider']['catalog_requests'], 4)
            self.assertEqual(metadata['backend'], 'native')
            self.assertGreater(result['target']['threads'], 4)

    def test_pi_core_with_real_responses_transport(self):
        self.check_engine('pi')

    def test_shared_codex_app_server_with_real_responses_transport(self):
        self.check_engine('codex')

    def test_rust_core_with_real_responses_transport(self):
        self.check_engine('rust')


@unittest.skipUnless(os.environ.get('AGENT_BENCH_TEST_FX') == '1',
                     'set AGENT_BENCH_TEST_FX=1 with pinned libfx installed')
class FxIntegrationTests(unittest.TestCase):
    def test_native_fx_with_real_gateway_transport_and_retained_history(self):
        EngineIntegrationTests.check_engine(self, 'fx')


@unittest.skipUnless(os.environ.get('AGENT_BENCH_TEST_CLAUDE_CODE') == '1',
                     'set AGENT_BENCH_TEST_CLAUDE_CODE=1 with pinned Claude Code under .local')
class ClaudeCodeIntegrationTests(unittest.TestCase):
    def test_claude_code_process_per_agent_with_real_messages_transport(self):
        EngineIntegrationTests.check_engine(self, 'claude-code')
