"""Loopback binary fixture and a deliberately narrow Responses SSE fixture."""

import argparse
import asyncio
import json
from pathlib import Path
import signal

from .config import workload
from .responses import Frames, Transcript


async def headers(reader):
    raw = await reader.readuntil(b"\r\n\r\n")
    lines = raw.decode("ascii").split("\r\n")
    fields = {}
    for line in lines[1:]:
        if line:
            key, value = line.split(":", 1)
            fields[key.lower()] = value.strip()
    return lines[0], fields


async def serve(config, stats_path, protocol="binary"):
    stats = {"requests": 0, "completed_requests": 0, "aborted_requests": 0,
             "connections_used": 0, "peak_active_requests": 0,
             "request_body_bytes": 0, "response_body_bytes": 0,
             "output_text_bytes": 0, "invalid_requests": 0}
    transcript = Transcript(config)
    active = 0
    handlers = set()
    payload = b"x" * config["chunk_bytes"]

    async def handle(reader, writer):
        nonlocal active
        task = asyncio.current_task()
        handlers.add(task)
        counted_connection = False
        in_request = False
        try:
            while True:
                line, fields = await headers(reader)
                route = "/stream" if protocol == "binary" else "/v1/responses"
                if line != f"POST {route} HTTP/1.1" or "transfer-encoding" in fields:
                    raise ValueError("unsupported fixture request")
                size = int(fields["content-length"])
                limit = config["history_bytes"] + 128 if protocol == "binary" else 16 * 1024 * 1024
                if not 0 <= size <= limit:
                    raise ValueError("fixture request body too large")
                body = await reader.readexactly(size)
                request = json.loads(body)
                if protocol == "responses":
                    agent, turn = transcript.accept(request)
                elif request != {"history": "x" * config["history_bytes"]}:
                    raise ValueError("fixture history mismatch")
                stats["request_body_bytes"] += size
                stats["requests"] += 1
                if not counted_connection:
                    stats["connections_used"] += 1
                    counted_connection = True
                active += 1
                in_request = True
                stats["peak_active_requests"] = max(stats["peak_active_requests"], active)
                size = config["chunks"] * config["chunk_bytes"]
                if protocol == "binary":
                    writer.write((f"HTTP/1.1 200 OK\r\nContent-Length: {size}\r\n"
                                  "Content-Type: application/octet-stream\r\n\r\n").encode())
                    frames = ((config["chunk_delay_ms"] / 1000, payload)
                              for _ in range(config["chunks"]))
                else:
                    writer.write(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n"
                                 b"Content-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n")
                    frames = ((delay, ("event: " + event["type"] + "\ndata: " +
                               json.dumps(event, separators=(",", ":")) + "\n\n").encode())
                              for delay, event in Frames(config, agent, turn).events())
                for delay, frame in frames:
                    if delay:
                        await asyncio.sleep(delay)
                    if protocol == "responses":
                        writer.write(f"{len(frame):x}\r\n".encode() + frame + b"\r\n")
                    else:
                        writer.write(frame)
                    await writer.drain()
                    stats["response_body_bytes"] += len(frame)
                if protocol == "responses":
                    writer.write(b"0\r\n\r\n")
                    await writer.drain()
                    transcript.complete(agent, turn)
                stats["output_text_bytes"] += size
                stats["completed_requests"] += 1
                active -= 1
                in_request = False
        except (ValueError, KeyError, UnicodeError, TypeError, AttributeError,
                asyncio.LimitOverrunError):
            stats["invalid_requests"] += 1
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        finally:
            if in_request:
                active -= 1
                stats["aborted_requests"] += 1
            writer.close()
            handlers.discard(task)

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)
    server = await asyncio.start_server(handle, "127.0.0.1", 0, limit=8192)
    port = server.sockets[0].getsockname()[1]
    print(json.dumps({"port": port}), flush=True)
    try:
        await stop.wait()
    finally:
        server.close()
        await server.wait_closed()
        pending = list(handlers)
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
        Path(stats_path).write_text(json.dumps(stats, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--workload", required=True)
    parser.add_argument("--stats", required=True)
    parser.add_argument("--protocol", choices=("binary", "responses"), default="binary")
    args = parser.parse_args()
    asyncio.run(serve(workload(args.workload), args.stats, args.protocol))
