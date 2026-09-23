"""Deterministic text-only subset of the Anthropic Messages streaming protocol.

Conversation checks remain in responses.Transcript. Native context (the
top-level system prompt, mid-conversation system messages, and extra user text
blocks such as date reminders) may accompany the validated synthetic turns.
"""

ROUTES = ('/v1/messages', '/v1/messages?beta=true')
PRECONNECT = 'HEAD /api/hello HTTP/1.1'


def normalize(request):
    if (request.get('model') != 'bench-model' or request.get('stream') is not True
            or request.get('tools') not in (None, [])):
        raise ValueError('unsupported Messages model request')
    messages = request.get('messages')
    if not isinstance(messages, list):
        raise ValueError('explicit conversation required')
    items = []
    for message in messages:
        role, content = message.get('role'), message.get('content')
        if role not in ('system', 'user', 'assistant'):
            raise ValueError('only text messages supported')
        if isinstance(content, str):
            content = [{'type': 'text', 'text': content}]
        if not isinstance(content, list) or any(
                block.get('type') != 'text' or not isinstance(block.get('text'), str)
                for block in content):
            raise ValueError('only text content supported')
        if role == 'assistant':
            items.append({'role': role, 'content': content})
        else:
            # A native user message can carry a context block before the
            # workload prompt; each block is checked on its own.
            items.extend({'role': role, 'content': [block]} for block in content)
    return {'model': 'bench-model', 'stream': True, 'input': items}


class Frames:
    def __init__(self, config, agent, turn):
        self.config = config
        self.message_id = f'msg_bench_{agent}_{turn}'

    def events(self):
        usage = {'input_tokens': 1, 'output_tokens': 1}
        yield 0, {'type': 'message_start', 'message': {
            'id': self.message_id, 'type': 'message', 'role': 'assistant',
            'model': 'bench-model', 'content': [], 'stop_reason': None,
            'stop_sequence': None, 'usage': usage}}
        yield 0, {'type': 'content_block_start', 'index': 0,
                  'content_block': {'type': 'text', 'text': ''}}
        for _ in range(self.config['chunks']):
            yield self.config['chunk_delay_ms'] / 1000, {
                'type': 'content_block_delta', 'index': 0,
                'delta': {'type': 'text_delta', 'text': 'x' * self.config['chunk_bytes']}}
        yield 0, {'type': 'content_block_stop', 'index': 0}
        yield 0, {'type': 'message_delta',
                  'delta': {'stop_reason': 'end_turn', 'stop_sequence': None},
                  'usage': {'output_tokens': 1}}
        yield 0, {'type': 'message_stop'}
