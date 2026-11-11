"""A reply as long as the model can write keeps its turn; one the model's own
output cap cuts short is dropped, the model is told, and the turn goes on."""
import json
import os
import unittest

from bench.runtime_client import node_item
from tests.test_runtime import ModelFixture


def outputs(request):
    return [i for i in request['input'] if i.get('type') == 'function_call_output']


def notes(request):
    """The words of each `limit` note the request carries, parsed."""
    return [json.loads(i['content'][0]['text']) for i in request['input']
            if i.get('role') == 'user' and i['content'][0]['text'].startswith('{"dropped"')]


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class DroppedReplyTests(ModelFixture):
    def turn(self, client, bot='Bob'):
        client.request('create', bot=bot, workspace=str(self.path))
        return client.request('submit', bot=bot, request_id='r', prompt='go')['result']['message']

    def requests(self):
        sent = []
        while not self.model.requests.empty():
            sent.append(self.model.requests.get_nowait())
        return sent

    def test_a_reply_past_the_old_bound_completes_after_earlier_rounds(self):
        # 900 KiB of words, past the 768 KiB the daemon used to allow, then
        # 3 MiB, past what one event line carries.
        self.model.chunk_size = 65536
        client = self.client()
        for bot, size in (('Bob', 900 * 1024), ('Ann', 3 * 1024 * 1024)):
            with self.subTest(size=size):
                self.model.respond = lambda request, user: (
                    'x' * size if outputs(request) else [['echo', {'text': 'first'}]])
                turn = self.turn(client, bot)
                end = client.finished(turn, timeout=30)['data']
                self.assertEqual(end['status'], 'completed', end)
                events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
                messages = [e['data']['node'] for e in events if e['event'] == 'message' and e['turn'] == turn]
                # The call, its result and the reply.
                self.assertEqual(len(messages), 2)
                self.assertEqual(node_item(client, bot, messages[0])['result']['type'], 'function_call')
                self.assertEqual([e['event'] for e in events if e['event'] == 'tool_completed'], ['tool_completed'])
                reply = node_item(client, bot, messages[1])
                if size < 1024 * 1024:
                    self.assertEqual(len(reply['result']['content'][0]['text']), size)
                else:
                    # Past a line, a read says so rather than failing.
                    self.assertEqual(reply, {'error': 'item_too_large'})

    def test_a_reply_cut_short_is_dropped_and_the_turn_goes_on(self):
        # A call, then a reply the model's cap cuts while it writes a call,
        # then an answer once told.
        self.model.respond = lambda request, user: (
            'done' if notes(request) else None if outputs(request) else [['echo', {'text': 'first'}]])
        self.model.cut_short = lambda request, user: bool(outputs(request)) and not notes(request)
        client = self.client()
        turn = self.turn(client)
        end = client.finished(turn)['data']
        self.assertEqual(end['status'], 'completed', end)
        first, cut, last = self.requests()
        self.assertFalse(outputs(first))
        self.assertEqual(len(outputs(cut)), 1)
        facts = {'limit': 'max_tokens', 'output_tokens': 32000, 'in': 'write'}
        # The earlier rounds go again; nothing of the cut reply does, and
        # the model reads what cut it, as a note from the limit.
        self.assertEqual(last['input'][:len(cut['input'])], cut['input'])
        self.assertEqual(notes(last), [{'dropped': facts}])
        note = last['input'][-1]
        self.assertEqual(note['content'][1]['text'], f'[Message Bob/{turn + 1}, sent by limit.]')
        self.assertFalse([i for i in last['input'] if i.get('name') == 'write'])
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual([e['data'] for e in events if e['event'] == 'dropped'], [facts])
        row = next(t for t in client.request('turns', bot='Bob')['result']['turns'] if t['turn'] == turn)
        self.assertEqual(row['model_rounds'], 3)

    def test_the_cap_cutting_three_replies_in_a_row_ends_the_turn_on_it(self):
        self.model.cut_short = lambda request, user: True
        client = self.client()
        turn = self.turn(client)
        end = client.finished(turn)['data']
        self.assertEqual((end['status'], end['error'], end['detail']),
                         ('failed', 'provider_incomplete', 'max_output_tokens'))
        self.assertEqual((end['limit'], end['output_tokens'], end['in']), ('max_tokens', 32000, 'write'))
        sent = self.requests()
        self.assertEqual(len(sent), 3)
        self.assertEqual([len(notes(r)) for r in sent], [0, 1, 2])

    def test_the_count_in_a_row_holds_across_a_park_and_a_restart(self):
        # Cut, then parked on a rate limit and the daemon restarted, then cut
        # twice more: the third cut in a row ends the turn.
        self.model.cut_short = lambda request, user: True
        paced = []
        self.model.pace = lambda request, user: (
            len(notes(request)) == 1 and not paced and not paced.append(1))
        client = self.client()
        turn = self.turn(client)
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn, timeout=30)
        client.close(kill=True)
        client = self.client()
        end = client.finished(turn, timeout=30)['data']
        self.assertEqual((end['status'], end['error']), ('failed', 'provider_incomplete'))
        # Three cut, one parked.
        self.assertEqual([len(notes(r)) for r in self.requests()], [0, 1, 1, 2])

    def test_a_bot_sets_how_many_cuts_in_a_row_end_its_turn(self):
        self.model.cut_short = lambda request, user: True
        client = self.client(settings={'max_drops': 1})
        turn = self.turn(client)
        end = client.finished(turn)['data']
        self.assertEqual((end['status'], end['error']), ('failed', 'provider_incomplete'))
        self.assertEqual(len(self.requests()), 1)
        refused = client.request('configure', bot='Bob', settings={'max_drops': 0})
        self.assertEqual(refused['error'], 'invalid_setting', refused)


if __name__ == '__main__':
    unittest.main()
