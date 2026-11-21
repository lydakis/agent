"""A view the provider refuses as past the model's window is summarized and
sent once more, as Pi Durable compacts and asks again; a second refusal ends
the turn on it, and the bot's next turn summarizes before its first call, as
Codex marks the window full. Only the providers' typed signals count."""
import json
import os
import unittest

from bench.runtime_client import Client
from bench.targets import clean_env
from tests.test_runtime import AnthropicModel, ModelFixture, is_summary

class Overflow:
    """What every family's tests share: a history, then a refused view."""

    def drain(self):
        sent = []
        while not self.model.requests.empty():
            sent.append(self.model.requests.get_nowait())
        return sent

    def history(self, client, bot, turns=6, size=2000):
        self.assertIn('result', client.request('create', bot=bot, workspace=str(self.path),
                                               compaction_instructions='Summarize.'))
        for n in range(turns):
            turn = client.request('submit', bot=bot, request_id=str(n),
                                  prompt=f'{n}:' + 'x' * size)['result']['message']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertFalse(any(is_summary(r) for r in self.drain()))

    def events(self, client, bot, turn, name):
        events = client.request('events', bot=bot, after=0, limit=256)['result']['events']
        return [e['data'] for e in events if e['event'] == name and e['turn'] == turn]

    def refused_then_summarized(self, client, bot, form):
        """The first view is refused: one summary, then the call once more,
        on the smaller view, and the turn completes."""
        refusals = [form]
        self.model.context_full = lambda request, *_: refusals.pop() if refusals else None
        turn = client.request('submit', bot=bot, request_id='go', prompt='go')['result']['message']
        end = client.finished(turn, timeout=30)['data']
        self.assertEqual(end['status'], 'completed', end)
        sent = self.drain()
        self.assertEqual([is_summary(r) for r in sent], [False, True, False])
        self.assertLess(len(json.dumps(sent[2])), len(json.dumps(sent[0])))
        retry, = self.events(client, bot, turn, 'retry')
        self.assertEqual((retry['error'], retry['compact'], retry['delay_ms']),
                         ('provider_context_full', True, 0))
        compacted, = self.events(client, bot, turn, 'compacted')
        self.assertEqual(compacted['trigger']['reason'], 'context_full')
        return turn


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class ResponsesContextFullTests(Overflow, ModelFixture):
    def test_a_refused_view_is_summarized_and_sent_once_more(self):
        client = self.client()
        for bot, form in (('Bob', 'http'), ('Ann', 'stream')):
            with self.subTest(form=form):
                self.model.context_full = None
                self.history(client, bot)
                self.refused_then_summarized(client, bot, form)

    def test_a_second_refusal_ends_the_turn_and_the_next_turn_summarizes_first(self):
        client = self.client()
        # Turns small enough that a second summary finds some to take.
        self.history(client, 'Bob', turns=16, size=500)
        self.model.context_full = lambda request, user: 'http'
        turn = client.request('submit', bot='Bob', request_id='go', prompt='go')['result']['message']
        end = client.finished(turn, timeout=30)['data']
        self.assertEqual((end['status'], end['error']), ('failed', 'provider_context_full'), end)
        # Refused, summarized, refused again: once more, never a loop.
        self.assertEqual([is_summary(r) for r in self.drain()], [False, True, False])
        self.assertEqual(len(self.events(client, 'Bob', turn, 'retry')), 1)
        # The window counts as full from that view on: the next turn
        # summarizes before its call, which then fits.
        self.model.context_full = None
        turn = client.request('submit', bot='Bob', request_id='next', prompt='next')['result']['message']
        end = client.finished(turn, timeout=30)['data']
        self.assertEqual(end['status'], 'completed', end)
        sent = self.drain()
        self.assertEqual([is_summary(r) for r in sent], [True, False])
        compacted, = self.events(client, 'Bob', turn, 'compacted')
        self.assertEqual(compacted['trigger']['reason'], 'bytes')

    def test_a_second_refusal_after_a_park_still_ends_the_turn(self):
        client = self.client()
        self.history(client, 'Bob')
        # Refused, summarized, parked on a rate limit, then refused again:
        # the park keeps that the call already went once more.
        calls = []

        def refuse(request, user):
            calls.append(request)
            return None if len(calls) == 2 else 'http'
        self.model.context_full = refuse
        parks = [True]
        self.model.pace = lambda request, user: bool(parks) and parks.pop()
        turn = client.request('submit', bot='Bob', request_id='go', prompt='go')['result']['message']
        end = client.finished(turn, timeout=30)['data']
        self.assertEqual((end['status'], end['error']), ('failed', 'provider_context_full'), end)
        self.assertEqual([is_summary(r) for r in self.drain()], [False, True, False, False])
        self.assertEqual(len(self.events(client, 'Bob', turn, 'turn_paced')), 1)
        self.assertEqual(len(self.events(client, 'Bob', turn, 'compacted')), 1)
        retries = self.events(client, 'Bob', turn, 'retry')
        self.assertEqual([r['error'] for r in retries if r.get('compact')], ['provider_context_full'])

    def test_a_summary_parked_on_a_rate_limit_still_comes_before_the_call_goes_once_more(self):
        # The token threshold off, so only the kept refusal summarizes as
        # the turn resumes; the call is refused twice, so the turn ends on
        # the second refusal after one summary.
        client = self.client(settings={'compact_tokens': 0})
        self.history(client, 'Bob')
        calls = []

        def refuse(request, user):
            calls.append(request)
            return 'http' if len(calls) <= 2 else None
        self.model.context_full = refuse
        self.model.compaction_refusals = 1
        turn = client.request('submit', bot='Bob', request_id='go', prompt='go')['result']['message']
        end = client.finished(turn, timeout=30)['data']
        self.assertEqual((end['status'], end['error']), ('failed', 'provider_context_full'), end)
        self.assertEqual([is_summary(r) for r in self.drain()], [False, True, True, False])
        self.assertEqual(len(self.events(client, 'Bob', turn, 'turn_paced')), 1)
        retries = self.events(client, 'Bob', turn, 'retry')
        self.assertEqual([r['error'] for r in retries if r.get('compact')], ['provider_context_full'])

    def test_a_refusal_waiting_on_its_summary_outlasts_a_start_without_its_provider(self):
        # Parked on the summary's rate limit; a start without the bot's
        # provider holds the turn and parks it again; the next start with
        # it still summarizes first, and the call goes only once more.
        provider = ('--provider', f'changed=responses,{self.url}')
        client = self.client(extra=provider, settings={'compact_tokens': 0})
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path),
                                               model='changed/synthetic-model',
                                               compaction_instructions='Summarize.'))
        for n in range(6):
            turn = client.request('submit', bot='Bob', request_id=str(n),
                                  prompt=f'{n}:' + 'x' * 2000)['result']['message']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.drain()
        calls = []

        def refuse(request, user):
            calls.append(request)
            return 'http' if len(calls) <= 2 else None
        self.model.context_full = refuse
        self.model.compaction_refusals, self.model.compaction_retry_after = 1, '1'
        turn = client.request('submit', bot='Bob', request_id='go', prompt='go')['result']['message']
        client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        client.close(kill=True)
        client = self.client()
        client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn
                       and (e['data'].get('blocked') or {}).get('code') == 'provider_unavailable')
        client.close(kill=True)
        client = self.client(extra=provider)
        end = client.finished(turn, timeout=30)['data']
        self.assertEqual((end['status'], end['error']), ('failed', 'provider_context_full'), end)
        self.assertEqual([is_summary(r) for r in self.drain()], [False, True, True, False])
        retries = self.events(client, 'Bob', turn, 'retry')
        self.assertEqual([r['error'] for r in retries if r.get('compact')], ['provider_context_full'])

    def test_a_bot_with_its_token_threshold_off_still_summarizes_a_refused_view(self):
        # Its byte budget alone would keep the whole view; the summary keeps
        # shares of the view the provider refused instead.
        client = self.client(settings={'compact_tokens': 0})
        self.history(client, 'Bob')
        self.refused_then_summarized(client, 'Bob', 'http')

    def test_a_bot_with_nothing_to_summarize_ends_the_turn_on_the_refusal(self):
        client = self.client()
        self.history(client, 'Bob', turns=0)
        self.model.context_full = lambda request, user: 'http'
        turn = client.request('submit', bot='Bob', request_id='go', prompt='go')['result']['message']
        end = client.finished(turn, timeout=30)['data']
        self.assertEqual((end['status'], end['error']), ('failed', 'provider_context_full'), end)
        self.assertEqual([is_summary(r) for r in self.drain()], [False])
        self.assertEqual(self.events(client, 'Bob', turn, 'compacted'), [])

    def test_another_400_is_not_read_as_a_full_window(self):
        client = self.client()
        self.history(client, 'Bob')
        self.model.refused_prompts = {'go'}
        turn = client.request('submit', bot='Bob', request_id='go', prompt='go')['result']['message']
        end = client.finished(turn, timeout=30)['data']
        self.assertEqual((end['status'], end['error']), ('failed', 'provider_http_400'), end)
        self.assertFalse(any(is_summary(r) for r in self.drain()))


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class AnthropicContextFullTests(Overflow, ModelFixture):
    handler = AnthropicModel

    def client(self, **_):
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell', provider='anthropic', family='anthropic',
                        model='synthetic-claude', key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'})
        self.addCleanup(client.close)
        return client

    def test_a_reply_cut_at_the_window_or_a_request_past_the_size_limit_is_summarized_and_sent_again(self):
        client = self.client()
        for bot, form in (('Bob', 'stop'), ('Ann', '413')):
            with self.subTest(form=form):
                self.model.context_full = None
                self.history(client, bot)
                self.refused_then_summarized(client, bot, form)


if __name__ == '__main__':
    unittest.main()
