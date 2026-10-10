"""The swarm skill's script against a real daemon: start, the board, who hears a post, the work
contract, the budget notices and stop."""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

from bench.targets import clean_env
from tests.test_runtime import ModelFixture

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / 'app/skills/swarm/swarm'


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class SwarmTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.store = self.path / 'state.sqlite'
        self.home = self.path / 'home'
        self.home.mkdir()
        self.common = ['--store', str(self.store), '--provider', f'openai=responses,{self.url}',
                       '--model', 'openai/synthetic-model', '--tools', 'echo,shell']
        self.addCleanup(lambda: subprocess.run([str(self.binary), 'shutdown', '--store', str(self.store)],
                                               env=clean_env(), capture_output=True, timeout=35))

    def agent(self, *args):
        result = subprocess.run([str(self.binary), *args], env=clean_env(), capture_output=True, text=True,
                                timeout=30, cwd=self.path)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return result

    def listed(self):
        return {b['name']: b for b in json.loads(self.agent('ls', '--store', str(self.store)).stdout)}

    def turns(self, bot):
        return json.loads(self.agent('turns', '--store', str(self.store), '--bot', bot).stdout)

    def settle(self, *bots):
        for bot in bots:
            self.agent('wait', '--store', str(self.store), f"turn:{bot}/{self.turns(bot)[-1]['turn']}")

    def swarm(self, bot, *args):
        """The script run from an agent's shell, as its tool call would; what it printed, parsed."""
        out = self.path / f'out-{bot}.json'
        words = ' '.join("'" + a.replace("'", "'\\''") + "'" for a in args)
        self.agent('run', '--store', str(self.store), '--bot', bot,
                   f"shell:HOME='{self.home}' '{SCRIPT}' {words} > '{out}' 2>&1")
        return json.loads(out.read_text() or '{}')

    def person(self, *args):
        """The script as the app runs it for the person: no agent in its environment."""
        env = clean_env() | {'HOME': str(self.home), 'AGENT_SOCKET': str(self.store) + '.sock',
                             'AGENT_BIN': str(self.binary)}
        ran = subprocess.run([str(SCRIPT), *args], env=env, capture_output=True, text=True, timeout=30)
        return json.loads(ran.stdout if ran.returncode == 0 else ran.stderr)

    def start(self, *args, goal='Ship the widget'):
        self.agent('run', *self.common, '--new', '--bot', 'p.lead', 'hello')
        started = self.swarm('p.lead', 'start', '--in-project', *args, '--', goal)
        self.assertIn('swarm', started, started)
        self.settle(*started['bots'])
        return started

    def board(self, started):
        return [json.loads(line) for line in Path(started['board']).read_text().splitlines()]

    def test_a_coordinator_starts_a_swarm_from_its_shell(self):
        started = self.start('--agents', '2', '--budget', '0.5')
        self.assertEqual((started['swarm']['swarm'], started['bots']), ('p.widget', ['p.widget-1', 'p.widget-2']))
        self.assertEqual(started['failed'], [])
        # Every agent runs the coordinator's model, in its folder, with half the budget.
        listed = self.listed()
        self.assertEqual(started['swarm']['ids'], {m: listed[m]['bot_id'] for m in started['bots']})
        self.assertEqual(started['swarm']['mix'], [{'model': 'openai/synthetic-model', 'share': 100,
                                                    'identity': '', 'effort': None}])
        self.assertEqual(started['swarm']['workspace'], str(self.path))
        self.assertEqual(started['swarm']['coordinator'], {'bot': 'p.lead', 'id': listed['p.lead']['bot_id']})
        self.assertEqual(listed['p.widget-1']['budget_tokens'], 250000)
        self.assertEqual(listed['p.widget-1']['created_by'], 'p.lead')
        # Its folder is the one a window on this store reads, and the goal opens the board.
        board = Path(started['board'])
        self.assertEqual(board.parent.parent.parent, self.home / '.agent/swarms')
        self.assertEqual(self.board(started)[0]['text'], 'Ship the widget')
        # Each agent got its rules and its brief as its first message.
        first = self.turns('p.widget-1')[0]['prompt_preview']
        self.assertTrue(first.startswith('You are one of several agents in a flat swarm'), first)
        # Its coordinator posts for the person, as the person would.
        posted = self.swarm('p.lead', 'post', '--swarm', 'p.widget', '@widget-1 use the small fixture')
        self.assertEqual(posted['woke'], ['widget-1'])
        self.assertEqual(self.board(started)[-1]['from'], 'user')
        self.settle('p.widget-1')
        # A turn run at another effort starts its swarm at that effort.
        self.agent('run', '--store', str(self.store), '--bot', 'p.lead', '--effort', 'xhigh',
                   f"shell:HOME='{self.home}' '{SCRIPT}' start --agents 1 --budget 0.5 --in-project -- Ship the gadget"
                   f" > '{self.path}/again.json' 2>&1")
        gadget = json.loads((self.path / 'again.json').read_text())
        self.assertEqual(gadget['swarm']['mix'][0]['effort'], 'xhigh')
        self.assertEqual(self.listed()['p.gadget-1']['effort'], 'xhigh')
        # Only a coordinator starts one, and the app lists both.
        self.agent('run', *self.common, '--new', '--bot', 'q', 'hello')
        self.assertEqual(self.swarm('q', 'start', '--', 'x')['error'], 'coordinators_only')
        self.assertEqual([s['swarm'] for s in self.person('list')['swarms']], ['p.gadget', 'p.widget'])

    def test_a_shared_worktree_runs_setup_and_a_failed_setup_leaves_nothing(self):
        repo = self.path / 'repo'
        repo.mkdir()
        git = lambda *a: subprocess.run(['git', '-C', str(repo), *a], check=True, capture_output=True,
                                        env=clean_env() | {'HOME': str(self.home), 'GIT_AUTHOR_NAME': 't', 'GIT_AUTHOR_EMAIL': 't@t',
                                                           'GIT_COMMITTER_NAME': 't', 'GIT_COMMITTER_EMAIL': 't@t'})
        (repo / '.agents').mkdir()
        (repo / '.agents/setup').write_text('#!/bin/sh\n[ -f fail ] && exit 3\necho "$AGENT_SOURCE" > set-up\n')
        (repo / '.agents/setup').chmod(0o755)
        git('init', '-q')
        git('add', '.')
        git('commit', '-qm', 'init')
        self.agent('run', *self.common, '--new', '--bot', 'r.lead', '--workspace', str(repo), 'hello')
        started = self.swarm('r.lead', 'start', '--agents', '1', '--', 'Ship the widget')
        tree = self.home / '.agent/worktrees/r.widget'
        self.assertEqual(started['swarm']['workspace'], str(tree))
        self.assertEqual((tree / 'set-up').read_text().strip(), str(repo))
        self.assertEqual(self.listed()['r.widget-1']['workspace'], str(tree))
        (repo / 'fail').write_text('')
        git('add', 'fail')
        git('commit', '-qm', 'fail')
        failed = self.swarm('r.lead', 'start', '--agents', '1', '--', 'Ship the widget')
        self.assertEqual(failed['error'], 'setup_failed')
        self.assertFalse((self.home / '.agent/worktrees/r.widget-2').exists())
        self.assertNotIn('agent/r.widget-2', git('branch').stdout.decode())
        # A setup that cannot even start leaves nothing either.
        (repo / '.agents/setup').write_text('#!/no/such/shell\n')
        git('commit', '-qam', 'unrunnable')
        failed = self.swarm('r.lead', 'start', '--agents', '1', '--', 'Ship the widget')
        self.assertEqual(failed['error'], 'setup_failed')
        self.assertIn('could not run', failed['detail'])
        self.assertFalse((self.home / '.agent/worktrees/r.widget-2').exists())
        # The profile the agents get is the worktree's, not an edit not yet committed.
        (repo / '.agents/setup').unlink()
        (repo / '.agents/agents').mkdir()
        (repo / '.agents/agents/narrow.md').write_text('---\ntools: [read]\n---\n')
        git('add', '-A')
        git('commit', '-qm', 'narrow')
        (repo / '.agents/agents/narrow.md').write_text('---\ntools: [read, shell]\n---\n')
        failed = self.swarm('r.lead', 'start', '--agents', '1', '--row', 'openai/synthetic-model,100,narrow', '--', 'Ship the widget')
        self.assertEqual(failed['error'], 'identity_without_shell')
        self.assertFalse((self.home / '.agent/worktrees/r.widget-2').exists())
        self.assertNotIn('agent/r.widget-2', git('branch').stdout.decode())
        self.assertEqual([s['swarm'] for s in self.person('list')['swarms']], ['r.widget'])

    def test_publication_is_silent_and_mentions_deliver_only_to_named_members(self):
        started = self.start('--agents', '4')
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        # widget-2 works on a turn that waits on the model; widget-3 and widget-4 are idle.
        self.agent('run', '--store', str(self.store), '--bot', 'p.widget-2', '--detach', 'gate')
        for _ in range(200):
            if self.turns('p.widget-2')[-1]['status'] == 'running':
                break
            time.sleep(.02)
        posted = self.swarm('p.widget-1', 'post', '@widget-3 take the tests')
        self.assertEqual((posted['steered'], posted['woke'], posted['missed']), ([], ['widget-3'], []))
        line = self.board(started)[-1]
        self.assertEqual((line['from'], line['bot'], line['text'], line['sent']),
                         ('widget-1', 'p.widget-1', '@widget-3 take the tests', 1))
        self.assertEqual(line['turn'], self.turns('p.widget-1')[-1]['turn'])
        woken = self.turns('p.widget-3')[-1]
        self.assertEqual((woken['prompt_preview'], woken['delivery']), ('[board] widget-1: @widget-3 take the tests', 'steer'))
        self.assertEqual(len(self.turns('p.widget-4')), 1)
        # A routine post interrupts nobody, not even a working peer.
        quiet = self.swarm('p.widget-4', 'post', 'profile is up')
        self.assertEqual((quiet['steered'], quiet['woke'], quiet['missed']), ([], [], []))
        # --all reaches everyone: the working one in its turn, the idle ones with a turn.
        everyone = self.swarm('p.widget-4', 'post', '--all', 'plan is up')
        self.assertEqual((everyone['steered'], everyone['woke']), (['widget-2'], ['widget-1', 'widget-3']))
        self.model.release_headers.set()
        self.settle('p.widget-1', 'p.widget-2', 'p.widget-3')
        # Posting is for members.
        self.agent('run', *self.common, '--new', '--bot', 'q', 'hello')
        self.assertEqual(self.swarm('q', 'post', '--swarm', 'p.widget', 'hi')['error'], 'not_a_member')
        # A member deleted and made again under its name is another bot: it cannot post, and a
        # post naming it reaches nobody rather than waking the new bot.
        self.agent('rm', '--store', str(self.store), '--bot', 'p.widget-4')
        self.agent('run', *self.common, '--new', '--bot', 'p.widget-4', 'hello')
        self.assertEqual(self.swarm('p.widget-4', 'post', 'hi')['error'], 'not_a_member')
        named = self.swarm('p.widget-1', 'post', '@widget-4 you there?')
        self.assertEqual((named['missed'], named['woke'], named['steered']), ([], [], []))
        self.assertEqual(len(self.turns('p.widget-4')), 2)
        # Stopped, it refuses its agents' posts; the person's post resumes it and wakes everyone.
        stopped = self.person('stop', '--swarm', 'p.widget')
        self.assertEqual((stopped['swarm']['stopped'], stopped['failed']), (True, []))
        self.assertEqual(self.swarm('p.widget-1', 'post', 'hi')['error'], 'swarm_stopped')
        resumed = self.person('post', '--swarm', 'p.widget', 'carry on')
        self.assertEqual(resumed['woke'], ['widget-1', 'widget-2', 'widget-3'])
        self.assertFalse(self.person('list')['swarms'][0]['stopped'])

    def test_the_work_contract_and_its_status(self):
        started = self.start('--agents', '2')
        self.swarm('p.widget-1', 'post', '--all', 'Deliverable: source evidence.')
        assigned = self.swarm('p.widget-1', 'assign', 'waits', 'widget-1', 'widget-2', 'Inspect cleanup; report evidence')
        self.assertEqual((assigned['task'], assigned['status']), ('waits', 'assigned'))
        self.assertEqual(self.swarm('p.widget-2', 'assign', 'waits', 'widget-1', 'widget-2', 'Inspect cleanup; report evidence')['error'], 'task_exists')
        self.assertEqual(self.swarm('p.widget-1', 'assign', 'more', 'widget-1', 'widget-2', 'x')['error'], 'owner_busy')
        self.assertEqual(self.swarm('p.widget-2', 'claim', 'waits')['error'], 'task_owner_only')
        # A malformed command does nothing.
        self.assertEqual(self.swarm('p.widget-1', 'claim', 'waits', 'typo')['error'], 'usage')
        self.assertEqual(self.person('stop', '--swarm', 'p.widget', 'typo')['error'], 'usage')
        self.assertEqual(self.swarm('p.widget-1', 'claim', 'waits')['status'], 'working')
        self.assertEqual(self.swarm('p.widget-1', 'review', 'waits', 'supported', 'mine')['error'], 'reviewer_only')
        submitted = self.swarm('p.widget-1', 'submit', 'waits', 'Source inspected; no measured latency')
        # The reviewer is told; it was idle, so it got a turn of its own.
        self.assertEqual((submitted['status'], submitted['woke']), ('reviewing', ['widget-2']))
        self.settle('p.widget-2')
        self.assertTrue(self.turns('p.widget-2')[-1]['prompt_preview'].startswith('[board] waits submitted for your review'))
        partial = self.person('status', '--swarm', 'p.widget')
        self.assertIsNone(partial['result'])
        self.assertEqual(partial['tasks']['waits']['status'], 'reviewing')
        self.swarm('p.widget-2', 'review', 'waits', 'conditional', 'Verified source; only helper-heavy workloads exercise it')
        finished = self.swarm('p.widget-1', 'finish', 'partial', 'One conditional finding, no measured speedup')
        # The coordinator hears the result after what it is doing.
        self.assertEqual(finished['woke'], ['lead'])
        self.settle('p.lead')
        self.assertTrue(self.turns('p.lead')[-1]['prompt_preview'].startswith('[swarm p.widget] final result from widget-1: partial'))
        done = self.person('status', '--swarm', 'p.widget')
        self.assertEqual((done['outcome'], done['result']['outcome']), ('completed', 'partial'))
        self.assertEqual(done['tasks']['waits']['verdict'], 'conditional')
        self.assertEqual([m['name'] for m in done['members']], ['p.widget-1', 'p.widget-2'])
        # The state is the board folded: without its cache, the same.
        (Path(started['board']).parent / 'state.json').unlink()
        again = self.person('status', '--swarm', 'p.widget')
        self.assertEqual((again['tasks'], again['result']), (done['tasks'], done['result']))
        # A new assignment reopens the work, and its answer says so.
        self.assertIs(self.swarm('p.widget-2', 'assign', 'next', 'widget-2', 'widget-1', 'One more check')['completed'], False)
        self.swarm('p.widget-1', 'finish', 'partial', 'One conditional finding, no measured speedup; next is open')
        kinds = [line.get('kind', 'post') for line in self.board(started)]
        self.assertEqual(kinds, ['post', 'post', 'assign', 'claim', 'submit', 'review', 'finish', 'assign', 'finish'])

    def test_an_added_agent_takes_the_row_a_deleted_one_left(self):
        row = 'openai/synthetic-model,50'
        started = self.start('--agents', '2', '--row', row, '--row', row)
        self.assertEqual(started['swarm']['rows'], {'p.widget-1': 0, 'p.widget-2': 1})
        self.agent('rm', '--store', str(self.store), '--bot', 'p.widget-2')
        added = self.person('add', '--swarm', 'p.widget')
        self.assertEqual(added['bots'], ['p.widget-3'])
        self.assertEqual(added['swarm']['rows']['p.widget-3'], 1)
        self.assertEqual(self.listed()['p.widget-3']['budget_tokens'], self.listed()['p.widget-1']['budget_tokens'])
        # The board says who joined, with the share of the new budget used, so later shares are news.
        joined = self.board(started)[-1]
        self.assertEqual((joined['kind'], joined['member'], joined['spent']), ('joined', 'widget-3', 0))
        self.assertEqual(self.person('add', '--swarm', 'p.widget', '--row')['error'], 'usage')

    def test_two_claims_at_once_have_one_winner(self):
        started = self.start('--agents', '3')
        self.swarm('p.widget-1', 'assign', 'waits', 'widget-2', 'widget-3', 'Inspect cleanup')
        # Two scripts claim the same task at once, as one agent's two shells could.
        env = clean_env() | {'HOME': str(self.home), 'AGENT_SOCKET': str(self.store) + '.sock',
                             'AGENT_BOT': 'p.widget-2', 'AGENT_TURN': '1',
                             'AGENT_BOT_ID': str(started['swarm']['ids']['p.widget-2'])}
        claims = [subprocess.Popen([str(SCRIPT), 'claim', 'waits'], env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True) for _ in range(4)]
        codes = sorted(c.wait(timeout=30) for c in claims)
        self.assertEqual(codes, [0, 1, 1, 1])
        self.assertEqual([line.get('kind') for line in self.board(started)].count('claim'), 1)

    def test_the_coordinator_hears_once_when_nothing_runs_and_there_is_no_result(self):
        started = self.start('--agents', '2')
        quiet = self.person('check', '--swarm', 'p.widget')
        self.assertEqual((quiet['woke'], quiet['board_changed']), (['lead'], True))
        self.settle('p.lead')
        prompt = self.turns('p.lead')[-1]['prompt_preview']
        self.assertTrue(prompt.startswith('[swarm p.widget] nothing is running and there is no final result (partial)'), prompt)
        self.assertEqual(self.person('check', '--swarm', 'p.widget')['board_changed'], False)
        # Once a member has acted, quiet again is news again.
        self.swarm('p.widget-1', 'role', 'tester')
        self.assertEqual(self.person('check', '--swarm', 'p.widget')['woke'], ['lead'])
        self.assertEqual([line.get('kind') for line in self.board(started)].count('quiet'), 2)

    def test_stop_ends_members_and_helpers_turns(self):
        started = self.start('--agents', '2')
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        # A helper a member made, named after it, works too.
        self.agent('run', '--store', str(self.store), '--bot', 'p.widget-1',
                   f"shell:AGENT_STORE='{self.store}' '{self.binary}' fork --source p.widget-1 --bot p.widget-1.tests")
        for bot in ('p.widget-1', 'p.widget-1.tests'):
            self.agent('run', '--store', str(self.store), '--bot', bot, '--detach', 'gate')
        for _ in range(200):
            if all(self.turns(b)[-1]['status'] == 'running' for b in ('p.widget-1', 'p.widget-1.tests')):
                break
            time.sleep(.02)
        self.assertEqual([m['name'] for m in self.person('status', '--swarm', 'p.widget')['members'] if m.get('helper')],
                         ['p.widget-1.tests'])
        stopped = self.person('stop', '--swarm', 'p.widget')
        self.assertEqual(stopped['failed'], [])
        for bot in ('p.widget-1', 'p.widget-1.tests'):
            self.assertEqual(self.turns(bot)[-1]['status'], 'interrupted')
        self.assertEqual(self.person('status', '--swarm', 'p.widget')['outcome'], 'stopped')
        self.assertEqual(len(self.board(started)), 1)


