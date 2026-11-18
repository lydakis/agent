"""A socket between Sonny and its daemon that holds every answer DELAY seconds
while the file HOLD exists; the daemon's events pass at once."""
import asyncio, json, os, sys

listen, upstream, hold, delay = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4])

async def serve(reader, writer):
    up_r, up_w = await asyncio.open_unix_connection(upstream, limit=1 << 26)
    lock = asyncio.Lock()
    async def put(line, wait):
        if wait:
            await asyncio.sleep(wait)
        async with lock:
            writer.write(line)
            await writer.drain()
    async def down():
        while line := await up_r.readline():
            try:
                answer = 'id' in json.loads(line)
            except ValueError:
                answer = False
            wait = delay if answer and os.path.exists(hold) else 0
            if wait:
                asyncio.create_task(put(line, wait))
            else:
                await put(line, 0)
        writer.close()
    async def up():
        while line := await reader.readline():
            up_w.write(line)
            await up_w.drain()
        up_w.close()
    await asyncio.gather(down(), up(), return_exceptions=True)

async def main():
    server = await asyncio.start_unix_server(serve, listen, limit=1 << 26)
    print('ready', flush=True)
    async with server:
        await server.serve_forever()

asyncio.run(main())
