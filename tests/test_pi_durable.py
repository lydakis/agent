"""Pi Durable lifecycle baseline: provider validation of another engine's
serialization, pin checks, and (with the pinned adapter installed,
AGENT_BENCH_TEST_PI_DURABLE=1) the adapter itself."""
import json
import os
import queue
from pathlib import Path
import shutil
import tempfile
import unittest

from bench.lifecycle import PI_DURABLE_ADAPTER, run_once
from bench.lifecycle_provider import canonical, tool_frames
from bench.runtime_client import Client
from bench.targets import clean_env, pi_durable_metadata

ROOT = Path(__file__).resolve().parent.parent
TEXT = 'x' * 16


def pi_request(*turns, tool=None):
    """A conversation as pi-ai's Responses client serializes it."""
    items = [{'role': 'system', 'content': '<instructions>\nTest agent.\n</instructions>'}]
    for prompt, answer in turns:
        items.append({'role': 'user', 'content': [{'type': 'input_text', 'text': prompt}]})
        if answer is not None:
            items.append({'type': 'message', 'role': 'assistant', 'id': 'msg_1', 'status': 'completed',
                          'content': [{'type': 'output_text', 'text': answer, 'annotations': []}]})
    if tool:
        items += tool
    return items


class ValidationTests(unittest.TestCase):
    def test_serialization_differences_compare_equal_but_content_does_not(self):
        provider_item = {'id': 'msg_1', 'type': 'message', 'role': 'assistant', 'status': 'completed',
                         'content': [{'type': 'output_text', 'text': TEXT, 'annotations': [], 'logprobs': []}]}
        expected = [{'role': 'user', 'content': [{'type': 'input_text', 'text': 'BENCH agent=0 turn=0'}]},
                    provider_item]
        request = pi_request(('BENCH agent=0 turn=0', TEXT))
        self.assertEqual(canonical(request), canonical([request[0], *expected]))
        changed = pi_request(('BENCH agent=0 turn=0', TEXT[:-1]))
        self.assertNotEqual(canonical(changed), canonical([changed[0], *expected]))
        other_agent = pi_request(('BENCH agent=1 turn=0', TEXT))
        self.assertNotEqual(canonical(other_agent), canonical([other_agent[0], *expected]))

    def test_instructions_are_required_and_unknown_items_rejected(self):
        with self.assertRaises(ValueError):
            canonical(pi_request(('BENCH agent=0 turn=0', None))[1:])
        with self.assertRaises(ValueError):
            canonical([*pi_request(('BENCH agent=0 turn=0', None)), {'type': 'reasoning', 'summary': []}])

    def test_tool_call_is_streamed_as_an_item_and_compares_by_arguments(self):
        frames, output = tool_frames(0, 0, 'bash', {'command': 'true', 'timeout': 2})
        kinds = [event['type'] for _, event in frames]
        self.assertEqual(kinds, ['response.created', 'response.output_item.added',
                                 'response.function_call_arguments.delta',
                                 'response.function_call_arguments.done',
                                 'response.output_item.done', 'response.completed'])
        self.assertEqual(frames[-2][1]['item'], output[0])
        # A client may reorder argument keys; the call is the same.
        resent = {**output[0], 'arguments': json.dumps({'timeout': 2, 'command': 'true'})}
        result = {'type': 'function_call_output', 'call_id': 'tool-0-0', 'output': 'tool-ok'}
        request = pi_request(('BENCH agent=0 turn=0', None), tool=[resent, result])
        self.assertEqual(canonical(request)[1:], [('function_call', 'tool-0-0', 'bash', {'command': 'true', 'timeout': 2}),
                                                  ('function_call_output', 'tool-0-0', 'tool-ok')])

    def test_missing_pinned_packages_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, 'install the pinned'):
                pi_durable_metadata(Path(directory))

    def test_only_agent_has_a_socket_transport(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):
                run_once(None, Path(directory), {}, 'echo', 'echo', 'socket', engine='pi-durable')


@unittest.skipUnless(os.environ.get('AGENT_BENCH_TEST_PI_DURABLE') == '1' and shutil.which('node'),
                     'set AGENT_BENCH_TEST_PI_DURABLE=1 with the pinned adapter dependencies installed')
