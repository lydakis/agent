"""Pictures sent with a prompt: validated on submit, after the words in the user item."""
import base64
import os
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
        # A retry must carry the same pictures.
        retried = client.request('submit', bot='Bob', request_id='a', prompt='slow', images=[])
        self.assertEqual(retried['error'], 'idempotency_conflict')
        self.assertEqual(retried['field'], 'images')

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