def load():
    """The script as a module, for its rules without a daemon."""
    import importlib.machinery
    import importlib.util
    loader = importlib.machinery.SourceFileLoader('swarm_skill', str(SCRIPT))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader('swarm_skill', loader))
    # No __pycache__ beside the script: the app bundles that folder as it is.
    written, sys.dont_write_bytecode = sys.dont_write_bytecode, True
    try:
        loader.exec_module(module)
    finally:
        sys.dont_write_bytecode = written
    return module


class SwarmRuleTests(unittest.TestCase):
    def setUp(self):
        self.s = load()
        self.swarm = {'name': 'p.w', 'project': 'p', 'members': ['p.w-1', 'p.w-2', 'p.w-3'], 'ids': {'p.w-1': 1, 'p.w-2': 2, 'p.w-3': 3},
                      'coordinator': {'bot': 'p.lead', 'id': 9}, 'stopped': False}

    def act(self, author=None, state=None, live=None):
        folder = type('Folder', (), {'swarm': self.swarm, 'state': state or self.s.empty_state()})()
        # The daemon's member rows by name; a set names members with no usage.
        live = {m: {} for m in live} if isinstance(live, set) else live
        return folder, self.s.Act(folder, author, 1, live)

    def bot(self, n, used, cap=1000, turn=None):
        return {'name': f'p.w-{n}', 'bot_id': n, 'tokens_used': used, 'budget_tokens': cap, 'running_turn': turn}

    def test_a_deleted_helper_still_counts_and_a_deleted_member_takes_its_share(self):
        state = self.s.empty_state()
        members = {'p.w-1': self.bot(1, 100), 'p.w-2': self.bot(2, 50)}
        helper = {'name': 'p.w-1.fix', 'bot_id': 7, 'tokens_used': 30, 'root': 1}
        self.assertEqual(self.s.usage(state, members, [helper]), (180, 2000))
        self.assertTrue(state.pop('dirty'))
        self.assertEqual(self.s.usage(state, members, [helper]), (180, 2000))
        self.assertNotIn('dirty', state)
        # Once gone, a helper is its member's sum, not an id kept forever,
        # unless a helper it made is still listed.
        deeper = {'name': 'p.w-1.fix.more', 'bot_id': 8, 'tokens_used': 5, 'root': 1, 'created_by_id': 7}
        self.assertEqual(self.s.usage(state, members, [deeper]), (185, 2000))
        self.assertEqual((state['helpers'], state['gone']), ({'7': [0, 1], '8': [5, 1]}, {'1': 30}))
        state.pop('dirty')
        self.assertEqual(self.s.usage(state, members, [deeper]), (185, 2000))
        self.assertNotIn('dirty', state)
        self.assertEqual(self.s.usage(state, members, []), (185, 2000))
        self.assertEqual((state['helpers'], state['gone']), ({}, {'1': 35}))
        del members['p.w-1']
        self.assertEqual(self.s.usage(state, members, []), (50, 1000))
        self.assertEqual(state['gone'], {})

    def test_names_drop_the_whole_project_even_with_dots(self):
        self.assertEqual(self.s.short('foo.bar.widget-1', {'project': 'foo.bar'}), 'widget-1')
        self.assertEqual(self.s.short('p.w-1.fix', self.swarm), 'w-1.fix')

    def test_posts_reach_who_they_name_and_everyone_only_when_asked(self):
        reach = lambda act: sorted(m for m, how, _, _ in act.sends)
        _, act = self.act('p.w-1')
        self.s.post(act, 'profile is up, see notes.md', False)
        self.assertEqual(reach(act), [])
        _, act = self.act('p.w-1')
        self.s.post(act, 'over to @w-3. And @p.w-2.', False)
        self.assertEqual(reach(act), ['p.w-2', 'p.w-3'])
        _, act = self.act('p.w-1')
        self.s.post(act, 'plan', True)
        self.assertEqual(reach(act), ['p.w-2', 'p.w-3'])
        _, act = self.act()
        self.s.post(act, 'carry on', False)
        self.assertEqual(reach(act), ['p.w-1', 'p.w-2', 'p.w-3'])
        # The person's @name that matches nobody reaches nobody, and says so.
        _, act = self.act()
        self.s.post(act, '@w-9 look at this', False)
        self.assertEqual((reach(act), act.answer['unmatched']), ([], ['w-9']))

    def test_each_share_of_a_budget_is_told_once_and_only_to_working_agents(self):
        folder, act = self.act('p.w-1')
        members = {'p.w-1': self.bot(1, 700, turn=4), 'p.w-2': self.bot(2, 100, turn=7), 'p.w-3': self.bot(3, 900)}
        self.s.budget(act, folder, members, [])
        # 1700 of 3000 passes 50% of the swarm's; w-1 is past 65% of its own, w-3 is idle.
        self.assertEqual([(l['from'], l.get('member'), l['spent']) for l in act.lines], [('budget', None, 50), ('budget', 'w-1', 65)])
        self.assertEqual(sorted((m, how) for m, how, _, _ in act.sends), [('p.w-2', 'steer')])
        for line in act.lines:
            self.s.apply(folder.state, line)
        folder, act = self.act('p.w-2', folder.state)
        self.s.budget(act, folder, members, [])
        self.assertEqual(act.lines, [])
        # A call can overshoot a cap; what remains is never below none.
        members['p.w-2'] = self.bot(2, 1100, turn=7)
        folder, act = self.act('p.w-2', folder.state)
        self.s.budget(act, folder, members, [])
        self.assertIn('0 remain', act.lines[-1]['text'])

    def test_the_state_is_the_board_folded(self):
        state = self.s.empty_state()
        for line in [{'from': 'w-1', 'kind': 'assign', 'stream': 't', 'owner': 'w-1', 'reviewer': 'w-2', 'brief': 'b', 'at': 1},
                     {'from': 'w-1', 'kind': 'claim', 'stream': 't', 'at': 2},
                     {'from': 'w-1', 'kind': 'leave', 'stream': 't', 'at': 3},
                     {'from': 'w-1', 'kind': 'claim', 'stream': 't', 'at': 4},
                     {'from': 'w-1', 'kind': 'submit', 'stream': 't', 'text': 'done', 'at': 5},
                     {'from': 'w-2', 'kind': 'review', 'stream': 't', 'verdict': 'supported', 'evidence': 'ran it', 'at': 6},
                     {'from': 'w-1', 'kind': 'finish', 'outcome': 'achieved', 'text': 'shipped', 'at': 7}]:
            self.s.apply(state, line)
        self.assertEqual(state['tasks']['t'], {'owner': 'w-1', 'reviewer': 'w-2', 'brief': 'b', 'status': 'reviewed',
                                               'result': 'done', 'verdict': 'supported', 'evidence': 'ran it'})
        self.assertEqual((state['streams'], state['result']['outcome']), ({}, 'achieved'))

    def test_a_reviewer_that_left_is_replaced_and_the_result_keeps_its_author(self):
        state = self.s.empty_state()
        state['tasks']['t'] = {'owner': 'w-1', 'reviewer': 'w-9', 'brief': 'b', 'status': 'reviewing', 'result': 'r',
                               'verdict': None, 'evidence': None}
        _, act = self.act('p.w-1', state)
        self.s.assign(act, 't', 'w-1', 'w-3', 'please review')
        self.s.apply(state, act.lines[0])
        self.assertEqual((state['tasks']['t']['reviewer'], state['tasks']['t']['result']), ('w-3', 'r'))
        self.assertEqual([m for m, *_ in act.sends], ['p.w-3'])
        _, act = self.act('p.w-1', state, {'p.w-1', 'p.w-2'})
        state['tasks']['t']['reviewer'] = 'w-3'
        with self.assertRaises(self.s.Refused) as refused:
            self.s.assign(act, 't', 'w-1', 'w-1', 'I will')
        self.assertEqual(refused.exception.code, 'independent_review_required')

    def test_a_deleted_member_takes_no_work_and_its_review_is_handed_on(self):
        state = self.s.empty_state()
        state['tasks']['t'] = {'owner': 'w-1', 'reviewer': 'w-2', 'brief': 'b', 'status': 'reviewing', 'result': 'r',
                               'verdict': None, 'evidence': None}
        live = {'p.w-1', 'p.w-3'}
        _, act = self.act('p.w-1', state, live)
        with self.assertRaises(self.s.Refused) as refused:
            self.s.assign(act, 'u', 'w-3', 'w-2', 'new work')
        self.assertEqual(refused.exception.code, 'not_a_member')
        self.s.assign(act, 't', 'w-1', 'w-3', 'please review')
        self.assertEqual(act.lines[0]['reviewer'], 'w-3')
        # Finished work stays finished when one who did it is deleted.
        state['tasks']['t'].update(status='reviewed', reviewer='w-2')
        _, act = self.act('p.w-1', state, live)
        with self.assertRaises(self.s.Refused) as refused:
            self.s.assign(act, 't', 'w-1', 'w-3', 'again')
        self.assertEqual(refused.exception.code, 'task_exists')

    def test_a_member_out_of_tokens_hands_its_work_on_and_takes_none(self):
        state = self.s.empty_state()
        state['tasks']['t'] = {'owner': 'w-1', 'reviewer': 'w-2', 'brief': 'b', 'status': 'working', 'result': None,
                               'verdict': None, 'evidence': None}
        live = {'p.w-1': self.bot(1, 1000), 'p.w-2': self.bot(2, 10), 'p.w-3': self.bot(3, 10)}
        _, act = self.act('p.w-2', state, live)
        for owner, reviewer in (('w-3', 'w-1'), ('w-1', 'w-3')):
            with self.assertRaises(self.s.Refused) as refused:
                self.s.assign(act, 'u', owner, reviewer, 'new work')
            self.assertEqual(refused.exception.code, 'member_exhausted')
        self.s.assign(act, 't', 'w-3', 'w-2', 'take this over')
        self.assertEqual((act.lines[0]['owner'], act.lines[0]['reviewer']), ('w-3', 'w-2'))
        # A submitted result whose reviewer ran out goes to another reviewer.
        state['tasks']['t'].update(owner='w-3', reviewer='w-1', status='reviewing', result='r')
        _, act = self.act('p.w-3', state, live)
        self.s.assign(act, 't', 'w-3', 'w-2', 'please review')
        self.assertTrue(act.lines[0]['reviewing'])
        # Naming another owner never drops the submitted result, and a review
        # that ends wakes the first member that can still take a turn.
        with self.assertRaises(self.s.Refused) as refused:
            self.s.assign(act, 't', 'w-2', 'w-3', 'redo it')
        self.assertEqual(refused.exception.code, 'result_submitted')
        state['tasks']['t'].update(reviewer='w-2')
        _, act = self.act('p.w-2', state, live)
        with self.assertRaises(self.s.Refused) as refused:
            self.s.assign(act, 't', 'w-3', 'w-1', 'please review')
        self.assertEqual(refused.exception.code, 'member_exhausted')
        state['tasks']['t'].update(owner='w-3', reviewer='w-2')
        _, act = self.act('p.w-2', state, live)
        self.s.review(act, 't', 'supported', 'checked')
        self.assertEqual(act.sends, [])
        state['tasks']['t'].update(owner='w-2', reviewer='w-3', status='reviewing')
        _, act = self.act('p.w-3', state, live)
        self.s.review(act, 't', 'supported', 'checked')
        self.assertEqual([m for m, *_ in act.sends], ['p.w-2'])
        # An added agent's brief leaves a member at its cap out of who plans first.
        self.assertEqual(self.s.able(self.swarm, live), ['p.w-2', 'p.w-3'])
        # Nothing is sent to a member at its cap, so a post naming it wakes nobody.
        _, act = self.act(None, state, live)
        self.s.post(act, '@w-1 carry on', False)
        self.assertEqual((act.sends, [m['agent'] for m in act.missed]), ([], ['w-1']))

    def test_the_coordinator_hears_when_every_member_is_deleted(self):
        folder, act = self.act()
        folder.board = open(os.devnull, 'rb')
        self.addCleanup(folder.board.close)
        self.swarm['rows'] = {}
        self.s.budget(act, folder, {}, [])
        self.assertEqual([(l['kind'], l['outcome']) for l in act.lines], [('quiet', 'blocked')])
        self.assertEqual([m for m, *_ in act.sends], ['p.lead'])

    def test_an_identity_needs_shell_only_when_its_profile_lists_tools(self):
        made = tempfile.TemporaryDirectory()
        self.addCleanup(made.cleanup)
        folder = Path(made.name)
        agents = folder / '.agents/agents'
        agents.mkdir(parents=True)
        (agents / 'inline.md').write_text('---\ntools: [read, "edit"]\n---\nReviews.\n')
        (agents / 'block.md').write_text('---\ndescription: x\ntools:\n  - read\n  - shell\nmodel: m\n---\n')
        (agents / 'open.md').write_text('---\ndescription: x\n---\nAnything.\n')
        (agents / 'noted.md').write_text('---\ntools: [read, shell]\t# the defaults\n---\n')
        (agents / 'listed.md').write_text('---\ntools:\n  # needed\n  - shell # for the board\n---\n')
        self.assertEqual(self.s.profile_tools(str(folder), 'noted'), ['read', 'shell'])
        self.assertEqual(self.s.profile_tools(str(folder), 'listed'), ['shell'])
        self.assertEqual(self.s.profile_tools(str(folder), 'inline'), ['read', 'edit'])
        self.assertEqual(self.s.profile_tools(str(folder), 'block'), ['read', 'shell'])
        self.assertIn('shell', self.s.profile_tools(str(folder), 'open'))
        self.assertIn('shell', self.s.profile_tools(str(folder), 'missing'))

    def test_a_member_finds_its_swarm_even_in_a_dotted_project(self):
        made = tempfile.TemporaryDirectory()
        self.addCleanup(made.cleanup)
        # p.widget-2 was a member of p.widget, deleted, and its name became a swarm's.
        for name, members in [('p.widget', ['p.widget-1', 'p.widget-2']), ('p.widget-2', ['p.widget-2-1']),
                              ('foo.bar.widget', ['foo.bar.widget-1'])]:
            (Path(made.name) / name).mkdir()
            (Path(made.name) / name / 'swarm.json').write_text(json.dumps({'members': members}))
        self.addCleanup(os.environ.pop, 'AGENT_BOT', None)
        # The one that names it, else the first there: an added agent is named once made.
        for bot, name in [('p.widget-1', 'p.widget'), ('p.widget-2-1', 'p.widget-2'), ('p.widget-1.fix-2', 'p.widget'),
                          ('foo.bar.widget-1.fix.deep', 'foo.bar.widget'), ('p.widget-3', 'p.widget'),
                          ('p.widget-2-2', 'p.widget-2')]:
            os.environ['AGENT_BOT'] = bot
            self.assertEqual(self.s.which(['status'], made.name), (name, ['status']), bot)

    def test_a_board_cut_mid_line_keeps_its_next_line_whole(self):
        made = tempfile.TemporaryDirectory()
        self.addCleanup(made.cleanup)
        folder = Path(made.name) / 'p.w'
        folder.mkdir()
        (folder / 'swarm.json').write_text(json.dumps(self.swarm))
        (folder / 'board.jsonl').write_bytes(b'{"from":"user","text":"goal"}\n{"from":"w-1","kind":"ro')
        board = self.s.Folder(made.name, 'p.w')
        board.append([{'from': 'w-1', 'kind': 'role', 'role': 'profiler'}])
        board.board.close()
        lines = (folder / 'board.jsonl').read_bytes().split(b'\n')
        self.assertEqual(json.loads(lines[2])['role'], 'profiler')
        (folder / 'state.json').unlink()
        again = self.s.Folder(made.name, 'p.w')
        self.assertEqual(again.state['roles'], {'w-1': 'profiler'})
        # A cache folded again is written back, even by an act that adds nothing.
        again.append([])
        again.board.close()
        self.assertEqual(json.loads((folder / 'state.json').read_text())['roles'], {'w-1': 'profiler'})

    def test_your_post_that_wakes_someone_is_news_after_quiet_even_refolded(self):
        state = self.s.empty_state()
        self.s.apply(state, {'from': 'swarm', 'kind': 'quiet'})
        self.s.apply(state, {'from': 'user', 'text': '@w-9 typo', 'sent': 0})
        self.assertTrue(state['told'])
        self.s.apply(state, {'from': 'user', 'text': 'carry on', 'sent': 3})
        self.assertFalse(state['told'])
        # Joining resets the share of the budget the swarm was told it passed,
        # and a new agent is news after quiet.
        state['spent'] = 80
        self.s.apply(state, {'from': 'swarm', 'kind': 'quiet'})
        self.s.apply(state, {'from': 'swarm', 'kind': 'joined', 'member': 'w-4', 'spent': 50})
        self.assertEqual((state['spent'], state['told']), (50, False))

    def test_the_mix_is_dealt_to_the_row_furthest_below_its_share(self):
        mix = [{'share': 50}, {'share': 25}, {'share': 25}]
        self.assertEqual(self.s.deal(mix, 4), [0, 1, 2, 0])
        self.assertEqual(self.s.deal(mix, 1, [2, 1, 0]), [2])
        self.assertEqual(self.s.goal_name('Make the provider pool faster'), 'provider')
        # A client's own role is not a swarm identity.
        with self.assertRaises(self.s.Refused) as refused:
            self.s.valid_mix([{'model': 'm', 'share': 100, 'identity': 'Coordinator', 'effort': None}])
        self.assertIn('client', refused.exception.detail)


if __name__ == '__main__':
    unittest.main()
