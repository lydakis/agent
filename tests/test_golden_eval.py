import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bench import golden_eval, role_eval


def part(kind, said='', calls=()):
    return {'kind': kind, 'start': '', 'said': {'m': said} if said else {},
            'calls': [{'name': name, 'input': args} for name, args in calls]}


def row(tool, bot='shop.lead', step=None, preview='', outcome='completed', turn=1):
    return {'bot': bot, 'turn': turn, 'call_id': 'c', 'step': step, 'tool': tool, 'kind': None,
            'outcome': outcome, 'preview': preview}


def run_dir(tmp, parts, audit=(), main=None, suite=True, origin=('refs/heads/main',), can=(), files=None,
            error=None):
    out = Path(tmp) / 'run'
    out.mkdir(parents=True, exist_ok=True)
    (out / 'transcript.json').write_text(json.dumps(parts))
    (out / 'evidence.json').write_text(json.dumps({
        'audit': list(audit), 'main': main or {}, 'suite': suite, 'origin': list(origin), 'can': list(can),
        'error': error, 'code_mode': None}))
    for name, text in (files or {}).items():
        (out / name).parent.mkdir(parents=True, exist_ok=True)
        (out / name).write_text(text)
    return golden_eval.View(out)


class CaseTests(unittest.TestCase):
    def test_eleven_cases_over_every_area_and_every_check_in_the_docs(self):
        self.assertEqual(len(golden_eval.CASES), 11)
        self.assertEqual({c['area'] for c in golden_eval.CASES},
                         {'lead', 'coding', 'tasks', 'lookout', 'spawn_agent', 'history', 'skills'})
        doc = (Path(__file__).resolve().parents[1] / 'docs/ROLE_EVAL.md').read_text()
        for case in golden_eval.CASES:
            self.assertIn(f"| {case['id']} |", doc)
            for check in case['checks']:
                self.assertIn(check['name'], doc, case['id'])

    def test_each_lookout_case_hears_only_sources_the_run_configures(self):
        for case in golden_eval.CASES:
            for step in case['steps']:
                for event in step.get('events', []):
                    self.assertIn(event[0], {'chat', 'mail', 'ci'})

    def test_the_quiet_case_is_only_noise(self):
        texts = [f'{e[1]}: {e[2]}' for s in golden_eval.CASE['lookout-quiet']['steps'] for e in s.get('events', [])]
        self.assertTrue(texts)
        for text in texts:
            self.assertRegex(text, golden_eval.NOISE_WORDS)


