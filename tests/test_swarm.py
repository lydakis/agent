"""A swarm's post tool against a real daemon: the board, and who hears a post."""
import json
import os
import threading
import time
import unittest
from pathlib import Path

import subprocess

from bench.targets import clean_env
from tests.test_runtime import ModelFixture

ROOT = Path(__file__).resolve().parent.parent
APP = next((p for p in (ROOT / '.local/target/debug/agent-app', ROOT / '.local/target/release/agent-app') if p.exists()), None)


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
@unittest.skipUnless(APP, 'build the app first: cargo build -p agent-app')
class SwarmPostTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.store = self.path / 'state.sqlite'
        self.common = ['--store', str(self.store), '--provider', f'openai=responses,{self.url}',
                       '--model', 'openai/synthetic-model', '--tools', 'echo,shell']
        self.addCleanup(lambda: subprocess.run([str(self.binary), 'shutdown', '--store', str(self.store)],
                                               env=clean_env(), capture_output=True, timeout=35))

    def agent(self, *args):
        result = subprocess.run([str(self.binary), *args], env=clean_env(), capture_output=True, text=True,
                                timeout=30, cwd=self.path)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return result

    def ids(self):
        return {b['name']: b['id'] for b in json.loads(self.agent('ls', '--store', str(self.store)).stdout)}

    def swarm(self, members, council=0):
        # The folder the app writes for a swarm, by hand: each member pinned to its bot's id.
        folder = self.path / 'swarms' / 'p.s'
        folder.mkdir(parents=True)
        listed = ', '.join(json.dumps(m) for m in members)
        ids = self.ids()
        pinned = ''.join(f'{json.dumps(m)} = {ids[m]}\n' for m in members)
        rows = ''.join(f'{json.dumps(m)} = 0\n' for m in members)
        (folder / 'swarm.toml').write_text(
            f'project = "p"\ngoal = "g"\nworkspace = "{self.path}"\n'
            f'budget_tokens = 1000000\nmembers = [{listed}]\nstopped = false\ncouncil = {council}\n'
            f'[[mix]]\nidentity = ""\nmodel = "openai/synthetic-model"\nshare = 100\n'
            f'[ids]\n{pinned}[rows]\n{rows}')
        (folder / 'board.jsonl').write_text('')
        for tool, flag in [('post', ''), ('propose', ' --propose'), ('vote', ' --vote'), ('join', ' --join')]:
            script = folder / tool
            script.write_text(f'#!/bin/sh\nexec \'{APP}\' --swarm-post \'{folder}\'{flag} "$@"\n')
            script.chmod(0o755)
        return folder

    def turns(self, bot):
        return json.loads(self.agent('turns', '--store', str(self.store), '--bot', bot).stdout)

    def post_from(self, bot, folder, text, tool='post'):
        # The agent runs the script from its shell tool; its output is kept for the test.
        out = self.path / f'posted-{bot}.json'
        ran = self.agent('run', '--store', str(self.store), '--bot', bot,
                         f'shell:"{folder}/{tool}" {text} > "{out}" 2>&1')
        self.assertTrue(out.exists(), ran.stdout)
        return json.loads(out.read_text() or '{}')

    def settle(self, bot):
        self.agent('wait', '--store', str(self.store), f"turn:{bot}/{self.turns(bot)[-1]['turn']}")

    def test_a_post_reaches_working_agents_and_wakes_only_named_idle_ones(self):
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        members = ['p.s-1', 'p.s-2', 'p.s-3', 'p.s-4']
        for bot in members:
            self.agent('run', *self.common, '--new', '--bot', bot, 'hello')
        folder = self.swarm(members)
        # s-2 works on a turn that waits on the model; s-3 and s-4 are idle.
        self.agent('run', '--store', str(self.store), '--bot', 'p.s-2', '--detach', 'gate')
        for _ in range(200):
            if self.turns('p.s-2')[-1]['status'] == 'running':
                break
            time.sleep(.02)
        posted = self.post_from('p.s-1', folder, '"@s-3 take the tests"')
        self.assertEqual((posted['steered'], posted['woke'], posted['missed']), (['s-2'], ['s-3'], []))
        board = [json.loads(line) for line in (folder / 'board.jsonl').read_text().splitlines()]
        self.assertEqual(len(board), 1)
        self.assertEqual((board[0]['from'], board[0]['bot'], board[0]['text']), ('s-1', 'p.s-1', '@s-3 take the tests'))
        self.assertEqual(board[0]['turn'], self.turns('p.s-1')[-1]['turn'])
        # The line says how many agents it reached: what a swarm's posts cost is on its board.
        self.assertEqual(board[0]['reached'], 2)
        # The named idle agent got a turn of its own; the other idle one heard nothing.
        woken = self.turns('p.s-3')[-1]
        self.assertEqual((woken['prompt_preview'], woken['delivery']), ('[board] s-1: @s-3 take the tests', 'steer'))
        self.assertEqual(len(self.turns('p.s-4')), 1)
        # A post naming nobody goes only to the turns running now.
        quiet = self.post_from('p.s-4', folder, 'profile is up')
        self.assertIn('s-2', quiet['steered'])
        self.assertEqual((quiet['woke'], quiet['missed']), ([], []))
        self.model.release_headers.set()
        self.agent('wait', '--store', str(self.store), f"turn:p.s-2/{self.turns('p.s-2')[-1]['turn']}")
        # Posting is for members, and a stopped swarm takes no posts from its agents.
        outsider = self.path / 'outsider.json'
        self.agent('run', *self.common, '--new', '--bot', 'q', f'shell:"{folder}/post" hi > "{outsider}" 2>&1')
        self.assertIn('not_a_member', outsider.read_text())
        # A member deleted and made again under its name is another bot: it cannot post, and a
        # post naming it misses it rather than waking the new bot.
        self.agent('rm', '--store', str(self.store), '--bot', 'p.s-4')
        self.agent('run', *self.common, '--new', '--bot', 'p.s-4', 'hello')
        again = self.path / 'again.json'
        self.agent('run', '--store', str(self.store), '--bot', 'p.s-4', f'shell:"{folder}/post" hi > "{again}" 2>&1')
        self.assertIn('not_a_member', again.read_text())
        named = self.post_from('p.s-1', folder, '"@s-4 you there?"')
        self.assertEqual(([m['agent'] for m in named['missed']], named['woke']), (['s-4'], []))
        self.assertEqual(len(self.turns('p.s-4')), 2)
        toml = folder / 'swarm.toml'
        toml.write_text(toml.read_text().replace('stopped = false', 'stopped = true'))
        stopped = self.path / 'stopped.json'
        self.agent('run', '--store', str(self.store), '--bot', 'p.s-1', f'shell:"{folder}/post" hi > "{stopped}" 2>&1')
        self.assertIn('swarm_stopped', stopped.read_text())
        self.assertEqual(len((folder / 'board.jsonl').read_text().splitlines()), 3)

    def test_a_proposal_wakes_the_seats_and_their_majority_opens_a_stream(self):
        members = ['p.s-1', 'p.s-2', 'p.s-3', 'p.s-4']
        for bot in members:
            self.agent('run', *self.common, '--new', '--bot', bot, 'hello')
        folder = self.swarm(members, council=3)
        proposed = self.post_from('p.s-4', folder, 'conn-pool "reuse provider connections"', 'propose')
        self.assertEqual((proposed['id'], proposed['woke']), ('P1', ['s-1', 's-2', 's-3']))
        for seat in members[:3]:
            self.settle(seat)
            self.assertTrue(self.turns(seat)[-1]['prompt_preview'].startswith('[board] s-4 proposes P1 #conn-pool'))
        self.assertIsNone(self.post_from('p.s-1', folder, 'P1 yes "measured first"', 'vote')['decided'])
        refused = self.post_from('p.s-4', folder, 'P1 yes "mine"', 'vote')
        self.assertTrue(refused['error'].startswith('not_a_seat'))
        decided = self.post_from('p.s-2', folder, 'P1 yes "worth it"', 'vote')
        self.assertEqual((decided['decided'], decided['woke']), ('approved', ['s-4']))
        self.settle('p.s-4')
        self.assertTrue(self.turns('p.s-4')[-1]['prompt_preview'].startswith('[board] P1 #conn-pool approved'))
        state = json.loads((folder / 'state.json').read_text())
        self.assertEqual(state['streams'], {'s-4': 'conn-pool'})
        self.assertEqual(state['proposals'][0]['status'], 'approved')
        # A stream's post names its stream; nobody else is in it, so it reaches nobody.
        self.post_from('p.s-1', folder, 'conn-pool', 'join')
        streamed = self.post_from('p.s-4', folder, '"pool is in"')
        self.assertEqual((streamed['stream'], streamed['steered'], streamed['woke']), ('conn-pool', [], []))
        kinds = [json.loads(line).get('kind', 'post') for line in (folder / 'board.jsonl').read_text().splitlines()]
        self.assertEqual(kinds, ['propose', 'vote', 'vote', 'decision', 'join', 'post'])


    def test_a_coordinator_starts_a_swarm_from_its_shell(self):
        # What the coordinator's role tells it to run: the app, without a window, from its shell.
        self.agent('run', *self.common, '--new', '--bot', 'p.lead', 'hello')
        home = self.path / 'home'
        out = self.path / 'started.json'
        self.agent('run', '--store', str(self.store), '--bot', 'p.lead',
                   f"shell:HOME='{home}' '{APP}' --swarm-start --agents 2 --budget 0.5 --in-project"
                   f" -- Ship the widget > '{out}' 2>&1")
        started = json.loads(out.read_text())
        self.assertIn('swarm', started, started)
        self.assertEqual((started['swarm']['swarm'], started['bots']), ('p.widget', ['p.widget-1', 'p.widget-2']))
        self.assertEqual(started['failed'], [])
        # Every agent runs the coordinator's model, in its folder, with half the budget.
        ids = self.ids()
        self.assertEqual(started['swarm']['ids'], {'p.widget-1': ids['p.widget-1'], 'p.widget-2': ids['p.widget-2']})
        self.assertEqual(started['swarm']['mix'], [{'identity': '', 'model': 'openai/synthetic-model', 'share': 100}])
        self.assertEqual(started['swarm']['workspace'], str(self.path))
        listed = {b['name']: b for b in json.loads(self.agent('ls', '--store', str(self.store)).stdout)}
        self.assertEqual(listed['p.widget-1']['budget_tokens'], 250000)
        # Its folder is the one a window on this store reads, and the goal opens the board.
        board = Path(started['board'])
        self.assertEqual(board.parent.parent.parent, home / '.agent/swarms')
        self.assertEqual(json.loads(board.read_text().splitlines()[0])['text'], 'Ship the widget')
        # Each agent got its brief.
        for bot in started['bots']:
            self.assertIn('Ship the widget', self.turns(bot)[0]['prompt_preview'])
        # Only a coordinator starts one.
        refused = self.path / 'refused.json'
        self.agent('run', *self.common, '--new', '--bot', 'q',
                   f"shell:HOME='{home}' '{APP}' --swarm-start -- x > '{refused}' 2>&1")
        self.assertIn("coordinator's shell", refused.read_text())


if __name__ == '__main__':
    unittest.main()
