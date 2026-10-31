"""Useful work an agent does, in each of Sonny's permission rows: what runs,
what asks once, and what an "always allow" lets through after. As Codex's
rows balance it: a sandboxed agent's own work runs without asking, what
leaves the sandbox asks, and the person can say yes for good."""
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path

from bench.runtime_client import node_item
from bench.socket_client import SocketClient
from bench.targets import clean_env
from tests.test_runtime import PROTECTING, ModelFixture

ROOT = Path(__file__).resolve().parent.parent

# Sonny's rows as the daemon gets them (sonny/src/approvals.rs `builtin`
# and `OWN_UNSANDBOXED`): Auto and Ask me differ only in who answers what
# leaves the sandbox.
OWN = ['sonny zen']
AUTO = {'sandbox': 'workspace-write', 'network': True, 'unsandboxed': OWN}
READ_ONLY = {'sandbox': 'read-only', 'unsandboxed': OWN}
FULL = {}


def serve(path, answer=b'pong'):
    """A program's socket, as MCPX's, Docker's or ssh-agent's: it answers
    each connection once."""
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(path))
    listener.listen()

    def accept():
        while True:
            try:
                connection, _ = listener.accept()
            except OSError:
                return
            with connection:
                connection.sendall(answer)

    threading.Thread(target=accept, daemon=True).start()
    return listener


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
@unittest.skipUnless(shutil.which('bwrap') or sys.platform == 'darwin', 'sandboxes need bubblewrap')
class WorkflowTests(ModelFixture):
    def setUp(self):
        super().setUp()
        # Outside the temporary directory, which a sandbox may write.
        self.home = Path(tempfile.mkdtemp(dir=ROOT / '.local', prefix='home-'))
        self.addCleanup(shutil.rmtree, self.home)
        self.work = self.path / 'work'
        self.work.mkdir()
        subprocess.run(['git', 'init', '-q', '-b', 'main'], cwd=self.work, check=True)
        bin = self.path / 'bin'
        bin.mkdir()
        for name in ('agent', 'sonny'):
            (bin / name).symlink_to(self.binary.parent / name)
        # MCPX, as it is reached: a CLI that asks its own daemon over a
        # socket in the person's home.
        (self.home / '.mcpx').mkdir()
        self.mcpx = serve(self.home / '.mcpx/mcpx.sock')
        self.addCleanup(self.mcpx.close)
        (bin / 'mcpx').write_text(
            '#!/usr/bin/env python3\n'
            'import os, socket\n'
            's = socket.socket(socket.AF_UNIX)\n'
            's.connect(os.path.expanduser("~/.mcpx/mcpx.sock"))\n'
            'print(s.recv(16).decode())\n')
        (bin / 'mcpx').chmod(0o755)
        env = {**clean_env(), 'HOME': str(self.home), 'PATH': f'{bin}:{os.environ["PATH"]}'}
        self.daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'shell',
                                   extra=PROTECTING, env=env)
        self.addCleanup(self.daemon.close)
        self.turns = 0

    def agent(self, settings, gated=True):
        name = f'bot{len(self.daemon.request("bots")["result"]["bots"])}'
        gate = {'approve': ['shell'], 'approver': 'manual'} if gated else {}
        self.daemon.request('create', bot=name, workspace=str(self.work), tools=['shell'],
                            settings=settings, **gate)
        return name

    def run_(self, bot, command, escalated=False, allow=None):
        """`bot` runs `command`; whether it was asked about, and its output.
        With `allow`, an ask is answered so."""
        self.turns += 1
        arguments = {'command': command, 'timeout_ms': 20000}
        if escalated:
            arguments.update(sandbox_permissions='require_escalated', justification='Needs to.')
        turn = self.daemon.request('submit', bot=bot, request_id=f't{self.turns}',
                                   prompt='calls:' + json.dumps([['shell', arguments]]))['result']['turn']
        asked = False
        if allow is not None:
            event = self.daemon.followers[bot].receive(
                lambda m: m.get('event') == 'approval_requested' and m.get('turn') == turn, 20)
            asked = True
            [call] = event['data']['calls']
            self.daemon.request('answer', bot=bot, turn=turn, call_id=call['call_id'], request=call['request'],
                                decision='allow' if allow else 'deny', by='test')
        self.assertEqual(self.daemon.finished(turn, 30)['data']['status'], 'completed')
        events = self.daemon.request('events', bot=bot, after=0, limit=256)['result']['events']
        asked = asked or any(e['event'] == 'approval_requested' and e['turn'] == turn for e in events)
        [done] = [e for e in events if e['turn'] == turn and e['event'] == 'tool_completed']
        output = json.loads(node_item(self.daemon, bot, done['data']['node'])['result']['output'])
        return asked, output

    def runs(self, bot, command, **ask):
        asked, output = self.run_(bot, command, **ask)
        self.assertEqual(output.get('exit_code'), 0, f'{command}: {output}')
        return asked, output

    def test_the_agent_and_sonny_clis_work_in_every_row(self):
        # A lead lists its agents with the agent CLI, as Sonny's skills do;
        # nothing asks.
        for settings in (AUTO, READ_ONLY, FULL):
            bot = self.agent(settings, gated=settings is not FULL)
            asked, output = self.runs(bot, 'agent ls')
            self.assertFalse(asked, settings)
            self.assertIn(bot, output['stdout'], settings)
            # It keeps its focus with Sonny's CLI, which writes Sonny's state.
            asked, _ = self.runs(bot, 'sonny zen the tests')
            self.assertFalse(asked, settings)
        self.assertEqual(len(list((self.home / '.agent/sonny/zen').iterdir())), 3)

    def test_mcpx_docker_and_ssh_agent_sockets_are_reached_with_the_network_on(self):
        # Auto and Ask me reach the network, so every socket, as Codex's
        # full network does.
        bot = self.agent(AUTO)
        asked, output = self.runs(bot, 'mcpx')
        self.assertEqual((asked, output['stdout'].strip()), (False, 'pong'))
        bot = self.agent(FULL, gated=False)
        self.assertEqual(self.runs(bot, 'mcpx')[1]['stdout'].strip(), 'pong')
        # Read only reaches none: the agent asks to leave the sandbox, once.
        bot = self.agent(READ_ONLY)
        asked, output = self.run_(bot, 'mcpx')
        self.assertNotEqual(output.get('exit_code'), 0)
        self.assertFalse(asked)
        asked, output = self.runs(bot, 'mcpx', escalated=True, allow=True)
        self.assertTrue(asked)
        # "Always allow mcpx" adds it to the commands that leave every
        # sandbox; it never asks again.
        self.daemon.request('configure', bot=bot, sandbox='read-only', unsandboxed=[*OWN, 'mcpx'])
        asked, output = self.runs(bot, 'mcpx')
        self.assertEqual((asked, output['stdout'].strip()), (False, 'pong'))

    def test_an_agent_runs_its_tests_servers_and_commits_in_auto(self):
        bot = self.agent({**AUTO, 'writable': [str(self.work / '.git')]})
        # A test suite that listens on a socket of its own and on localhost.
        (self.work / 'suite.py').write_text(
            'import socket, http.server, threading, urllib.request\n'
            's = socket.socket(socket.AF_UNIX); s.bind("own.sock"); s.listen()\n'
            'c = socket.socket(socket.AF_UNIX); c.connect("own.sock")\n'
            'server = http.server.HTTPServer(("127.0.0.1", 0), http.server.SimpleHTTPRequestHandler)\n'
            'threading.Thread(target=server.serve_forever, daemon=True).start()\n'
            'urllib.request.urlopen(f"http://127.0.0.1:{server.server_port}/suite.py").read()\n'
            'print("ok")\n')
        asked, output = self.runs(bot, 'python3 suite.py; rm -f own.sock')
        self.assertEqual((asked, output['stdout'].strip()), (False, 'ok'))
        asked, _ = self.runs(bot, 'git add suite.py && git -c user.name=t -c user.email=t@e commit -qm suite')
        self.assertFalse(asked)

    def test_a_write_outside_its_folder_asks_once_then_always_allowed_goes_through(self):
        cache = self.home / '.cache'
        bot = self.agent(AUTO)
        # Confined, the write is refused, not asked about.
        asked, output = self.run_(bot, f'touch {cache}-x')
        self.assertNotEqual(output.get('exit_code'), 0)
        self.assertFalse(asked)
        # Asked to leave the sandbox, the person (or the reviewer) allows it.
        asked, _ = self.runs(bot, f'touch {cache}-x', escalated=True, allow=True)
        self.assertTrue(asked)
        self.assertTrue(Path(f'{cache}-x').exists())
        # Allowed always, the same command runs without asking.
        self.daemon.request('configure', bot=bot, sandbox='workspace-write', unsandboxed=[*OWN, 'touch'])
        asked, _ = self.runs(bot, f'touch {cache}-y')
        self.assertFalse(asked)
        self.assertTrue(Path(f'{cache}-y').exists())


if __name__ == '__main__':
    unittest.main()
