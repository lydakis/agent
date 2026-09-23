"""Soak observers: disk-backed exact replay checks and bounded slow reads."""
import itertools
import json
import socket
import tempfile
import threading

from .socket_client import Connection


class JournalFollower(Connection):
    """Keep durable events on disk instead of accumulating payloads in RAM."""

    def initialize(self, journal):
        self.journal = journal
        self.changed = threading.Condition()
        self.last_cursor = 0

    def __init__(self, path, bot):
        self.initialize(tempfile.TemporaryFile(mode='w+'))
        try:
            super().__init__(path)
            response = self.request('follow', bot=bot, after=0)
            if 'error' in response:
                raise RuntimeError(f"follow {bot}: {response['error']}")
        except BaseException:
            self.close()
            raise

    def record(self, event):
        if 'cursor' in event and event.get('event') != 'follow_live':
            with self.changed:
                self.journal.write(json.dumps(event) + '\n')
                self.last_cursor = event['cursor']
                self.changed.notify_all()
        elif 'id' in event or event.get('event') == 'ready':
            super().record(event)
        # Discard live deltas; they are not part of durable replay.

    def matches(self, control, bot, timeout=5):
        """After submissions drain, compare every retained event through EOF."""
        with tempfile.TemporaryFile(mode='w+') as replay:
            after, pruned = 0, 0
            while True:
                page = control.request('events', bot=bot, after=after, limit=256)['result']
                pruned = max(pruned, page.get('pruned_before', 0))
                for event in page['events']:
                    replay.write(json.dumps(event) + '\n')
                if not page['events']:
                    break
                cursor = page['next_cursor']
                if cursor <= after:
                    raise RuntimeError('event replay made no progress')
                after = cursor
            with self.changed:
                if not self.changed.wait_for(lambda: self.last_cursor >= after, timeout):
                    return False
                self.journal.flush()
                self.journal.seek(0)
                replay.seek(0)

                def retained(stream):
                    for line in stream:
                        event = json.loads(line)
                        if event['cursor'] > pruned:
                            yield event

                try:
                    return all(a == b for a, b in itertools.zip_longest(
                        retained(self.journal), retained(replay)))
                finally:
                    self.journal.seek(0, 2)

    def close(self):
        if hasattr(self, 'worker'):
            super().close()
        elif hasattr(self, 'socket'):
            self.socket.close()
        self.journal.close()


class SlowFollower:
    """Read at most 256 bytes per pause, without an unbounded reader queue.

    EOF/reset is an observed disconnect, not proof of its server-side cause.
    Buffered bytes can delay observing EOF. Local close never counts.
    """

    def __init__(self, path, bot, pause):
        self.pause, self.bytes_read, self.disconnected = pause, 0, False
        self.stop = threading.Event()
        self.socket = socket.socket(socket.AF_UNIX)
        try:
            self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
            self.socket.connect(str(path))
            self.socket.sendall((json.dumps(dict(id=1, op='follow', bot=bot, after=0)) + '\n').encode())
        except BaseException:
            self.socket.close()
            raise
        self.worker = threading.Thread(target=self.read, daemon=True)
        self.worker.start()

    def read(self):
        try:
            while not self.stop.wait(self.pause):
                data = self.socket.recv(256)
                if not data:
                    break
                self.bytes_read += len(data)
        except OSError:
            pass
        finally:
            self.disconnected = not self.stop.is_set()

    def close(self):
        self.stop.set()
        try:
            self.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.worker.join(timeout=2)
        self.socket.close()
