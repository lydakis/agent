"""The release smoke: an agent's shell and file calls, here and on a linked
machine, through each of Sonny's permission rows, with nothing stood in.

A real daemon, Sonny's real tool host and a laptop linked to it, with a
daemon and a tool host of its own, as tests/test_served.py sets them up.
In each row a lead's script calls `tools.shell`, `read`, `write`, `edit`
and `apply_patch` once in its folder and once on the laptop, and each call
must run at once, wait on the lead's gate and run once allowed, or be
refused, as the row says. 0.10.8 refused every call on a linked machine
while every unit test passed, the one for that path standing in for the
daemon's gate: this runs the whole path for every tool and row.

The runtime job of test.yml runs it as a step of its own, `Release smoke`
(`python -m tests.test_smoke`, where a skipped test fails it), which
release/green_gate.py requires of the commit a tag names.
"""
import json
import os
import shutil
import subprocess
import sys
import time
import unittest

from bench.targets import clean_env
from tests.test_served import ServedFixture

# Sonny's own permission rows as sonny/src/approvals.rs makes an agent in
# them: `gate` gates every tool but those AUTO_UNGATED names (reads among
# them) for the row's reviewer, and `sandbox` gives the row's sandbox,
# here for a folder that is no repository and a lead with no state folder.
GATED = ['shell', 'write', 'edit', 'apply_patch']
ROWS = {
    'auto': dict(settings={'sandbox': 'workspace-write', 'writable': [], 'network': True},
                 approve=GATED, approver='auto'),
    'ask': dict(settings={'sandbox': 'workspace-write', 'writable': [], 'network': True},
                approve=GATED, approver='manual'),
    'read-only': dict(settings={'sandbox': 'read-only'}, approve=GATED, approver='manual'),
    'full': dict(settings={'sandbox': 'danger-full-access'}),
}

RUNS, ASKS = 'runs', 'asks'
WRITES = ('write', 'edit', 'apply_patch')


def expected(row, tool):
    """How a call of `tool` in the agent's folder goes in `row`, here or on
    the laptop alike, since a call on another machine is decided as the
    same call here would be: only a read-only row's writes leave its
    sandbox, and wait for its reviewer."""
    return ASKS if row == 'read-only' and tool in WRITES else RUNS


