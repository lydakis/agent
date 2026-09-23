import http.client
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from bench import anthropic
from bench.responses import Transcript, prompt


class AnthropicMessagesTests(unittest.TestCase):
    def setUp(self):
        self.config = dict(concurrency=2, turns=3, history_bytes=32,
                           chunks=4, chunk_bytes=8, chunk_delay_ms=1)

    def request(self, agent, turn):
        # The shape Claude Code sends: a context block before the first prompt,
        # mid-conversation system messages, and string or block content.
        messages = []
        for index in range(turn + 1):
            blocks = [{'type': 'text', 'text': prompt(self.config, agent, index)}]
            if index == 0:
                blocks.insert(0, {'type': 'text', 'text': '<system-reminder>date</system-reminder>'})
            messages.append({'role': 'user', 'content': blocks})
            messages.append({'role': 'system', 'content': 'native context'})
            if index < turn:
                messages.append({'role': 'assistant', 'content': [{'type': 'text', 'text': 'x' * 32}]})
        return {'model': 'bench-model', 'stream': True, 'max_tokens': 32000,
                'system': [{'type': 'text', 'text': 'native prefix'}], 'messages': messages}

    def test_history_is_validated_through_the_shared_transcript(self):
        ledger = Transcript(self.config)
        self.assertEqual(ledger.accept(anthropic.normalize(self.request(0, 0))), (0, 0))
        ledger.complete(0, 0)
        for mutate in (
            lambda r: r['messages'].pop(0),
            lambda r: r['messages'][0]['content'][1].update(text=prompt(self.config, 1, 0)),
            lambda r: r['messages'][2]['content'][0].update(text='wrong response'),
        ):
            bad = self.request(0, 1)
            mutate(bad)
            with self.assertRaises(ValueError):
                ledger.accept(anthropic.normalize(bad))
        self.assertEqual(ledger.accept(anthropic.normalize(self.request(0, 1))), (0, 1))
        ledger.complete(0, 1)
        with self.assertRaises(ValueError):  # A retry of an accepted turn.
            ledger.accept(anthropic.normalize(self.request(0, 1)))

    def test_tools_and_non_text_content_are_rejected(self):
        for mutate in (
            lambda r: r.update(tools=[{'name': 'Bash'}]),
            lambda r: r.update(stream=False),
            lambda r: r.update(model='other'),
            lambda r: r['messages'][2]['content'].append(
                {'type': 'tool_use', 'id': 't', 'name': 'Bash', 'input': {}}),
            lambda r: r['messages'][2]['content'].insert(0, {'type': 'thinking', 'thinking': ''}),
        ):
            bad = self.request(0, 1)
            mutate(bad)
            with self.assertRaises(ValueError):
                anthropic.normalize(bad)

    def test_stream_carries_scheduled_text_and_a_clean_stop(self):
        frames = list(anthropic.Frames(self.config, 1, 2).events())
        kinds = [event['type'] for _, event in frames]
        self.assertEqual(kinds, ['message_start', 'content_block_start'] + ['content_block_delta'] * 4
                         + ['content_block_stop', 'message_delta', 'message_stop'])
        deltas = [(delay, event['delta']['text']) for delay, event in frames
                  if event['type'] == 'content_block_delta']
        self.assertEqual(deltas, [(.001, 'x' * 8)] * 4)
        self.assertEqual(frames[-2][1]['delta']['stop_reason'], 'end_turn')


class AnthropicFixtureTests(unittest.TestCase):
    def test_preconnect_is_counted_separately_from_inference(self):
        config = dict(version=1, concurrency=1, turns=1, chunks=2, chunk_bytes=4,
                      chunk_delay_ms=1, history_bytes=16)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / 'workload.json').write_text(json.dumps(config))
            provider = subprocess.Popen(
                [sys.executable, '-m', 'bench.provider', '--workload', str(path / 'workload.json'),
                 '--stats', str(path / 'stats.json'), '--protocol', 'anthropic_messages'],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            try:
                port = json.loads(provider.stdout.readline())['port']
                connection = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
                connection.request('HEAD', '/api/hello')
                response = connection.getresponse()
                response.read()
                self.assertEqual(response.status, 200)
                body = json.dumps({'model': 'bench-model', 'stream': True, 'messages': [
                    {'role': 'user', 'content': prompt(config, 0, 0)}]})
                connection.request('POST', '/v1/messages?beta=true', body,
                                   {'Content-Type': 'application/json'})
                response = connection.getresponse()
                events = [json.loads(line[6:]) for line in response.read().decode().splitlines()
                          if line.startswith('data: ')]
                self.assertEqual(''.join(e['delta']['text'] for e in events
                                         if e['type'] == 'content_block_delta'), 'x' * 8)
                connection.close()
            finally:
                provider.terminate()
                provider.wait(timeout=5)
                provider.stdout.close()
            stats = json.loads((path / 'stats.json').read_text())
        self.assertEqual(stats['preconnect_requests'], 1)
        self.assertEqual(stats['requests'], 1)
        self.assertEqual(stats['completed_requests'], 1)
        self.assertEqual(stats['invalid_requests'], 0)
        self.assertEqual(stats['output_text_bytes'], 8)


if __name__ == '__main__':
    unittest.main()