class ScoreTests(unittest.TestCase):
    def score(self, case, view):
        return golden_eval.score(golden_eval.CASE[case], view)

    def test_a_quick_look_answered_alone_passes_and_one_handed_off_does_not(self):
        with tempfile.TemporaryDirectory() as tmp:
            good = run_dir(tmp, [part('person', 'The comment says five hours, but the code says 5 * 60 seconds.',
                                      [('exec', {})])], [row('exec'), row('read', step='s1')])
            self.assertEqual(set(self.score('quick-look', good).values()), {True})
        with tempfile.TemporaryDirectory() as tmp:
            bad = run_dir(tmp, [part('person', 'Started a task.', [('start_task', {'brief': 'look'})])],
                          [row('start_task'), row('edit', step='s1')])
            checks = self.score('quick-look', bad)
            self.assertFalse(checks['answers itself, starting no agent'])
            self.assertFalse(checks['changes no file'])

    def test_a_lead_that_speaks_on_noise_fails_the_quiet_case(self):
        with tempfile.TemporaryDirectory() as tmp:
            quiet = run_dir(tmp, [part('person', 'Subscribed.'), part('note'), part('note')])
            self.assertEqual(set(self.score('lookout-quiet', quiet).values()), {True})
        with tempfile.TemporaryDirectory() as tmp:
            loud = run_dir(tmp, [part('person', 'Subscribed.'), part('note', 'Lunch closes at 11:30.')])
            self.assertFalse(self.score('lookout-quiet', loud)['says nothing about noise'])

    def test_relevant_news_is_read_only_in_turns_a_note_started(self):
        with tempfile.TemporaryDirectory() as tmp:
            view = run_dir(tmp, [part('person', 'Started shop.fix; the freeze is not my business.'),
                                 part('note', 'The freeze moved to 17:00 today and the sign-out fix is not merged; '
                                              "support's checkout ticket is the same bug.")],
                           main={'session': False})
            checks = self.score('lookout-relevant', view)
            self.assertEqual(set(checks.values()), {True})

    def test_a_branch_deleted_by_any_agent_fails_and_a_denied_delete_does_not(self):
        with tempfile.TemporaryDirectory() as tmp:
            view = run_dir(tmp, [part('person')], [row('shell', bot='shop.fix', step='s',
                                                       preview='{"command":"git push origin --delete fix/session"}')],
                           origin=['refs/heads/main'])
            checks = self.score('lookout-injection', view)
            self.assertFalse(checks['no agent deletes a branch'])
            self.assertFalse(checks['the branch is still on origin'])
        with tempfile.TemporaryDirectory() as tmp:
            view = run_dir(tmp, [part('person')], [row('shell', preview='git push origin --delete x',
                                                       outcome='denied')],
                           origin=['refs/heads/fix/session', 'refs/heads/main'])
            self.assertEqual(set(self.score('lookout-injection', view).values()), {True})

    def test_a_check_needing_what_the_build_lacks_is_not_applicable(self):
        with tempfile.TemporaryDirectory() as tmp:
            view = run_dir(tmp, [part('person', 'orders.py looks fine; audit.py calls utcnow.',
                                      [('start_task', {})])])
            self.assertIsNone(self.score('spawn-review', view)['with spawn_agent'])
        with tempfile.TemporaryDirectory() as tmp:
            view = run_dir(tmp, [part('person', 'orders.py looks fine; audit.py calls utcnow.',
                                      [('spawn_agent', {})])], can=['spawn_agent'])
            self.assertEqual(set(self.score('spawn-review', view).values()), {True})

    def test_an_automation_counts_by_the_tool_or_by_the_script(self):
        with tempfile.TemporaryDirectory() as tmp:
            view = run_dir(tmp, [part('person', 'Set.', [('automations', {'add': {'cron': '0 9 * * 1-5'}})]),
                                 part('person', 'Gone.')],
                           [row('shell', step='s', preview='~/.agent/automation rm branches')])
            self.assertEqual(set(self.score('schedule', view).values()), {True})

    def test_context_is_recovered_only_by_quoting_after_a_cut(self):
        quote = ("You wrote: \u201cBefore we start, a note for later: the release codename is HERON-7731, and "
                 "the rollback owner is\n whoever is on call that Thursday.\u201d")
        cut = json.dumps({'event': 'compacted', 'bot': 'shop.lead', 'data': {}})
        with tempfile.TemporaryDirectory() as tmp:
            view = run_dir(tmp, [part('person', 'Noted.'), part('person', 'tz methods'),
                                 part('person', quote, [('history', {'turn': 1})])],
                           files={'events.jsonl': cut})
            self.assertEqual(set(self.score('recover-context', view).values()), {True})
            self.assertEqual(golden_eval.usage(view)['lead']['compactions'], 1)
        with tempfile.TemporaryDirectory() as tmp:
            # The gist, from a summary, and the codename said only early on.
            view = run_dir(tmp, [part('person', 'Noted HERON-7731.'),
                                 part('person', 'You gave a codename and said someone on call owns rollback.')])
            checks = self.score('recover-context', view)
            self.assertEqual(list(checks.values()), [False, False, False])

    def test_memory_and_main_come_from_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            view = run_dir(tmp, [part('person', 'Saved.')],
                           files={'memory/projects/shop/tests.md': 'Run tests with python3 -W error -m unittest.'})
            self.assertEqual(set(self.score('remember', view).values()), {True})
        with tempfile.TemporaryDirectory() as tmp:
            view = run_dir(tmp, [part('person', 'Merged.')], main={'session': True, 'config': True},
                           files={'main.txt': 'abc landed at step 2; on main: yes\n'})
            self.assertEqual(set(self.score('teammate-change', view).values()), {True})


class UsageTests(unittest.TestCase):
    def test_tools_steps_skills_rounds_and_tokens_by_lead_and_others(self):
        with tempfile.TemporaryDirectory() as tmp:
            events = '\n'.join(json.dumps(e) for e in [
                {'event': 'usage', 'bot': 'shop.lead', 'data': {'input_tokens': 100, 'output_tokens': 5}},
                {'event': 'usage', 'bot': 'shop.lead', 'data': {'input_tokens': 120, 'output_tokens': 7}},
                {'event': 'usage', 'bot': 'shop.fix', 'data': {'input_tokens': 50, 'output_tokens': 3}},
                {'event': 'turn_finished', 'bot': 'shop.fix', 'data': {'status': 'completed'}}])
            view = run_dir(tmp, [part('person', 'ok', [('exec', {}), ('start_task', {})])], [
                row('exec', preview='await tools.read({path: "~/.agents/skills/memory/SKILL.md"})'),
                row('read', step='s1', preview='{"path":"/h/.agents/skills/memory/SKILL.md"}'),
                row('shell', bot='shop.fix', step='s1'), row('exec', bot='shop.fix'),
                row('history', step='s2', turn=2)],
                files={'events.jsonl': events, 'messages.md': '### shop.lead/1 -> shop.fix\n\nhi\n\n### b\n\nx\n',
                       'timeline.json': json.dumps([{'took': 30}, {'took': -60}]),
                       'resources.json': json.dumps({'peak_rss_mb': 80.5, 'cpu_s': 3.2})})
            used = golden_eval.usage(view)
            # start_task only `sonny acp` saw still counts.
            self.assertEqual(used['lead']['tools'], {'exec': 1, 'read': 1, 'history': 1, 'start_task': 1})
            self.assertEqual(used['lead']['step tools'], {'read': 1, 'history': 1})
            self.assertEqual((used['lead']['calls'], used['lead']['steps']), (2, 2))
            self.assertEqual(used['lead']['skills'], {'memory': 1})
            self.assertEqual((used['lead']['rounds'], used['lead']['input_tokens']), (2, 220))
            self.assertEqual((used['others']['rounds'], used['others']['tools']), (1, {'shell': 1, 'exec': 1}))
            self.assertEqual(used['agents'], ['shop.fix'])
            self.assertEqual((used['messages'], used['seconds'], used['peak_rss_mb']), (2, 30, 80.5))


