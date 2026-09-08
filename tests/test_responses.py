import copy
import unittest

from bench.responses import Frames, Transcript, prompt


class ResponsesTests(unittest.TestCase):
    def setUp(self):
        self.config = dict(concurrency=2, turns=3, history_bytes=32,
                           chunks=4, chunk_bytes=8, chunk_delay_ms=1)

    def request(self, agent, turn):
        items = []
        for index in range(turn + 1):
            items.append({'role': 'user', 'content': [
                {'type': 'input_text', 'text': prompt(self.config, agent, index)}]})
            if index < turn:
                items.append({'role': 'assistant', 'content': [
                    {'type': 'output_text', 'text': 'x' * 32}]})
        return {'model': 'bench-model', 'stream': True, 'input': items}

    def test_history_is_retained_and_scoped_to_each_agent(self):
        ledger = Transcript(self.config)
        self.assertEqual(ledger.accept(self.request(0, 0)), (0, 0))
        with self.assertRaises(ValueError):  # No overlapping turn on one agent.
            ledger.accept(self.request(0, 1))
        ledger.complete(0, 0)
        for mutate in (
            lambda r: r['input'].pop(0),
            lambda r: r['input'][0]['content'][0].update(text=prompt(self.config, 1, 0)),
            lambda r: r['input'][1]['content'][0].update(text='wrong response'),
        ):
            bad = self.request(0, 1)
            mutate(bad)
            with self.assertRaises(ValueError):
                ledger.accept(bad)
        self.assertEqual(ledger.accept(self.request(0, 1)), (0, 1))
        self.assertEqual(ledger.accept(self.request(1, 0)), (1, 0))

    def test_retries_and_hidden_server_history_are_rejected(self):
        ledger = Transcript(self.config)
        request = self.request(0, 0)
        hidden = copy.deepcopy(request)
        hidden['previous_response_id'] = 'resp_hidden'
        with self.assertRaises(ValueError):
            ledger.accept(hidden)
        ledger.accept(request)
        ledger.complete(0, 0)
        with self.assertRaises(ValueError):
            ledger.accept(request)

    def test_terminal_response_matches_stream_and_has_stable_ids(self):
        frames = list(Frames(self.config, 1, 2).events())
        deltas = [event['delta'] for delay, event in frames
                  if event['type'] == 'response.output_text.delta']
        self.assertEqual(deltas, ['x' * 8] * 4)
        final = frames[-1][1]
        self.assertEqual(final['type'], 'response.completed')
        self.assertEqual(final['response']['output'][0]['content'][0]['text'], ''.join(deltas))
        self.assertEqual([e['sequence_number'] for _, e in frames], list(range(len(frames))))


if __name__ == '__main__':
    unittest.main()
