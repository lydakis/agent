"""A slow store, as a client of the daemon's socket sees one.

A proxy on a socket of its own, in front of the daemon's: every reply
waits `delay_s` before it reaches the client, as one would behind a store
whose reads and writes take that long. A lease renewal is not held, since
the daemon answers it as its session reads it (#436) without the store,
and neither is a pushed event. Set `delay_s` to 0 to make the store fast
again; `renewals` counts the renewals that went through.
"""
import asyncio
import json
import threading
from pathlib import Path

# A page of history or approvals runs to megabytes on one line.
LINE = 64 * 1024 * 1024


class SlowSocket:
    def __init__(self, daemon, path, delay_s):
        self.daemon, self.path = str(daemon), Path(path)
        self.delay_s = delay_s
        self.renewals = 0
        self.loop = asyncio.new_event_loop()
        started = threading.Event()
        threading.Thread(target=self.run, args=(started,), daemon=True).start()
        if not started.wait(10):
            raise RuntimeError('the slow socket did not start')

    def run(self, started):
        asyncio.set_event_loop(self.loop)

        async def serve():
            self.server = await asyncio.start_unix_server(self.session, str(self.path), limit=LINE)
            started.set()
        self.loop.run_until_complete(serve())
        self.loop.run_forever()

    async def session(self, reader, writer):
        try:
            up_reader, up_writer = await asyncio.open_unix_connection(self.daemon, limit=LINE)
        except OSError:
            writer.close()
            return
        ops = {}

        def deliver(line):
            # The client may have gone while its reply was held.
            if not writer.is_closing():
                try:
                    writer.write(line)
                except (ConnectionError, OSError):
                    pass

        async def upward():
            try:
                await forward_requests()
            except (ConnectionError, OSError):
                pass
            up_writer.close()

        async def forward_requests():
            while line := await reader.readline():
                try:
                    request = json.loads(line)
                except ValueError:
                    request = {}
                if isinstance(request, dict) and 'id' in request:
                    ops[json.dumps(request['id'])] = request.get('op')
                    if request.get('op') == 'renew_approvals':
                        self.renewals += 1
                up_writer.write(line)
                await up_writer.drain()

        async def downward():
            try:
                await forward_replies()
            except (ConnectionError, OSError):
                pass
            writer.close()

        async def forward_replies():
            while line := await up_reader.readline():
                try:
                    message = json.loads(line)
                except ValueError:
                    message = {}
                op = ops.pop(json.dumps(message['id']), None) if 'id' in message else None
                if op is not None and op != 'renew_approvals' and self.delay_s:
                    self.loop.call_later(self.delay_s, deliver, line)
                else:
                    writer.write(line)
                    await writer.drain()

        await asyncio.gather(upward(), downward())

    def close(self):
        def stop():
            self.server.close()
            self.loop.stop()
        self.loop.call_soon_threadsafe(stop)