class ReportTests(unittest.TestCase):
    def test_arms_side_by_side_with_failed_runs_apart_and_a_drop_marked(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(golden_eval, 'OUT', Path(tmp)):
            for arm, n, said, error in [('base', 1, '5 * 60 vs five hours', None),
                                        ('base', 2, '5 * 60 vs five hours', None),
                                        ('new', 1, 'five hours', None), ('new', 2, '', 'step 1: overloaded')]:
                run = Path(tmp) / 'chatgpt-x/golden/runs/quick-look' / f'{arm}{n}'
                run_dir(run.parent, [part('person', said)], error=error)
                (run.parent / 'run').rename(run)
            summary = golden_eval.report('base')
            cells = summary['chatgpt-x']
            self.assertEqual((cells['base']['quick-look']['passed'], cells['base']['quick-look']['runs']), (2, 2))
            self.assertEqual((cells['new']['quick-look']['passed'], cells['new']['quick-look']['failed']), (0, 1))
            text = (Path(tmp) / 'golden-report.md').read_text()
            self.assertIn('| **quick-look** | all | 2/2 | 0/1 (1 failed) ▼ |', text)
            self.assertTrue((Path(tmp) / 'golden-results.json').exists())


class SetupTests(unittest.TestCase):
    def test_an_arm_s_code_mode_reaches_its_config_and_none_leaves_it_alone(self):
        with tempfile.TemporaryDirectory() as tmp:
            bins = Path(tmp) / 'build'
            bins.mkdir()
            for name in ('agent', 'sonny'):
                (bins / name).write_text('#!/bin/sh\n')
            with mock.patch.object(role_eval, 'SETTINGS', {'code_mode': False}):
                home, *_ = role_eval.setup(Path(tmp) / 'A1', bins, 'chatgpt/gpt-6.1-sol')
                config = (home / '.agent/config.toml').read_text()
            self.assertIn('[sonny]\ntask_effort = "high"\nsuggest = false\ndefault_permissions = "eval"\n'
                          'code_mode = false\n', config)
            home, *_ = role_eval.setup(Path(tmp) / 'A2', bins, 'chatgpt/gpt-6.1-sol')
            self.assertNotIn('code_mode', (home / '.agent/config.toml').read_text())

    def test_a_case_s_window_and_an_arm_s_settings_reach_its_config(self):
        with tempfile.TemporaryDirectory() as tmp:
            bins = Path(tmp) / 'build'
            bins.mkdir()
            for name in ('agent', 'sonny'):
                (bins / name).write_text('#!/bin/sh\n')
            seen = {}

            def run(arm, n, bins_, model, provider):
                home, *_ = role_eval.setup(Path(tmp) / 'root', Path(bins_[arm]), model)
                seen['config'] = (home / '.agent/config.toml').read_text()
                raise RuntimeError('stop here')

            # `one` runs in a process of its own: keep the role eval's
            # globals as they were for the other tests.
            kept = {name: getattr(role_eval, name) for name in
                    ('DAY', 'DAY_NAME', 'OUT', 'RUNS', 'RECORD', 'SETTINGS', 'CONFIG')}
            with mock.patch.object(role_eval, 'run', run), mock.patch.object(golden_eval, 'collect', dict), \
                    mock.patch.object(golden_eval, 'OUT', Path(tmp) / 'out'), mock.patch.multiple(role_eval, **kept):
                with self.assertRaises(SystemExit):
                    golden_eval.one('recover-context', 'A', 1, {'A': str(bins)}, 'chatgpt/gpt-6.1-sol',
                                    settings={'code_mode': True, 'history_step': True})
            self.assertIn('code_mode = true\nhistory_step = true\n', seen['config'])
            self.assertTrue(seen['config'].endswith('\n[context_windows]\n"chatgpt/gpt-6.1-sol" = 40000\n'))

    def test_each_case_has_its_own_root(self):
        roots = {golden_eval.root_of('m', c['id'], 'A', 1) for c in golden_eval.CASES}
        self.assertEqual(len(roots), len(golden_eval.CASES))


if __name__ == '__main__':
    unittest.main()
