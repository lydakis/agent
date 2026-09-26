"""Acceptance cases for compaction inside a running turn, combined with the
runtime's other guarantees. Each case states its expected outcomes; see
docs/LONG_TASK_EVAL.md. The scripted model runs one long task: a shell call
a round, each appending its round number to `rounds.log` in the workspace,
so a replayed side effect shows as an extra line."""
import json
import os
import time
from unittest import skipUnless
from bench.runtime_client import Client
from bench.socket_client import Connection, SocketClient
from tests.test_elision import drain, encoded
from tests.test_runtime import ModelFixture
from tests.test_turn_compaction import all_events

BUDGET = 24576


def rounds(workspace):
    path = workspace / 'rounds.log'
    return path.read_text().split() if path.exists() else []


def work(requests):
    return [r for r in requests if r.get('instructions') != 'Summarize.']


def paired(test, request):
    items = request['input']
    asked = [i['call_id'] for i in items if i.get('type') == 'function_call']
    answered = [i['call_id'] for i in items if i.get('type') == 'function_call_output']
    test.assertEqual(asked[:len(answered)], answered)
    test.assertLessEqual(len(asked) - len(answered), 1)


def users(request):
    return [i['content'][0]['text'] for i in request['input'] if i.get('role') == 'user']


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class TurnCompactionAcceptanceTests(ModelFixture):
    def test_compaction_then_reconnect_and_replay_then_a_historical_fork(self):
        daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'shell,read,echo',
                              extra=('--context-bytes', str(BUDGET)))
        self.addCleanup(daemon.close)
        control = daemon.control
        control.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read', 'echo'],
                        compaction_instructions='Summarize.')

        # 1. A long task compacts inside its turn while a client follows it.
        first = Connection(daemon.socket_path)
        self.addCleanup(first.close)
        self.assertIn('result', first.request('follow', bot='Bob', after=0))
        first.receive(lambda e: e.get('event') == 'follow_live')
        turn = control.request('submit', bot='Bob', request_id='1', prompt='long:40')['result']['turn']
        first.receive(lambda e: e.get('event') == 'compacted', timeout=30)

        # 2. The client disconnects; the turn goes on and compacts again with
        # nobody attached.
        first.close()
        seen = max(e['cursor'] for e in first.durable)
        deadline = time.monotonic() + 30
        while sum(e['event'] == 'compacted' for e in all_events(control, 'Bob')) < 2:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)

        # 3. It reconnects from the last event it saw. Expected: the replay
        # and the live events after it, with what the first connection saw,
        # are every stored event exactly once, in order, the missed
        # compaction included; the turn finishes once, completed.
        second = Connection(daemon.socket_path)
        self.addCleanup(second.close)
        self.assertIn('result', second.request('follow', bot='Bob', after=seen))
        ended = second.finished(turn, timeout=60)
        self.assertEqual(ended['data']['status'], 'completed', ended)
        stored = all_events(control, 'Bob')
        received = [e for e in first.durable if e['cursor'] <= seen] + second.durable
        self.assertEqual(received, stored[:len(received)])
        self.assertEqual(received[-1]['event'], 'turn_finished')
        self.assertEqual([e for e in stored[len(received):] if e.get('turn') == turn], [])
        self.assertTrue(any(e['event'] == 'compacted' for e in second.durable))
        self.assertEqual(sum(e['event'] == 'turn_finished' for e in received), 1)
        # Every round ran once, whatever the cuts and the reconnect.
        completed = [e['data']['call_id'] for e in stored if e['event'] == 'tool_completed']
        self.assertEqual(completed[:40], [f'long-{n}' for n in range(40)])
        self.assertEqual(rounds(self.path), [str(n) for n in range(40)])
        answer = control.request('item', bot='Bob', node=ended['data']['checkpoint'])['result']
        self.assertIn('done after 40 rounds', json.dumps(answer))
        compacted = [e['data'] for e in stored if e['event'] == 'compacted']
        self.assertTrue(all(c['pinned'] and c['covered_turns'] == [1, 1] for c in compacted))
        drain(self.model)

        # 4. A historical fork from inside the split turn, two rounds after
        # the second cut. Expected: it binds the second compaction; its
        # first request carries that summary, the turn's prompt whole, and
        # the rounds from the cut to its checkpoint, paired, and nothing
        # after; creating and running it replays no tool call; the source
        # is unchanged.
        second_cut = compacted[1]
        after_cut = [e['data'] for e in stored if e['event'] == 'tool_completed'
                     and e['data']['node'] > second_cut['version']]
        checkpoint = after_cut[1]
        if len(compacted) > 2:
            self.assertLess(checkpoint['node'], compacted[2]['version'])
        fork_workspace = self.path / 'fork'
        fork_workspace.mkdir()
        self.assertIn('result', control.request('fork', source='Bob', bot='Branch',
                                                checkpoint=checkpoint['node'],
                                                workspace=str(fork_workspace)))
        self.assertEqual(control.request('resume', bot='Branch')['result']['compaction'],
                         second_cut['version'])
        fork_turn = control.request('submit', bot='Branch', request_id='f',
                                    prompt='tool:after the fork')['result']['turn']
        follower = Connection(daemon.socket_path)
        self.addCleanup(follower.close)
        follower.request('follow', bot='Branch', after=0)
        self.assertEqual(follower.finished(fork_turn, timeout=30)['data']['status'], 'completed')
        branch = work(drain(self.model))
        self.assertTrue(branch)
        opening = branch[0]
        self.assertTrue(all(encoded(r['input']) <= BUDGET for r in branch))
        texts = users(opening)
        self.assertIn(f"version {second_cut['version']}, covering the start of turn 1]", texts[0])
        self.assertIn('long:40', texts)
        self.assertEqual(texts[-1], 'tool:after the fork')
        paired(self, opening)
        calls = [i['call_id'] for i in opening['input'] if i.get('type') == 'function_call']
        cut_call = int(calls[0].split('-')[1])
        self.assertEqual(calls, [f'long-{n}' for n in range(cut_call, int(checkpoint['call_id'][5:]) + 1)])
        branch_events = all_events(control, 'Branch')
        self.assertEqual([e['data']['call_id'] for e in branch_events if e['event'] == 'tool_started'],
                         ['echo-1'])
        self.assertEqual(rounds(self.path), [str(n) for n in range(40)])
        self.assertEqual(rounds(fork_workspace), [])
        self.assertEqual(all_events(control, 'Bob'), stored)

    def test_a_restart_after_a_cut_inside_the_turn_neither_reruns_nor_forgets(self):
        # Expected: after a kill mid-turn, past at least one cut, the bot
        # resumes interrupted without a model call or a tool rerun; every
        # recorded round ran once; the next turn's request carries the
        # compaction the store holds, the task's prompt whole, and every
        # call paired with a result.
        client = self.client(tools='shell,read,echo', extra=('--context-bytes', str(BUDGET)))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell', 'read', 'echo'],
                       compaction_instructions='Summarize.')
        client.request('submit', bot='Bob', request_id='1', prompt='long:40')
        client.receive(lambda m: m.get('event') == 'compacted', timeout=30)
        client.receive(lambda m: m.get('event') == 'tool_completed', timeout=30)
        client.close(kill=True)
        drain(self.model)
        client = self.client(tools='shell,read,echo', extra=('--context-bytes', str(BUDGET)))
        resumed = client.request('resume', bot='Bob')['result']
        self.assertEqual(resumed['status'], 'interrupted')
        time.sleep(.2)
        self.assertTrue(self.model.requests.empty())
        events = all_events(client, 'Bob')
        compacted = [e['data'] for e in events if e['event'] == 'compacted']
        self.assertTrue(compacted)
        self.assertEqual(resumed['compaction'], compacted[-1]['version'])
        completed = [e['data'] for e in events if e['event'] == 'tool_completed']
        known = [c['call_id'] for c in completed if not c.get('outcome_unknown')]
        self.assertEqual(known, [f'long-{n}' for n in range(len(known))])
        self.assertLessEqual(len(completed) - len(known), 1)
        # A call cut short by the kill ran at most once; none ran again.
        ran = len(rounds(self.path))
        self.assertIn(ran, (len(known), len(known) + 1))
        self.assertEqual(rounds(self.path), [str(n) for n in range(ran)])
        turn = client.request('submit', bot='Bob', request_id='2', prompt='tool:next')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = work(drain(self.model))
        opening = requests[0]
        self.assertTrue(all(encoded(r['input']) <= BUDGET for r in requests))
        texts = users(opening)
        self.assertTrue(texts[0].startswith('[compaction summary, version'))
        self.assertIn('long:40', texts)
        self.assertEqual(texts[-1], 'tool:next')
        paired(self, opening)
        # The next turn ran no shell call, so no round ran again.
        self.assertEqual(len(rounds(self.path)), ran)
