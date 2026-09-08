"""Synthetic transport client for validating the tools, not an agent engine."""

import asyncio
import json
import os

from .provider import headers


def emit(event, **fields):
    print(json.dumps({"event": event, **fields}, separators=(",", ":")), flush=True)


async def main():
    config = json.loads(os.environ["AGENT_BENCH_WORKLOAD"])
    port = int(os.environ["AGENT_BENCH_PORT"])
    body = json.dumps({"history": "x" * config["history_bytes"]},
                      separators=(",", ":")).encode()
    request = (f"POST /stream HTTP/1.1\r\nHost: 127.0.0.1\r\n"
               f"Content-Length: {len(body)}\r\n\r\n").encode() + body

    async def agent(index):
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        try:
            for turn in range(config["turns"]):
                identity = {"agent": str(index), "turn": str(turn)}
                emit("turn_start", **identity)
                writer.write(request)
                await writer.drain()
                status, fields = await headers(reader)
                size = config["chunks"] * config["chunk_bytes"]
                if status != "HTTP/1.1 200 OK" or int(fields["content-length"]) != size:
                    raise ValueError("fixture response mismatch")
                for seq in range(config["chunks"]):
                    chunk = await reader.readexactly(config["chunk_bytes"])
                    if chunk != b"x" * config["chunk_bytes"]:
                        raise ValueError("fixture payload mismatch")
                    emit("chunk", **identity, seq=seq, bytes=len(chunk))
                emit("turn_end", **identity)
        finally:
            writer.close()
            await writer.wait_closed()

    emit("ready")
    await asyncio.gather(*(agent(index) for index in range(config["concurrency"])))


if __name__ == "__main__":
    asyncio.run(main())
