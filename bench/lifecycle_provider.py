"""Synthetic tool round trips with exact per-agent history validation."""
import argparse
import asyncio
import json
from pathlib import Path
import signal

from .provider import headers
from .responses import Frames, prompt


async def serve(config, stats_path, mode):
    stats = dict(requests=0, completed_requests=0, invalid_requests=0, active=0,
                 peak_active_requests=0, tool_results=0, request_body_bytes=0, response_body_bytes=0)
    histories, turns, pending = {}, {}, {}
    handlers = set()
    async def handle(reader, writer):
        task = asyncio.current_task()
        handlers.add(task)
        active = False
        try:
            while True:
                route, fields = await headers(reader)
                assert route == 'POST /v1/responses HTTP/1.1'
                size = int(fields['content-length'])
                assert 0 <= size <= 16 * 1024**2
                request = json.loads(await reader.readexactly(size))
                assert request['model'] == 'synthetic-model' and request['stream'] and not request.get('previous_response_id')
                items = request['input']
                user = [x for x in items if x.get('role') == 'user'][-1]['content'][0]['text']
                import re
                match = re.match(r'BENCH agent=(\d+) turn=(\d+)\n', user)
                assert match
                agent, turn = map(int, match.groups())
                assert agent < config['concurrency'] and turn < config['turns']
                tool_result = items[-1].get('type') == 'function_call_output'
                expected = histories.get(agent, [])
                if tool_result:
                    assert mode != 'text' and pending[agent] == turn
                    assert items[:-1] == expected and items[-1]['call_id'] == f'tool-{agent}-{turn}'
                    result = items[-1]['output']
                    if mode == 'echo':
                        assert result == 'tool-ok'
                    else:
                        result = json.loads(result)
                        assert result == dict(stdout='tool-ok', stderr='', exit_code=0, success=True)
                    stats['tool_results'] += 1
                    del pending[agent]
                else:
                    assert agent not in pending and turn == turns.get(agent, -1) + 1
                    assert items == expected + [{'role':'user','content':[{'type':'input_text','text':prompt(config,agent,turn)}]}]
                stats['requests'] += 1
                stats['request_body_bytes'] += size
                stats['active'] += 1
                active = True
                stats['peak_active_requests'] = max(stats['peak_active_requests'], stats['active'])
                if mode != 'text' and not tool_result:
                    arguments = {'text':'tool-ok'} if mode == 'echo' else {
                        'command':"printf tool-ok > artifact; sleep 0.25; printf tool-ok", 'timeout_ms':2000}
                    output = [{'type':'function_call','name':mode,'call_id':f'tool-{agent}-{turn}',
                               'arguments':json.dumps(arguments)}]
                    frames = [(.05, {'type':'response.completed','response':{'status':'completed','output':output}})]
                    pending[agent] = turn
                else:
                    frames = list(Frames(config,agent,turn).events())
                    output = frames[-1][1]['response']['output']
                    turns[agent] = turn
                writer.write(b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n')
                for delay, event in frames:
                    if delay:
                        await asyncio.sleep(delay)
                    frame = ('data: '+json.dumps(event,separators=(',',':'))+'\n\n').encode()
                    writer.write(f'{len(frame):x}\r\n'.encode()+frame+b'\r\n')
                    await writer.drain()
                    stats['response_body_bytes'] += len(frame)
                writer.write(b'0\r\n\r\n')
                await writer.drain()
                histories[agent] = items + output
                stats['completed_requests'] += 1
                stats['active'] -= 1
                active = False
        except (AssertionError, ValueError, KeyError, TypeError, IndexError, UnicodeError):
            stats['invalid_requests'] += 1
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        finally:
            if active:
                stats['active'] -= 1
            writer.close()
            handlers.discard(task)
    stopped = asyncio.Event()
    asyncio.get_running_loop().add_signal_handler(signal.SIGTERM, stopped.set)
    server = await asyncio.start_server(handle, '127.0.0.1', 0)
    print(json.dumps({'port':server.sockets[0].getsockname()[1]}), flush=True)
    try:
        await stopped.wait()
    finally:
        server.close()
        await server.wait_closed()
        tasks = list(handlers)
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        Path(stats_path).write_text(json.dumps(stats))

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--workload', required=True, type=Path)
    parser.add_argument('--stats', required=True, type=Path)
    parser.add_argument('--mode', required=True, choices=('text','echo','shell'))
    args = parser.parse_args()
    asyncio.run(serve(json.loads(args.workload.read_text()), args.stats, args.mode))