class AdapterTests(unittest.TestCase):
    def start(self, store, synchronous='full'):
        env = {**clean_env(), 'AGENT_BENCH_PORT': '1', 'AGENT_BENCH_STORE': str(store),
               'AGENT_BENCH_SYNCHRONOUS': synchronous, 'AGENT_BENCH_TOOLS': 'echo,shell'}
        client = Client(None, None, None, env=env, command=[shutil.which('node'), str(PI_DURABLE_ADAPTER)])
        self.addCleanup(client.close, kill=True)
        return client

    def test_pinned_versions_and_source_revision_are_recorded(self):
        metadata = pi_durable_metadata(ROOT)
        self.assertEqual(metadata['pi-durable'], '1.0.0')
        self.assertEqual(metadata['pi-ai-durable'], '1.0.0')
        self.assertEqual(metadata['pi_durable_source_revision'], 'a13d35a742c6ef8462812a28fbe1d8c8b7431c32')

    def test_store_syncs_as_asked_and_reports_it(self):
        with tempfile.TemporaryDirectory(dir=ROOT / '.local') as directory:
            for synchronous in ('full', 'normal'):
                client = self.start(Path(directory) / f'{synchronous}.sqlite', synchronous)
                self.assertEqual(client.ready, {'event': 'ready', 'synchronous': synchronous, 'journal_mode': 'wal'})

    def test_names_survive_a_kill_and_a_missing_bot_is_never_created(self):
        with tempfile.TemporaryDirectory(dir=ROOT / '.local') as directory:
            store = Path(directory) / 'state.sqlite'
            client = self.start(store)
            created = client.request('create', bot='bob', workspace=directory)['result']
            self.assertEqual(client.request('create', bot='bob', workspace=directory)['error'], {'code': 'bot_exists'})
            client.close(kill=True)
            client = self.start(store)
            # Bob exists with no answered turn; a name never created stays absent.
            self.assertEqual(client.request('resume', bot='bob')['result'], {'status': 'not_completed'})
            self.assertEqual(client.request('transcript', bot='bob')['result'], {'entries': []})
            for _ in range(2):
                self.assertEqual(client.request('resume', bot='alice')['error'], {'code': 'bot_not_found'})
            created_again = client.request('create', bot='alice', workspace=directory)['result']
            self.assertNotEqual(created_again['conversation'], created['conversation'])
            self.assertEqual(client.request('entry', bot='bob', entry=10**9)['error'], {'code': 'entry_not_found'})

    def test_a_request_id_sent_twice_at_once_is_one_turn(self):
        with tempfile.TemporaryDirectory(dir=ROOT / '.local') as directory:
            client = self.start(Path(directory) / 'state.sqlite')
            client.request('create', bot='bob', workspace=directory)
            for id in (101, 102):
                client.process.stdin.write(json.dumps({'id': id, 'op': 'submit', 'bot': 'bob',
                                                       'request_id': 'r1', 'prompt': 'hi'}) + '\n')
            client.process.stdin.flush()
            replies = [client.receive(lambda m, id=id: m.get('id') == id)['result'] for id in (101, 102)]
            self.assertEqual(replies[0]['turn'], replies[1]['turn'])
            self.assertEqual(sorted(r['duplicate'] for r in replies), [False, True])
            client.finished(replies[0]['turn'])
            with self.assertRaises(queue.Empty):
                client.finished(replies[0]['turn'], timeout=1)

    def test_lifecycle_with_tools_restart_replay_duplicates_and_forks(self):
        config = dict(version=1, concurrency=2, turns=2, history_bytes=4096, chunks=4, chunk_bytes=256, chunk_delay_ms=25)
        for mode in ('text', 'echo', 'shell'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(dir=ROOT / '.local') as path:
                result = run_once(None, Path(path), config, mode, 'echo,shell', engine='pi-durable')
                self.assertEqual(result['status'], 'ok', result)
                self.assertEqual(result['completed_turns'], 4)
                tools = 0 if mode == 'text' else 4
                self.assertEqual(result['provider']['completed_requests'], 4 + tools)
                self.assertEqual(result['provider']['tool_results'], tools)
                self.assertEqual(result['provider']['invalid_requests'], 0)
                self.assertEqual(result['provider']['peak_active_requests'], 2)
                self.assertIn('forked_idle', result['phase_peak_rss_bytes'])
                # Node alone, or Node with each turn's bash and sleep.
                self.assertGreater(result['target_peak_processes'], 1 if mode == 'shell' else 0)


if __name__ == '__main__':
    unittest.main()
