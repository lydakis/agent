"""A summary due as a turn ends is written after that turn, while its last
call's cache is warm, rather than before the bot's next call. The next turn
starts at once, takes the summary in at a boundary once it lands, and waits
for it only when its view cannot go on without one."""
from contextlib import closing
import json
import os
import queue
import sqlite3
import threading
import time
from unittest import skipUnless
from tests.test_runtime import ModelFixture, is_summary
from tests.test_elision import drain
from bench.runtime_client import Client

SETTINGS = {'context_bytes': 4096, 'compact_at': 50}


def opens_with_summary(request):
    return request['input'][0]['content'][0]['text'].startswith('[compaction summary')


@skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class SummaryAfterTurnTests(ModelFixture):
    def start(self, tools='echo', delay=1.0, **settings):
        # Every summary takes `delay`; each request's arrival is timed.
        self.model.timeline, self.model.summary_delay = [], delay
        client = self.client(tools=tools, settings={**SETTINGS, **settings})
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path),
                                               tools=tools.split(','), compaction_instructions='Summarize.'))
        return client

    def turn(self, client, request, prompt, status='completed'):
        turn = client.request('submit', bot='Bob', request_id=str(request), prompt=prompt)['result']['turn']
        ended = client.finished(turn, timeout=15)
        self.assertEqual(ended['data']['status'], status, ended)
        return ended

    def past_threshold(self, client):
        """Two turns that end past the threshold: the second starts a summary."""
        self.turn(client, 0, '0' * 500)
        return self.turn(client, 1, '1' * 500)

    def compacted(self, client):
        events, after = [], 0
        while page := client.request('events', bot='Bob', after=after, limit=256)['result']['events']:
            events += [e for e in page if e['event'] == 'compacted']
            after = page[-1]['cursor']
        return events

    def summaries(self):
        return [(start, end) for summary, start, end in self.model.timeline if summary]

    def wait_for(self, condition, timeout=5):
        deadline = time.monotonic() + timeout
        while not condition():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)

    def test_a_summary_due_as_a_turn_ends_runs_after_it_and_goes_in_while_the_bot_waits(self):
        client = self.start()
        ended = self.past_threshold(client)
        landed = client.summarized('Bob', timeout=10)
        # The turn ended at once; its summary ran after it, billed to it.
        (start, end), = self.summaries()
        self.assertLess(ended['_received_at'], end)
        self.assertEqual((landed['event'], landed['turn']), ('compacted', ended['turn']))
        self.assertEqual(landed['data']['request']['after_turn'], True)
        self.assertEqual(landed['data']['request']['beside'], False)
        self.assertIsNotNone(client.request('inspect', bot='Bob')['result']['compaction'])
        with closing(sqlite3.connect(self.path / 'state.sqlite')) as db:
            billed = db.execute('select id,input_tokens+output_tokens,model_rounds from turns order by id').fetchall()
        self.assertEqual([b[1:] for b in billed], [(110, 1), (220, 2)])
        # The next turn starts from it, with no summary of its own.
        drain(self.model)
        self.turn(client, 2, 'small')
        requests = drain(self.model)
        self.assertEqual(len(requests), 1)
        self.assertTrue(opens_with_summary(requests[0]))

    def test_a_summary_landing_during_the_next_turn_goes_in_at_its_next_boundary(self):
        client = self.start(tools='shell')
        ended = self.past_threshold(client)
        # The next turn's first call is held until the summary has landed.
        self.wait_for(lambda: getattr(self.model, 'summarizing', False))
        gate = threading.Event()
        self.model.request_gates = queue.Queue()
        self.model.request_gates.put(gate)
        drain(self.model)
        turn = client.request('submit', bot='Bob', request_id='2', prompt='long:3x1')['result']['turn']
        self.wait_for(lambda: self.summaries())
        time.sleep(.2)
        gate.set()
        self.assertEqual(client.finished(turn, timeout=15)['data']['status'], 'completed')
        # The view stays past the threshold, so later boundaries may
        # summarize again; the first summary is the one that turn started.
        summary_end = self.summaries()[0][1]
        calls = [start for summary, start, _ in self.model.timeline if not summary]
        # The turn did not wait for it: its first call went out first.
        self.assertLess(calls[2], summary_end)
        work = [r for r in drain(self.model) if not is_summary(r)]
        self.assertFalse(opens_with_summary(work[0]))
        self.assertTrue(all(opens_with_summary(r) for r in work[1:]))
        compacted = self.compacted(client)[0]
        self.assertEqual(compacted['turn'], ended['turn'])
        self.assertTrue(compacted['data']['request']['after_turn'])

    def test_a_turn_whose_view_does_not_fit_waits_for_the_summary_on_its_way(self):
        client = self.start()
        ended = self.past_threshold(client)
        drain(self.model)
        self.turn(client, 2, 'y' * 2600)
        # One summary, the one started as the last turn ended; this turn's
        # call waited for it rather than summarizing again.
        (start, end), = self.summaries()
        calls = [call for summary, call, _ in self.model.timeline if not summary]
        self.assertGreaterEqual(calls[-1], end)
        work = [r for r in drain(self.model) if not is_summary(r)]
        self.assertEqual(len(work), 1)
        self.assertTrue(opens_with_summary(work[0]))
        compacted, = self.compacted(client)
        self.assertEqual(compacted['turn'], ended['turn'])
        self.assertTrue(compacted['data']['request']['after_turn'])

    def test_a_summary_planned_for_another_summarizer_is_dropped(self):
        client = self.start()
        self.past_threshold(client)
        self.wait_for(lambda: getattr(self.model, 'summarizing', False))
        changed = client.request('configure', bot='Bob', compaction_instructions='Summarize again.')
        self.assertIn('result', changed)
        failed = client.summarized('Bob', timeout=10)
        self.assertEqual((failed['event'], failed['error']), ('compaction_failed', 'compaction_stale'))
        self.assertIsNone(client.request('inspect', bot='Bob')['result']['compaction'])
        self.assertEqual(self.compacted(client), [])

    def test_a_summary_started_as_a_turn_ended_is_not_started_twice(self):
        client = self.start(delay=1.5)
        self.past_threshold(client)
        # This turn ends past the threshold too, while that summary runs.
        ended = self.turn(client, 2, 'z' * 300)
        landed = client.summarized('Bob', timeout=10)
        self.assertEqual(landed['event'], 'compacted')
        self.assertLess(landed['turn'], ended['turn'])
        time.sleep(.3)
        self.assertEqual(len(self.summaries()), 1)

    def test_a_daemon_stopping_drops_the_summary_and_the_next_turn_summarizes(self):
        client = self.start(delay=3.0)
        self.past_threshold(client)
        self.wait_for(lambda: getattr(self.model, 'summarizing', False))
        stopped = time.monotonic()
        client.close()
        # The stop did not wait for the summary.
        self.assertLess(time.monotonic() - stopped, 2.5)
        client = Client(self.binary, self.path / 'state.sqlite', self.url, settings=SETTINGS)
        self.addCleanup(client.close)
        self.assertIsNone(client.request('inspect', bot='Bob')['result']['compaction'])
        self.model.summary_delay = 0
        drain(self.model)
        self.turn(client, 2, 'small')
        requests = drain(self.model)
        self.assertTrue(is_summary(requests[0]))
        compacted = self.compacted(client)
        self.assertEqual(len(compacted), 1)
        self.assertNotIn('after_turn', compacted[0]['data']['request'])

    def test_events_a_summary_writes_after_its_turn_was_pruned_are_pruned_later(self):
        # Kept to one turn: the next turn ends, and its retention prunes the
        # turn whose summary still runs; that summary then writes the
        # turn's usage and `compacted` events.
        client = self.start(delay=1.5, keep_turns=1)
        ended = self.past_threshold(client)
        self.turn(client, 2, 'small')
        landed = client.summarized('Bob', timeout=10)
        self.assertEqual(landed['turn'], ended['turn'])
        late = lambda: [e for e in self.events(client) if e.get('turn') == ended['turn']]
        self.assertTrue(late())
        # The next turn's retention finds them.
        self.turn(client, 3, 'small')
        self.assertEqual(late(), [])

    def test_a_summary_on_its_way_holds_its_share_of_the_bot_budget(self):
        # A budget far past the summary's share lets it start as the turn
        # ends; cut below that share while it runs (budgets are fixed at
        # creation, so the test edits the store), the budget leaves the
        # next turn nothing the summary may still spend, so that turn's
        # first call is its last.
        self.model.call_script = [('echo', {'text': 'a'})] * 3
        self.model.timeline, self.model.summary_delay = [], 2.0
        client = self.client(tools='echo', settings={**SETTINGS, 'max_output_tokens': 64})
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path), tools=['echo'],
                                               compaction_instructions='Summarize.', budget_tokens=10**9))
        self.past_threshold(client)
        self.wait_for(lambda: getattr(self.model, 'summarizing', False))
        with closing(sqlite3.connect(self.path / 'state.sqlite')) as db, db:
            db.execute("UPDATE bots SET budget_tokens=10000 WHERE name='Bob'")
        drain(self.model)
        ended = self.turn(client, 2, 'script')
        self.assertEqual(ended['data'].get('limit'), 'budget')
        work = [r for r in drain(self.model) if not is_summary(r)]
        self.assertEqual(len(work), 1)
        self.assertEqual(work[0]['tool_choice'], 'none')
        landed = client.summarized('Bob', timeout=10)
        self.assertEqual(landed['event'], 'compacted')

    def test_a_bot_held_to_fewer_rounds_than_a_summary_takes_summarizes_at_its_next_turn(self):
        client = self.start(delay=0, max_rounds=2)
        self.past_threshold(client)
        time.sleep(.5)
        self.assertEqual(self.summaries(), [])
        self.turn(client, 2, 'small')
        compacted = self.compacted(client)
        self.assertTrue(compacted)
        self.assertNotIn('after_turn', compacted[0]['data']['request'])

    def events(self, client):
        events, after = [], 0
        while page := client.request('events', bot='Bob', after=after, limit=256)['result']['events']:
            events += page
            after = page[-1]['cursor']
        return events
