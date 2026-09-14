"""Synthetic Gateway wire format; conversation checks remain in Transcript."""


def normalize(request, headers):
    if (headers.get('ai-language-model-id') != 'bench-model'
            or headers.get('ai-language-model-streaming') != 'true'
            or request.get('tools') not in (None, [])):
        raise ValueError('unsupported Gateway model request')
    messages = request.get('prompt')
    if not isinstance(messages, list):
        raise ValueError('explicit conversation required')
    for message in messages:
        if message.get('role') not in ('system', 'developer', 'user', 'assistant'):
            raise ValueError('only text messages supported')
        content = message.get('content')
        if isinstance(content, str):
            continue
        if not isinstance(content, list) or any(
                part.get('type') != 'text' or not isinstance(part.get('text'), str)
                for part in content):
            raise ValueError('only text content supported')
    return {'model': 'bench-model', 'stream': True, 'input': messages}


def catalog():
    return {'object': 'list', 'data': [{'id': 'bench-model', 'type': 'language',
                                      'context_window': 1000000}]}


class Frames:
    def __init__(self, config):
        self.config = config

    def events(self):
        for _ in range(self.config['chunks']):
            yield self.config['chunk_delay_ms'] / 1000, {
                'type': 'text-delta', 'id': 'bench',
                'delta': 'x' * self.config['chunk_bytes']}
        yield 0, {'type': 'finish', 'finishReason': {'unified': 'stop', 'raw': 'stop'},
                  'usage': {'inputTokens': {'total': 1}, 'outputTokens': {'total': 1}}}
