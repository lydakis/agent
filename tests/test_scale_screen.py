import asyncio
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from bench import scale_screen
from bench.felt import felt


class ScreenTests(unittest.TestCase):
    def test_existing_output_is_preserved_before_any_program_starts(self):
        with tempfile.TemporaryDirectory() as folder:
            kept = Path(folder) / 'prior-result'
            kept.write_text('kept')
            for screen, args in [(scale_screen, []), (felt, ['--agent', 'agent', '--sonny', 'sonny', '--dylib', 'count'])]:
                with self.subTest(screen=screen.__name__), patch.object(sys, 'argv', ['screen', '--out', folder, *args]):
                    with patch('subprocess.Popen', side_effect=AssertionError('program started')):
                        with self.assertRaises(FileExistsError):
                            screen.main()
                    self.assertEqual(kept.read_text(), 'kept')


class ConnectionTests(unittest.IsolatedAsyncioTestCase):
    async def test_disconnect_ends_outstanding_and_future_requests(self):
        reader = asyncio.StreamReader()
        writer = Mock()
        writer.is_closing.side_effect = lambda: writer.close.called
        conn = scale_screen.Conn(reader, writer)
        first = conn.send('stats')
        second = conn.send('stats')
        reader.feed_data(b'{"id":1,"result":{}}\n')
        reader.feed_eof()
        await conn.read()
        self.assertEqual((await first)['result'], {})
        with self.assertRaises(ConnectionError):
            await second
        with self.assertRaises(ConnectionError):
            await conn.request('stats')