def call(tool, arguments):
    """A script making one call of `tool`: what it said, or why it failed."""
    return (f'const r = await tools.{tool}({json.dumps(arguments)}).then(\n'
            '  r => (r.isError ? "failed: " : "") + r.content.map(c => c.text).join("\\n"),\n'
            '  e => "refused: " + e.message);\n'
            'text(r);')


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
@unittest.skipUnless(shutil.which('bwrap') or sys.platform == 'darwin', 'sandboxes need bubblewrap')
class ReleaseSmoke(ServedFixture):
    def setUp(self):
        super().setUp()
        self.laptop, _ = self.linked(link='writable = ["~/lent"]')
        self.lent = self.laptop / 'lent'
        self.lent.mkdir()
        self.work = self.path / 'work'
        self.work.mkdir()
        for folder, where in ((self.work, 'the box'), (self.lent, 'the laptop')):
            (folder / 'notes.txt').write_text(f'on {where}\n')
            (folder / 'edit.txt').write_text('before\n')
        self.tool = json.loads(subprocess.run([str(self.sonny), 'mcp', 'tool'], env={**clean_env(), 'HOME': str(self.path)},
                                              check=True, capture_output=True, text=True).stdout)
        self.assertIn('# On other machines', self.tool['description'])

    def lead(self, row):
        """The lead, made in `row`. The tool host's first look gives it
        Sonny's tools and gates them as the row says, as it does every
        agent of Sonny's: the gate checked here is Sonny's own."""
        self.row = row
        settings = {**ROWS[row]['settings'], 'steps_only': ['*']}
        self.daemon.request('create', bot='calc.lead', workspace=str(self.work), model='openai/synthetic-model',
                            instructions='Lead.', tools=['echo', 'shell', 'read', 'write', 'edit', 'apply_patch'],
                            declare=[self.tool], **{**ROWS[row], 'settings': settings})
        host = self.host()
        while not ((line := json.loads(host.stdout.readline()))['event'] == 'tools_refreshed'
                   and line['bot'] == 'calc.lead'):
            pass
        record = self.daemon.request('inspect', bot='calc.lead')['result']
        self.assertEqual(record['settings']['sandbox'], settings['sandbox'])
        gates = {g['tag']: g['tools'] for g in record['gates']}
        if 'approver' not in ROWS[row]:
            self.assertEqual(gates, {})
            return
        self.assertEqual(list(gates), [ROWS[row]['approver']])
        tools = gates[ROWS[row]['approver']]
        self.assertTrue(set(GATED) <= set(tools), tools)
        self.assertNotIn('read', tools)

    def run_script(self, code):
        """The lead runs `code` in one `exec` call: its turn."""
        self.script.append(('exec', code))
        self.model.call_script = self.script
        return self.daemon.request('submit', bot='calc.lead', request_id=f'smoke-{len(self.script)}',
                                   prompt='script')['result']['message']

    def settle(self, turn, decision=None):
        """Wait for `turn` to end, answering its one ask with `decision`;
        the ask, which is there exactly when a decision was given."""
        asked, deadline = None, time.monotonic() + 30
        while True:
            [status] = [t['status'] for t in self.daemon.request('turns', bot='calc.lead')['result']['turns']
                        if t['turn'] == turn]
            if status in ('completed', 'failed', 'interrupted', 'withdrawn'):
                break
            pending = self.daemon.request('approvals', bot='calc.lead')['result']['approvals']
            if pending:
                self.assertIsNone(asked, f'asked twice: {pending}')
                self.assertIsNotNone(decision, f'asked when it should have run: {pending}')
                asked = pending[0]
                # Waiting on the row's reviewer: Sonny's reviewer model
                # serves `auto`, and the smoke answers as it would.
                tagged = self.daemon.request('approvals', tag=ROWS[self.row]['approver'])['result']['approvals']
                self.assertIn(asked['call_id'], [a['call_id'] for a in tagged])
                self.daemon.request('answer', bot='calc.lead', turn=turn, call_id=asked['call_id'],
                                    request=asked.get('request', 1), decision=decision, by='smoke')
            self.assertLess(time.monotonic(), deadline, f'turn {turn} never ended ({status})')
            time.sleep(.02)
        self.assertEqual(status, 'completed')
        self.assertTrue(asked or decision is None, 'ran without asking')
        return asked

    def made(self, tool, arguments, decision=None):
        """What the lead's call of `tool` said, its ask answered with
        `decision`; the gate, when it asked, saw the call as made."""
        turn = self.run_script(call(tool, arguments))
        asked = self.settle(turn, decision)
        if asked:
            self.assertEqual((asked['name'], asked['arguments']), (tool, arguments))
        return self.output('calc.lead', turn)

    def calls(self, row):
        self.lead(row)
        gated = 'approver' in ROWS[row]
        # Every tool in the agent's folder, here and on the laptop, where
        # the link's first writable folder is where it starts.
        for there in (False, True):
            folder, where = (self.lent, 'the laptop') if there else (self.work, 'the box')
            machine = {'machine': 'laptop'} if there else {}
            made = {
                'shell': ({'command': 'cat notes.txt'}, lambda said: self.assertIn(f'on {where}', said)),
                'read': ({'path': 'notes.txt'}, lambda said: self.assertIn(f'on {where}', said)),
                'write': ({'path': 'written.txt', 'content': 'written\n'},
                          lambda _: self.assertEqual((folder / 'written.txt').read_text(), 'written\n')),
                'edit': ({'path': 'edit.txt', 'old': 'before', 'new': 'after'},
                         lambda _: self.assertEqual((folder / 'edit.txt').read_text(), 'after\n')),
                'apply_patch': ({'input': '*** Begin Patch\n*** Add File: patched.txt\n+patched\n*** End Patch\n'},
                                lambda _: self.assertEqual((folder / 'patched.txt').read_text(), 'patched\n')),
            }
            for tool, (arguments, ran) in made.items():
                with self.subTest(row=row, tool=tool, where=where):
                    said = self.made(tool, {**arguments, **machine},
                                     'allow' if expected(row, tool) == ASKS else None)
                    self.assertNotIn('refused: ', said)
                    self.assertNotIn('failed: ', said)
                    ran(said)
        # What leaves the sandbox here waits for the row's reviewer, and
        # runs once allowed; with no reviewer it runs at once.
        outside = self.path / 'outside'
        outside.mkdir()
        decision = 'allow' if gated else None
        with self.subTest(row=row, call='write outside the folder'):
            self.made('write', {'path': str(outside / 'written.txt'), 'content': 'out\n'}, decision)
            self.assertEqual((outside / 'written.txt').read_text(), 'out\n')
        with self.subTest(row=row, call='escalated shell'):
            self.made('shell', {'command': f'touch {outside}/escaped', 'sandbox_permissions': 'require_escalated',
                                'justification': 'Needs to.'}, decision)
            self.assertTrue((outside / 'escaped').exists())
        # On the laptop, the laptop's sandbox holds whatever was allowed
        # here: a write past its folders is refused there, and a command
        # is never escalated, refused before anyone is asked.
        away = self.laptop / 'away'
        away.mkdir()
        with self.subTest(row=row, call='write outside the laptop\'s folders'):
            said = self.made('write', {'path': str(away / 'written.txt'), 'content': 'out\n', 'machine': 'laptop'},
                             decision)
            self.assertIn('write_denied', said)
            self.assertFalse((away / 'written.txt').exists())
        with self.subTest(row=row, call='escalated shell on the laptop'):
            said = self.made('shell', {'command': f'touch {away}/escaped', 'sandbox_permissions': 'require_escalated',
                                       'justification': 'Needs to.', 'machine': 'laptop'})
            self.assertIn('invalid_arguments', said)
            self.assertFalse((away / 'escaped').exists())
        # One its tool does not take is refused before the gate, asking no one.
        with self.subTest(row=row, call='a call its tool does not take'):
            self.assertIn('invalid_arguments', self.made('shell', {'machine': 'laptop'}))
        if not gated:
            return
        # Denied, a call does not run, here or on the laptop.
        with self.subTest(row=row, call='denied'):
            said = self.made('write', {'path': str(outside / 'denied.txt'), 'content': 'no\n'}, 'deny')
            self.assertIn('not approved', said)
            self.assertFalse((outside / 'denied.txt').exists())
            if row == 'read-only':
                said = self.made('write', {'path': 'denied.txt', 'content': 'no\n', 'machine': 'laptop'}, 'deny')
                self.assertIn('not approved', said)
                self.assertFalse((self.lent / 'denied.txt').exists())

    def test_auto(self):
        self.calls('auto')

    def test_ask_me(self):
        self.calls('ask')

    def test_read_only(self):
        self.calls('read-only')

    def test_full_access(self):
        self.calls('full')


if __name__ == '__main__':
    # As the gate runs it: a test skipped, as with no sandbox to run in,
    # fails it, since a smoke that skipped checked nothing.
    result = unittest.main(exit=False, verbosity=2).result
    sys.exit(0 if result.wasSuccessful() and result.testsRun and not result.skipped else 1)
