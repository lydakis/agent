"""Pictures sent with a prompt, validated on submit and put after its words, and image files read."""
import base64
import os
import sqlite3
import struct
import unittest
import zlib

from tests.test_runtime import ModelFixture
from bench.runtime_client import node_item


def png(width=2, height=2):
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    raw = b''.join(b'\x00' + b'\xff\x00\x00' * width for _ in range(height))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))


def noise(side):
    """A PNG of random pixels: large for its size, as a photograph is."""
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    raw = b''.join(b'\x00' + os.urandom(3 * side) for _ in range(side))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', side, side, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))


def image(data=None, media_type='image/png'):
    return {'media_type': media_type, 'data': base64.b64encode(data or png()).decode()}


def pictures(item):
    return [part['image_url'] for part in item['content'] if part['type'] == 'input_image']


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ImageTests(ModelFixture):
    def test_pictures_follow_the_words_of_a_turn_and_of_a_steer(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        shot = image()
        first = client.request('submit', bot='Bob', request_id='a', prompt='slow',
                               images=[shot])['result']['turn']
        request = self.model.requests.get(timeout=3)
        user = [i for i in request['input'] if i.get('role') == 'user'][-1]
        self.assertEqual(user['content'][0], {'type': 'input_text', 'text': 'slow'})
        self.assertEqual(pictures(user), [f"data:image/png;base64,{shot['data']}"])
        # A steer's pictures go with it into the running turn.
        steer = client.request('submit', bot='Bob', request_id='b', prompt='steer:and this',
                               images=[shot, shot], delivery='steer')['result']['turn']
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        self.assertEqual(client.finished(steer)['data']['status'], 'steered')
        while True:
            request = self.model.requests.get(timeout=3)
            steered = [i for i in request['input'] if i.get('role') == 'user'
                       and i['content'][0]['text'] == 'steer:and this']
            if steered:
                break
        self.assertEqual(len(pictures(steered[0])), 2)
        # History reads leave the bytes out; the model had them whole.
        events = client.request('events', bot='Bob', after=0, limit=64)['result']['events']
        node = next(e for e in events if e['event'] == 'accepted' and e['turn'] == first)['data']['node']
        read = node_item(client, 'Bob', node)['result']
        self.assertEqual(read['content'][1], {'type': 'input_image', 'image_url': 'data:image/png;base64,',
                                              'omitted_bytes': len(shot['data'])})
        # The node keeps the pictures; the turns keep only their digest,
        # which a retry with the same ones matches.
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            kept = [row[0] for row in db.execute('SELECT images FROM turns WHERE id IN (?,?)', (first, steer))]
        self.assertTrue(all(len(digest) == 64 and not digest.startswith('[') for digest in kept), kept)
        again = client.request('submit', bot='Bob', request_id='a', prompt='slow', images=[shot])
        self.assertEqual(again['result']['turn'], first)
        # A retry must carry the same pictures.
        retried = client.request('submit', bot='Bob', request_id='a', prompt='slow', images=[])
        self.assertEqual(retried['error'], 'idempotency_conflict')
        self.assertEqual(retried['field'], 'images')

    def test_a_picture_counts_at_its_token_cost_not_its_size(self):
        # Three pictures of 160 KB each stay in view under a 16 KB context
        # budget: 200 × 200 pixels is 54 tokens.
        client = self.client(settings={'context_bytes': 16384})
        client.request('create', bot='Bob', workspace=str(self.path))
        shots = [image(noise(200)) for _ in range(3)]
        for n, shot in enumerate(shots):
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt=f'look {n}',
                                  images=[shot])['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        while not self.model.requests.empty():
            request = self.model.requests.get()
        users = [i for i in request['input'] if i.get('role') == 'user']
        self.assertEqual([pictures(user) for user in users],
                         [[f"data:image/png;base64,{shot['data']}"] for shot in shots])

    def test_a_request_keeps_its_newest_pictures_and_older_ones_go_as_their_words(self):
        # Six messages of 20 pictures: the sixth would send 120, past the
        # 100 a request takes, so the oldest go until 50 at most remain.
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        shots = [image(png(4, 4))] * 20
        for n in range(6):
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt=f'look {n}',
                                  images=shots)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        while not self.model.requests.empty():
            request = self.model.requests.get()
        users = [i for i in request['input'] if i.get('role') == 'user']
        self.assertEqual([len(pictures(user)) for user in users], [0, 0, 0, 0, 20, 20])
        self.assertEqual(users[0]['content'], [{
            'type': 'input_text',
            'text': 'look 0\n[20 picture(s) sent with this message, elided from this request]'}])

    def test_read_sends_an_image_file_as_a_picture_scaled_to_fit(self):
        (self.path / 'shot.png').write_bytes(png(3000, 2000))
        client = self.client('read')
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='a', prompt='readpic:shot.png')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        while True:
            request = self.model.requests.get(timeout=3)
            if request['input'][-1].get('type') == 'function_call_output':
                break
        text, picture = request['input'][-1]['output']
        self.assertEqual(text['type'], 'input_text')
        self.assertTrue(text['text'].startswith('shot.png: a PNG image, 3000×2000,'), text)
        self.assertTrue(text['text'].endswith('scaled to 1568×1045'), text)
        self.assertEqual(picture['type'], 'input_image')
        data = base64.b64decode(picture['image_url'].removeprefix('data:image/png;base64,'))
        self.assertEqual(struct.unpack('>II', data[16:24]), (1568, 1045))
        # History reads leave the picture's bytes out.
        events = client.request('events', bot='Bob', after=0, limit=64)['result']['events']
        node = next(e for e in events if e['event'] == 'tool_completed')['data']['node']
        read = node_item(client, 'Bob', node)['result']
        self.assertEqual(read['output'][1], {'type': 'input_image', 'image_url': 'data:image/png;base64,',
                                             'omitted_bytes': len(picture['image_url']) - len('data:image/png;base64,')})

    def test_pictures_a_provider_would_refuse_are_refused_on_submit(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        refusals = {
            'invalid_image': [image(media_type='image/tiff'),
                              {'media_type': 'image/png', 'data': '!!'},
                              image(data=b'\xff\xd8\xff\xe0 a jpeg')],
            'image_limit': [image(data=png() + b'\0' * (5 << 20))],
        }
        for error, images in refusals.items():
            for n, picture in enumerate(images):
                refused = client.request('submit', bot='Bob', request_id=f'{error}{n}', prompt='x',
                                         images=[picture])
                self.assertEqual(refused['error'], error, refused)
        refused = client.request('submit', bot='Bob', request_id='many', prompt='x',
                                 images=[image()] * 21)
        self.assertEqual(refused['error'], 'image_limit')


if __name__ == '__main__':
    unittest.main()
