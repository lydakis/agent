"""A streaming Responses endpoint for `bench.scale_screen` (no spend).

Unlike `bench.synthetic_model`, it serves thousands of concurrent streams
from one asyncio loop, paces its deltas to the clock, and can play a
provider with a tokens-per-minute limit, with or without rate-limit headers.

Each call: response.created, then after --ttft-ms an output_item.added,
then --tokens output tokens at --tps tokens a second, --per-delta tokens per
delta event, then output_item.done and response.completed with usage
(input = request bytes / 4, output = --tokens).

With --tpm L it behaves like a provider with a tokens-per-minute limit: a
bucket of L refilling continuously, each call charged input + --charge-output
at arrival; a call it cannot afford gets 429 with retry-after-ms. Every
answer carries x-ratelimit-{limit,remaining}-tokens (and -requests with --rpm).

Prints {"port": N} then serves until SIGTERM, writing --stats JSON at exit.
"""
import argparse
import asyncio
import json
import random
import signal
import time

try:
    import uvloop
except ImportError:
    uvloop = None


def frame(event):
    data = b'data: ' + json.dumps(event, separators=(',', ':')).encode() + b'\n\n'
    return f'{len(data):x}\r\n'.encode() + data + b'\r\n'


class Bucket:
    def __init__(self, per_minute):
        self.limit = per_minute
        self.level = per_minute
        self.at = time.monotonic()

    def refill(self):
        now = time.monotonic()
        self.level = min(self.limit, self.level + self.limit * (now - self.at) / 60)
        self.at = now


async def main(args):
    stats = dict(requests=0, completed=0, refused_429=0, aborted=0, peak_active=0,
                 request_bytes=0, output_tokens=0, input_tokens=0, deltas=0)
    active = 0
    tokens = Bucket(args.tpm) if args.tpm else None
    requests = Bucket(args.rpm) if args.rpm else None
    word = 'tok '
    gap = args.per_delta / args.tps

    async def handle(reader, writer):
        nonlocal active
        try:
            while True:
                head = await reader.readuntil(b'\r\n\r\n')
                length = 0
                for line in head.split(b'\r\n'):
                    if line[:15].lower() == b'content-length:':
                        length = int(line[15:])
                body = await reader.readexactly(length)
                stats['requests'] += 1
                stats['request_bytes'] += length
                input_tokens = length // 4
                limit_headers = b''
                if tokens or requests:
                    cost = input_tokens + args.charge_output
                    short = 0.0
                    for bucket, need in ((tokens, cost), (requests, 1)):
                        if bucket:
                            bucket.refill()
                            if bucket.level < need:
                                short = max(short, (need - bucket.level) / bucket.limit * 60)
                    if short == 0:
                        for bucket, need in ((tokens, cost), (requests, 1)):
                            if bucket:
                                bucket.level -= need
                    hdrs = []
                    if tokens:
                        hdrs += [f'x-ratelimit-limit-tokens: {tokens.limit}',
                                 f'x-ratelimit-remaining-tokens: {int(max(0, tokens.level))}']
                    if requests:
                        hdrs += [f'x-ratelimit-limit-requests: {requests.limit}',
                                 f'x-ratelimit-remaining-requests: {int(max(0, requests.level))}']
                    limit_headers = b'' if args.no_headers else ('\r\n'.join(hdrs) + '\r\n').encode()
                    if short > 0:
                        stats['refused_429'] += 1
                        msg = b'{"error":{"message":"rate limited","type":"rate_limit_exceeded"}}'
                        writer.write(b'HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n'
                                     + limit_headers
                                     + f'retry-after-ms: {int(short * 1000) + 1}\r\nContent-Length: {len(msg)}\r\n\r\n'.encode()
                                     + msg)
                        await writer.drain()
                        continue
                active += 1
                stats['peak_active'] = max(stats['peak_active'], active)
                try:
                    writer.write(b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n'
                                 + limit_headers + b'Transfer-Encoding: chunked\r\n\r\n')
                    writer.write(frame({'type': 'response.created', 'response': {'id': 'r'}}))
                    await writer.drain()
                    j = 1 + args.jitter * (2 * random.random() - 1)
                    ntok = max(1, int(args.tokens * j))
                    await asyncio.sleep(args.ttft_ms * j / 1000)
                    writer.write(frame({'type': 'response.output_item.added', 'output_index': 0,
                                        'item': {'type': 'message', 'id': 'm', 'role': 'assistant', 'content': []}}))
                    sent = 0
                    start = time.monotonic()
                    while sent < ntok:
                        n = min(args.per_delta, ntok - sent)
                        writer.write(frame({'type': 'response.output_text.delta', 'item_id': 'm',
                                            'output_index': 0, 'content_index': 0, 'delta': word * n}))
                        stats['deltas'] += 1
                        sent += n
                        await writer.drain()
                        # Paced to the clock so a slow loop does not stretch the call.
                        wait = start + sent / args.tps - time.monotonic()
                        if wait > 0:
                            await asyncio.sleep(wait)
                    text = word * ntok
                    item = {'type': 'message', 'id': 'm', 'role': 'assistant',
                            'content': [{'type': 'output_text', 'text': text}]}
                    writer.write(frame({'type': 'response.output_item.done', 'output_index': 0, 'item': item}))
                    writer.write(frame({'type': 'response.completed', 'response': {
                        'id': 'r', 'status': 'completed', 'output': [item],
                        'usage': {'input_tokens': input_tokens, 'output_tokens': ntok,
                                  'input_tokens_details': {'cached_tokens': 0}}}}))
                    writer.write(b'0\r\n\r\n')
                    await writer.drain()
                    stats['completed'] += 1
                    stats['output_tokens'] += ntok
                    stats['input_tokens'] += input_tokens
                finally:
                    active -= 1
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        except asyncio.CancelledError:
            stats['aborted'] += 1
        finally:
            writer.close()

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)
    server = await asyncio.start_server(handle, '127.0.0.1', 0, limit=1 << 20, backlog=4096)
    print(json.dumps({'port': server.sockets[0].getsockname()[1]}), flush=True)

    async def dump():
        while True:
            with open(args.stats, 'w') as f:
                json.dump(stats, f)
            await asyncio.sleep(1)
    dumper = asyncio.create_task(dump())
    await stop.wait()
    dumper.cancel()
    server.close()
    with open(args.stats, 'w') as f:
        json.dump(stats, f)


if __name__ == '__main__':
    p = argparse.ArgumentParser()
    p.add_argument('--stats', required=True)
    p.add_argument('--ttft-ms', type=float, default=300)
    p.add_argument('--tokens', type=int, default=100)
    p.add_argument('--tps', type=float, default=50)
    p.add_argument('--per-delta', type=int, default=5)
    p.add_argument('--tpm', type=int, default=0)
    p.add_argument('--rpm', type=int, default=0)
    p.add_argument('--charge-output', type=int, default=100)
    p.add_argument('--no-headers', action='store_true')
    p.add_argument('--jitter', type=float, default=0)
    a = p.parse_args()
    if uvloop:
        uvloop.install()
    asyncio.run(main(a))
