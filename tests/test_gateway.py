import unittest

from bench.gateway import Frames, normalize
from bench.responses import Transcript, prompt


class GatewayTests(unittest.TestCase):
    def setUp(self):
        self.config = dict(concurrency=2, turns=3, history_bytes=32,
                           chunks=4, chunk_bytes=8, chunk_delay_ms=25)
        self.headers = {'ai-language-model-id': 'bench-model',
                        'ai-language-model-streaming': 'true'}

    def request(self, turn):
        messages = [{'role': 'system', 'content': 'Synthetic instructions.'}]
        for index in range(turn + 1):
            messages.append({'role': 'user', 'content': [
                {'type': 'text', 'text': prompt(self.config, 0, index)}]})
            if index < turn:
                messages.append({'role': 'assistant', 'content': [
                    {'type': 'text', 'text': 'x' * 32}]})
        return {'prompt': messages, 'tools': []}

    def test_gateway_history_uses_the_same_strict_transcript_ledger(self):
        ledger = Transcript(self.config)
        ledger.accept(normalize(self.request(0), self.headers))
        ledger.complete(0, 0)
        for mutate in (
            lambda r: r['prompt'].pop(1),
            lambda r: r['prompt'][1]['content'][0].update(text=prompt(self.config, 1, 0)),
            lambda r: r['prompt'][2]['content'][0].update(text='wrong response'),
        ):
            bad = self.request(1)
            mutate(bad)
            with self.assertRaises(ValueError):
                ledger.accept(normalize(bad, self.headers))
        ledger.accept(normalize(self.request(1), self.headers))
        ledger.complete(0, 1)
        with self.assertRaises(ValueError):
            ledger.accept(normalize(self.request(1), self.headers))

    def test_tools_nontext_and_wrong_model_are_rejected(self):
        for mutate in (
            lambda r: r.update(tools=[{'name': 'shell'}]),
            lambda r: r['prompt'][1]['content'].append({'type': 'image', 'image': 'x'}),
            lambda r: r['prompt'].append({'role': 'tool', 'content': []}),
        ):
            bad = self.request(0)
            mutate(bad)
            with self.assertRaises(ValueError):
                normalize(bad, self.headers)
        for header, value in (('ai-language-model-id', 'other'),
                              ('ai-language-model-streaming', 'false')):
            with self.assertRaises(ValueError):
                normalize(self.request(0), {**self.headers, header: value})

    def test_gateway_stream_preserves_workload_bytes_and_delays(self):
        frames = list(Frames(self.config).events())
        deltas = [(delay, e['delta']) for delay, e in frames if e['type'] == 'text-delta']
        self.assertEqual(deltas, [(.025, 'x' * 8)] * 4)
        self.assertEqual(frames[-1][1]['finishReason']['unified'], 'stop')


if __name__ == '__main__':
    unittest.main()
