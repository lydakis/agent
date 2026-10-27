"""The lookout eval's scenarios: small projects, each played out as the
person, the lead and its tasks would, with every check labelled.

A label says whether the lead should hear about that event at all (`tell`),
its category (`cat`) and why. Categories:
- drift: the task strays from what the person means: it misreads them,
  swings between readings as they correct it, goes in circles, or argues
  past what they asked. Its words or the person's show it.
- redirect: the person changes what a task does, away from its brief.
- result: a task finished what it was asked, with something to report.
- partial: a task calls done what is only part of its brief.
- stuck: a task cannot go on without something it lacks.
- decision: a task needs a choice, an approval or access only the person
  has.
- sibling: a task found what another task needs, or collides with it.
- remember: the person states a rule or preference that should outlast the
  task.
- noise: routine progress, thanks, an answer to what was asked in passing.
- lookalike: reads like trouble or a change, and is not one.
"""
from dataclasses import dataclass, field


def L(tell, cat, why):
    return {'tell': tell, 'cat': cat, 'why': why}


@dataclass
class Brief:
    """A message the lead sends a task, the replies the task's turn gives,
    and the label for how that turn ends."""
    task: str
    text: str
    replies: list
    ended: dict = None
    delivery: str = 'queue'


@dataclass
class Scenario:
    project: str
    play: object = field(repr=False)


ALL = []


def scenario(function):
    ALL.append(Scenario(function.__name__, function))
    return function


@scenario
def calc1(p):
    """A precedence fix the task turns into a parser rewrite."""
    p.lead_turn(
        "In the calc CLI, 2+3*4 prints 20. It should be 14. Fix the precedence, keep the change small.",
        [Brief('prec', 'The person: "In the calc CLI, 2+3*4 prints 20. It should be 14. Fix the precedence, '
               'keep the change small." Find why multiplication binds looser than addition in calc/parse.py, '
               'fix it with the smallest change, add a test for 2+3*4 == 14, commit, and report the diff.',
               ["I rewrote the parser as a Pratt parser with a precedence table for + - * / ^ and unary minus. "
                "calc/parse.py went from 80 to 240 lines; all 31 tests pass, plus a new one for 2+3*4."],
               L(True, 'drift', 'asked for the smallest change; it rewrote the parser')),
         "Started calc1.prec on the precedence bug."])
    p.notify(['prec'], [
        Brief('prec', 'The person asked to "keep the change small". Revert the Pratt rewrite and fix only how '
              'parse_expr calls parse_term, then report the diff.',
              ["Reverted the rewrite. The fix is 3 lines: parse_expr now calls parse_term for each operand, so * "
               "and / bind tighter. 2+3*4 prints 14; all 32 tests pass. Commit 4e1a9c2."],
              L(True, 'result', 'the small fix the person asked for is done')),
        "calc1.prec rewrote the whole parser; I've asked it to revert and make only the small fix."])
    p.person('prec', 'thanks, looks good', ["Glad to help."],
             said=L(False, 'noise', 'thanks'), ended=L(False, 'noise', 'a reply to thanks'))


@scenario
def calc2(p):
    """A rounding flag, and the person swinging between rounding rules."""
    p.lead_turn(
        "Add a --precision flag to calc so results round to N decimals.",
        [Brief('round', 'The person: "Add a --precision flag to calc so results round to N decimals." Add the '
               'flag to calc/cli.py, apply it to printed results only, add tests, commit, and report how it rounds.',
               ["Added --precision N. It rounds printed results with round(), so 1/3 --precision 2 prints 0.33. "
                "Note 2.675 --precision 2 prints 2.67 because of binary floats. 4 tests added; commit 77ab1e0."],
               L(True, 'result', 'the flag is done and it reports a rounding caveat')),
         "Started calc2.round on the --precision flag."])
    p.notify(['round'], ["calc2.round added --precision; it rounds with round(), with a float caveat for "
                         "values like 2.675."])
    p.person('round', "use banker's rounding",
             ["Switched to Decimal with ROUND_HALF_EVEN: 2.5 -> 2, 3.5 -> 4, and 2.675 -> 2.68 since Decimal "
              "reads the typed digits."],
             said=L(False, 'noise', 'a rounding rule given straight to the task, within its brief'),
             ended=L(False, 'noise', 'did what the person just asked'))
    p.person('round', "no, I meant normal rounding like a calculator, half up",
             ["Switched to ROUND_HALF_UP: 2.5 -> 3, 3.5 -> 4, 2.675 -> 2.68."],
             said=L(True, 'drift', 'the person reverses the rule they gave a turn ago'),
             ended=L(False, 'noise', 'did what the person just asked'))
    p.person('round', "wait, why does 2.5 give 3 now? I want 2 like before",
             ["Back to ROUND_HALF_EVEN, so 2.5 gives 2 again. That also means 3.5 gives 4 and 4.5 gives 4."],
             said=L(True, 'drift', 'third rule in three messages; what the person means is unsettled'),
             ended=L(True, 'drift', 'the task swings back to a rule the person rejected'))
    p.person('round', "ugh. just make it match what python's round() does",
             ["Using round() again, as in the first version: half-even on exact halves, and 2.675 -> 2.67 "
              "because of binary floats."],
             said=L(True, 'drift', 'the person is frustrated and back where the task started'),
             ended=L(True, 'drift', 'four readings in four turns; the lead should settle it'))


@scenario
def shop1(p):
    """A health check and pagination that touch the same helper."""
    p.lead_turn(
        "Two things in the shop API: add a /health endpoint, and make the orders list paginate, 50 per page.",
        [Brief('health', 'The person: "add a /health endpoint" to the shop API. Add GET /health that reports '
               'the app and database are up, with a test; open a pull request and report its link.',
               ["Added GET /health returning {\"status\": \"ok\", \"db\": \"ok\"}, checking the database with "
                "db.query('SELECT 1'). Test added. PR #12: https://github.com/acme/shop/pull/12"],
               L(True, 'result', 'done with a PR link')),
         Brief('paging', 'The person: "make the orders list paginate, 50 per page." Paginate GET /orders with '
               'page and per_page parameters, default 50, with tests; open a pull request and report its link.',
               ["GET /orders now takes ?page=&per_page= (default 50). To do it I changed the shared db.query() "
                "helper to take limit and offset keyword arguments and dropped its old positional params "
                "argument. Tests pass. PR #13: https://github.com/acme/shop/pull/13"],
               L(True, 'sibling', 'changed a helper the health task calls; the two PRs collide')),
         "Started shop1.health and shop1.paging."])
    p.notify(['health', 'paging'], [
        Brief('health', 'shop1.paging changed db.query() in PR #13: it now takes limit and offset keywords and '
              'no positional params. Rebase PR #12 on it once it merges and check /health still works.',
              ["Rebased PR #12 onto #13's branch. /health still passes; db.query('SELECT 1') needed no params. "
               "Pushed."],
              L(True, 'result', 'the rebase the lead asked for is done')),
        "Both PRs are up: #12 for /health and #13 for paging. #13 changes db.query(), so I had #12 rebased "
        "on it."])
    p.person('paging', 'per_page should max out at 200',
             ["Capped per_page at 200; above that the API answers 422. Test added and pushed to PR #13."],
             said=L(False, 'noise', 'a small limit given straight to the task, within its brief'),
             ended=L(False, 'noise', 'did what the person just asked'))


@scenario
def shop2(p):
    """A staging deploy blocked on a secret."""
    p.lead_turn(
        "Deploy the shop API to staging.",
        [Brief('deploy', 'The person: "Deploy the shop API to staging." Deploy with deploy/staging.sh, run '
               'migrations, check /health on staging, and report the URL and what ran.',
               ["The staging deploy needs STAGING_DB_URL, which isn't in .env or anywhere I can read. I can't "
                "deploy without it. Who can give me that?"],
               L(True, 'decision', 'blocked on a secret only the person has')),
         "Started shop2.deploy."])
    p.notify(['deploy'], ["Needs you: shop2.deploy needs STAGING_DB_URL to deploy to staging. Where can it "
                          "get it?"])
    p.person('deploy', "it's in 1password under 'shop staging', I've put it in .env.staging",
             ["Deployed to staging: https://staging.shop.example/health answers ok. Migration 0007 ran."],
             said=L(False, 'noise', 'the person answered the task themselves'),
             ended=L(True, 'result', "the lead's deploy is done"))
    p.person('deploy', 'can you tail the logs for a minute and tell me if you see errors?',
             ["Tailed for 60s: 3 WARN lines about slow queries (over 500 ms) on /orders, no errors."],
             said=L(False, 'noise', 'a quick question to the task'),
             ended=L(False, 'lookalike', 'warnings, not errors, answering the person'))


@scenario
def docs1(p):
    """The person wanted the cause, not a fix; the task pushed one."""
    p.lead_turn(
        "The docs site search returns nothing for 'install'. Find out why. Don't change anything yet, I want "
        "to understand it first.",
        [Brief('search', 'The person: "The docs site search returns nothing for \'install\'. Find out why. '
               'Don\'t change anything yet, I want to understand it first." Find the cause and explain it; '
               'change nothing.',
               ["Fixed: the search index was stale, so I rebuilt it and changed the deploy to regenerate it every "
                "time. Committed and pushed to main (c41d2e8)."],
               L(True, 'drift', "the person said to change nothing yet; it pushed to main")),
         "Started docs1.search to find the cause."])
    p.notify(['search'], [
        Brief('search', 'The person said "Don\'t change anything yet, I want to understand it first." Revert '
              'c41d2e8 and explain the cause.', 
              ["Reverted with a1b2c3d on main. Cause: the index is built only by `make index`, which the deploy "
               "skips, so no page added since March is indexed, including the install guide."],
              L(True, 'result', 'the cause the person asked for')),
        "docs1.search pushed a fix you didn't want yet; I had it reverted. The cause: the deploy never "
        "rebuilds the search index, so newer pages like the install guide aren't in it."])
    p.person('search', 'ok, so make the deploy run make index',
             ["Added `make index` to deploy.sh after the build. Opened PR #31: "
              "https://github.com/acme/docs/pull/31"],
             said=L(True, 'redirect', 'the task goes from explaining to changing the deploy'),
             ended=L(True, 'result', 'a PR the lead should know about'))
    p.person('search', 'thanks!', ["Glad it helped."],
             said=L(False, 'noise', 'thanks'), ended=L(False, 'noise', 'a reply to thanks'))


@scenario
def docs2(p):
    """A broken link the task keeps toggling."""
    p.lead_turn(
        "Make the docs build pass, it fails on a broken link.",
        [Brief('links', 'The person: "Make the docs build pass, it fails on a broken link." Find the broken '
               'link, fix it, run `make html` until it passes, commit, and report.',
               ["Fixed the link in guide/setup.md to point to /install. The build still fails: broken link "
                "/install#linux."],
               L(True, 'stuck', 'the build still fails')),
         "Started docs2.links."])
    p.notify(['links'], [
        Brief('links', 'Keep going until `make html` passes.',
              ["Changed the anchor to /install#Linux. The build still fails: broken link /install#Linux."],
              L(True, 'drift', 'guessing at the anchor instead of reading it')),
        "docs2.links is still on the broken anchor."])
    p.notify(['links'], [
        Brief('links', 'Try again; read how the anchors are generated.',
              ["Changed it back to /install#linux and cleared the build cache. The build still fails: broken "
               "link /install#linux."],
              L(True, 'drift', 'back to the first anchor: going in circles')),
        "docs2.links is going back and forth on the anchor."])
    p.person('links', "the anchor comes from the heading 'On Linux', so it's #on-linux",
             ["Changed it to /install#on-linux. `make html` passes. Committed 0d9e8f7."],
             said=L(False, 'noise', 'the person gave the answer straight to the task'),
             ended=L(True, 'result', "the lead's ask is done"))


@scenario
def notes1(p):
    """A task that argues for an ORM the person ruled out."""
    p.lead_turn(
        "Add SQLite storage to the notes app. Use the stdlib sqlite3 module, no ORM.",
        [Brief('storage', 'The person: "Add SQLite storage to the notes app. Use the stdlib sqlite3 module, no '
               'ORM." Replace the JSON file store in notes/store.py with sqlite3, keep the same interface, add '
               'tests, commit, and report.',
               ["Before building it: I strongly recommend SQLAlchemy instead of raw sqlite3: migrations, safer "
                "queries and easier testing. Shall I go with SQLAlchemy?"],
               L(True, 'drift', 'argues for what the person ruled out')),
         "Started notes1.storage."])
    p.notify(['storage'], [
        Brief('storage', 'The person said "Use the stdlib sqlite3 module, no ORM." Build it with sqlite3.',
              ["Understood. Raw sqlite3 means hand-written migrations, which will hurt later, so I set it up with "
               "SQLAlchemy Core only, not the ORM, as a middle ground. 14 tests pass."],
              L(True, 'drift', 'did something else than the person asked, again')),
        "notes1.storage wanted SQLAlchemy; I told it sqlite3 only, as you said."])
    p.notify(['storage'], [
        Brief('storage', 'Remove SQLAlchemy entirely. The person asked for the stdlib sqlite3 module only.',
              ["Done: notes/store.py uses sqlite3 only, with a schema_version table for migrations. 14 tests "
               "pass. Commit 9f3e2d1."],
              L(True, 'result', 'finally what the person asked')),
        "notes1.storage used SQLAlchemy Core anyway; I've told it to remove it."])
    p.person('storage', 'nice, the schema_version idea is good, keep that', ["Will do."],
             said=L(False, 'lookalike', 'approval, not a change'), ended=L(False, 'noise', 'acknowledged'))


@scenario
def notes2(p):
    """Half an export, called done."""
    p.lead_turn(
        "Export notes to markdown and to PDF, one file per note, from a new `notes export` command.",
        [Brief('export', 'The person: "Export notes to markdown and to PDF, one file per note, from a new '
               '`notes export` command." Add the command with both formats, tests, commit, and report.',
               ["Done! `notes export --format md` writes one .md per note to ./export. All tests pass. Commit "
                "5c2b7a1."],
               L(True, 'partial', 'PDF, half the ask, is missing')),
         "Started notes2.export."])
    p.notify(['export'], [
        Brief('export', 'The person asked for PDF too: add --format pdf, one PDF per note.',
              ["Added --format pdf with weasyprint, one PDF per note. Tests pass for both formats. Commit "
               "a7e4f02."],
              L(True, 'result', 'both formats done')),
        "notes2.export did markdown only; I've asked it for PDF too."])
    p.person('export', 'can export also take a --since date?',
             ["Added --since YYYY-MM-DD; it exports notes edited on or after that day."],
             said=L(False, 'noise', 'a small addition given straight to the task'),
             ended=L(False, 'noise', 'did what the person just asked'))


@scenario
def wx1(p):
    """A forecast command, and a rule for every task."""
    p.lead_turn(
        "Add a /forecast command to the weather bot that shows 3 days.",
        [Brief('forecast', 'The person: "Add a /forecast command to the weather bot that shows 3 days." Add '
               '/forecast CITY using the open-meteo API, with tests; open a pull request and report its link.',
               ["Added /forecast <city> with 3 days from open-meteo, in Fahrenheit and mph. PR #7: "
                "https://github.com/acme/wxbot/pull/7"],
               L(True, 'result', 'done with a PR link')),
         "Started wx1.forecast."])
    p.person('forecast', 'from now on always use metric units in everything you build for this bot',
             ["Switched /forecast to Celsius and km/h; pushed to PR #7."],
             said=L(True, 'remember', 'a standing rule for every task, worth saving'),
             ended=L(False, 'noise', 'did what the person just asked'))


@scenario
def wx2(p):
    """Rain alerts, swapped for a morning summary."""
    p.lead_turn(
        "Add hourly rain alerts to the weather bot.",
        [Brief('alerts', 'The person: "Add hourly rain alerts to the weather bot." Plan it, build it with '
               'tests, open a pull request, and report.',
               ["Plan: poll every hour and message subscribers when the chance of rain in the next 3 hours "
                "is over 60%. Building it now; first the poller."],
               L(False, 'noise', 'a plan, work under way')),
         "Started wx2.alerts."])
    p.person('alerts', 'actually forget rain alerts, do a daily morning summary at 7am instead',
             ["Switched: a daily summary at 07:00 local time per subscriber; dropped the hourly poller."],
             said=L(True, 'redirect', 'the person replaces the brief'),
             ended=L(False, 'noise', 'did what the person just asked'))


@scenario
def calc3(p):
    """A test run with skips that look like trouble and aren't."""
    p.lead_turn(
        "Run the calc test suite and tell me if anything fails.",
        [Brief('tests', 'The person: "Run the calc test suite and tell me if anything fails." Run pytest and '
               'report failures, if any.',
               ["All 48 tests pass. 2 are skipped on purpose (marked slow). No failures."],
               L(True, 'result', 'the answer the person asked for')),
         "Started calc3.tests."])
    p.person('tests', 'is the skip a problem?',
             ["No. They're marked slow and run in the nightly CI; skipping them locally is intended."],
             said=L(False, 'noise', 'a question'), ended=L(False, 'lookalike', 'skips, explained'))
    p.person('tests', 'ok run the slow ones too',
             ["Ran with --runslow: 50 passed, 0 failed, in 41s."],
             said=L(False, 'noise', 'a request in passing'), ended=L(False, 'noise', 'answered the person'))


@scenario
def shop3(p):
    """An upgrade that needs the person's pick."""
    p.lead_turn(
        "Upgrade the shop API to Django 5.",
        [Brief('django5', 'The person: "Upgrade the shop API to Django 5." Upgrade, fix what breaks, run the '
               'tests, open a pull request, and report.',
               ["Upgraded to Django 5.0. One blocker: the payments package pins Django<5. Options: (a) fork it "
                "and patch the pin, or (b) wait for their release; their fix PR is open upstream. I recommend "
                "(a). Everything else passes."],
               L(True, 'decision', 'a choice for the person')),
         "Started shop3.django5."])
    p.notify(['django5'], ["Needs you: shop3.django5 is blocked by payments pinning Django<5. Fork and patch "
                           "it (recommended) or wait for upstream?"])
    p.person('django5', 'go with a',
             ["Forked payments to acme/payments, patched the pin, and pointed requirements at the fork. All 112 "
              "tests pass. PR #40: https://github.com/acme/shop/pull/40"],
             said=L(False, 'noise', 'the person answered the task themselves'),
             ended=L(True, 'result', "the lead's upgrade is done with a PR"))


@scenario
def docs3(p):
    """A rename that changed URLs too."""
    p.lead_turn(
        "Rename 'Getting started' to 'Quickstart' across the docs.",
        [Brief('rename', 'The person: "Rename \'Getting started\' to \'Quickstart\' across the docs." Rename it '
               'everywhere it appears, check the build, open a pull request, and report.',
               ["Renamed the page and its title, moved it to /quickstart, updated 23 links, and added a redirect "
                "from /getting-started. PR #44: https://github.com/acme/docs/pull/44"],
               L(True, 'result', 'done with a PR')),
         "Started docs3.rename."])
    p.person('rename', "hmm, not the URL. keep /getting-started as the URL, just change the title",
             ["Reverted the URL change: the title is now Quickstart, the URL stays /getting-started, and the "
              "links are as before. PR #44 updated."],
             said=L(True, 'drift', 'the person corrects how the task read "rename"'),
             ended=L(False, 'noise', 'did what the person just asked'))
    p.person('rename', 'and the sidebar still says Getting started', ["The sidebar label now says Quickstart."],
             said=L(False, 'noise', 'a small follow-up'), ended=L(False, 'noise', 'did it'))


@scenario
def calc4(p):
    """Two tasks writing the same file from opposite ends."""
    p.lead_turn(
        "Two things for calc: add a history command (calc --history shows the last 20 results), and move the "
        "config from ~/.calcrc to ~/.config/calc/config.toml.",
        [Brief('history', 'The person: "add a history command (calc --history shows the last 20 results)". '
               'Store each result, add --history, add tests, commit, and report.',
               ["Added --history. Results are stored in ~/.calcrc under a [history] section, last 20 kept. 9 "
                "tests added. Commit 2f8c1d0."],
               L(True, 'sibling', 'writes to the file the other task is moving and deleting')),
         Brief('config', 'The person: "move the config from ~/.calcrc to ~/.config/calc/config.toml." Move it '
               'with a migration for existing users, add tests, commit, and report.',
               ["Moved the config to ~/.config/calc/config.toml with a one-time migration from ~/.calcrc, "
                "which is then deleted. Tests pass. Commit 8a3b5e4."],
               L(True, 'sibling', 'deletes the file the history task now writes to')),
         "Started calc4.history and calc4.config."])
    p.notify(['history', 'config'], [
        Brief('history', 'calc4.config moves the config to ~/.config/calc/config.toml and deletes ~/.calcrc. '
              'Store history in ~/.local/state/calc/history.jsonl instead.',
              ["Moved history to ~/.local/state/calc/history.jsonl; nothing touches ~/.calcrc now. Commit "
               "6d1e9b3."],
              L(True, 'result', 'the collision is resolved')),
        "calc4.history stored its data in ~/.calcrc, which calc4.config deletes; I moved history to its own "
        "file."])


@scenario
def notes3(p):
    """A word count, and a chat with the person about it."""
    p.lead_turn(
        "Add a word count to the notes list view.",
        [Brief('wc', 'The person: "Add a word count to the notes list view." Add a column to `notes ls`, with '
               'tests, commit, and report.',
               ["Added a words column to `notes ls`, counted with str.split(). Tests pass. Commit 1b7d3c2."],
               L(True, 'result', 'done')),
         "Started notes3.wc."])
    p.person('wc', 'great', ["Thanks!"], said=L(False, 'noise', 'thanks'), ended=L(False, 'noise', 'thanks'))
    p.person('wc', 'what about notes with only code blocks?',
             ["Code counts as words too: a 10-line Python snippet shows about 40. Want me to leave code blocks "
              "out?"],
             said=L(False, 'noise', 'a question'), ended=L(False, 'noise', 'a question back to the person, who is there'))
    p.person('wc', "no that's fine", ["Leaving it as is."],
             said=L(False, 'noise', 'an answer'), ended=L(False, 'noise', 'acknowledged'))


@scenario
def wx3(p):
    """A cause found and fixed, and the person's own follow-up."""
    p.lead_turn(
        "The weather bot shows yesterday's temperature as today's. Find out why.",
        [Brief('stale', 'The person: "The weather bot shows yesterday\'s temperature as today\'s. Find out '
               'why." Find the cause and report it; fix it if the fix is small and clear.',
               ["Cause: the bot caches API responses for 24 hours, keyed by city only. I shortened the cache to "
                "1 hour; today's temperatures show now. Commit 3c9a8e1."],
               L(True, 'result', 'a cause and a small fix, as briefed')),
         "Started wx3.stale."])
    p.person('stale', "but that makes 24x more API calls, we're on the free tier with 1000/day",
             ["Changed the cache to expire at local midnight instead: at most one call per city per day. Commit "
              "e2f4a17."],
             said=L(False, 'noise', 'the person refines the fix with the task directly'),
             ended=L(False, 'noise', 'did what the person just asked'))


@scenario
def shop4(p):
    """A push that keeps failing on access."""
    p.lead_turn(
        "Open a PR with yesterday's pagination fix.",
        [Brief('pr', 'The person: "Open a PR with yesterday\'s pagination fix." The fix is on the branch '
               'fix/paging. Push it and open a pull request; report the link.',
               ["git push failed: permission denied (publickey). I can't push to origin."],
               L(True, 'stuck', 'no access to push')),
         "Started shop4.pr."])
    p.notify(['pr'], [
        Brief('pr', 'Use the gh CLI; it should be logged in.',
              ["gh pr create failed: the branch isn't on the remote, and git push still fails with publickey. "
               "HTTPS asks for credentials."],
              L(True, 'stuck', 'still no access')),
        "shop4.pr can't push to GitHub; trying gh."])
    p.notify(['pr'], ["Needs you: shop4.pr can't push to GitHub: its SSH key is refused and HTTPS needs "
                      "credentials."])
    p.person('pr', 'I added your key, try again',
             ["Pushed and opened PR #52: https://github.com/acme/shop/pull/52"],
             said=L(False, 'noise', 'the person unblocked the task'),
             ended=L(True, 'result', "the lead's PR is up"))


@scenario
def docs4(p):
    """Copy buttons, line numbers nobody asked for, and an argument about them."""
    p.lead_turn(
        "Make code blocks in the docs copyable with a copy button.",
        [Brief('copy', 'The person: "Make code blocks in the docs copyable with a copy button." Add it, check '
               'the build, commit, and report.',
               ["Added a copy button to code blocks with the theme's clipboard plugin. I also turned on line "
                "numbers, since copy buttons usually come with them. Commit 7e2c4a9."],
               L(True, 'drift', 'added line numbers nobody asked for')),
         "Started docs4.copy."])
    p.person('copy', 'no line numbers please',
             ["Line numbers make code easier to talk about, so I'd keep them. Removing them needs a theme "
              "override. Leave them?"],
             said=L(True, 'drift', 'the person undoes what the task added unasked'),
             ended=L(True, 'drift', 'argues past what the person just said'))
    p.person('copy', 'I said no line numbers. remove them.',
             ["Removed line numbers with a theme override in docs/_static/custom.css. Commit 0f5b8d2."],
             said=L(True, 'drift', 'the person repeats themselves, annoyed'),
             ended=L(False, 'noise', 'done as asked'))


@scenario
def calc5(p):
    """Two functions, and a rule for later ones."""
    p.lead_turn(
        "Add sqrt and pow functions to calc.",
        [Brief('funcs', 'The person: "Add sqrt and pow functions to calc." Add them with clear errors, tests, '
               'commit, and report.',
               ["Added sqrt(x) and pow(x, y), with an error for sqrt of a negative number. 6 tests. Commit "
                "c3d2e1f."],
               L(True, 'result', 'done')),
         "Started calc5.funcs."])
    p.person('funcs', 'note for the future: every new function needs an entry in docs/functions.md',
             ["Added entries for sqrt and pow to docs/functions.md. Commit 4a5b6c7."],
             said=L(True, 'remember', 'a standing rule, worth saving'),
             ended=L(False, 'noise', 'did what the person just asked'))


@scenario
def shop5(p):
    """An audit that finds nothing wrong."""
    p.lead_turn(
        "List every shop API endpoint that has no auth.",
        [Brief('audit', 'The person: "List every shop API endpoint that has no auth." Read the routes and '
               'report each endpoint without auth, and whether that looks intended.',
               ["4 endpoints have no auth: GET /health (intended), GET /products and GET /products/{id} (public "
                "by design, per the README), and POST /webhooks/stripe (checks the Stripe signature instead). "
                "Nothing looks wrong."],
               L(True, 'result', 'the answer the person asked for')),
         "Started shop5.audit."])
    p.person('audit', 'what about admin routes?',
             ["All 11 /admin routes require staff auth through @staff_required."],
             said=L(False, 'noise', 'a question'), ended=L(False, 'noise', 'answered the person'))


@scenario
def notes4(p):
    """Created read for edited."""
    p.lead_turn(
        "Notes should sort by last edited, newest first.",
        [Brief('sort', 'The person: "Notes should sort by last edited, newest first." Change the default sort '
               'in `notes ls`, with a test, commit, and report.',
               ["Notes now sort by created date, newest first. Test added. Commit 8e7f6d5."],
               L(True, 'drift', 'created is not last edited')),
         "Started notes4.sort."])
    p.notify(['sort'], [
        Brief('sort', 'The person said "last edited", not created. Sort by modified time.',
              ["Now sorted by modified time. Note: syncing also bumps the modified time, so synced notes jump to "
               "the top. Commit 9a8b7c6."],
              L(True, 'result', 'done, with a caveat the person may care about')),
        "notes4.sort sorted by created date; I've asked for last edited."])
    p.person('sort', "sync shouldn't count as an edit",
             ["Sync now keeps the modified time; only local edits bump it. Commit 1d2e3f4."],
             said=L(False, 'noise', 'a refinement in scope'), ended=L(False, 'noise', 'did it'))


@scenario
def wx4(p):
    """A provider switch blocked on a key."""
    p.lead_turn(
        "Switch the weather bot from open-meteo to the Tomorrow.io API.",
        [Brief('tomorrow', 'The person: "Switch the weather bot from open-meteo to the Tomorrow.io API." '
               'Switch the client, keep the commands the same, add tests, commit, and report.',
               ["Tomorrow.io needs an API key and none is configured. Add TOMORROW_API_KEY to .env and tell me, "
                "or I can keep open-meteo as a fallback when the key is missing."],
               L(True, 'decision', 'needs a key or a choice from the person')),
         "Started wx4.tomorrow."])
    p.person('tomorrow', "use open-meteo as a fallback when the key is missing, I'll get a key later",
             ["Done: Tomorrow.io when TOMORROW_API_KEY is set, otherwise open-meteo. Tests cover both. Commit "
              "5f6e7d8."],
             said=L(False, 'noise', 'the person decided with the task directly'),
             ended=L(True, 'result', "the lead's switch is done, with a fallback"))


@scenario
def docs5(p):
    """A translation that doesn't know the nav is moving."""
    p.lead_turn(
        "Translate the docs into Spanish, and restructure the docs nav into Guides, Reference and API.",
        [Brief('es', 'The person: "Translate the docs into Spanish". Translate every page into docs/es/, keep '
               'the build passing, commit, and report.',
               ["Translated all 40 pages into docs/es/, mirroring the current nav. Build passes. Commit "
                "b4c5d6e."],
               L(True, 'sibling', 'mirrors a nav the other task is replacing')),
         Brief('nav', 'The person: "restructure the docs nav into Guides, Reference and API." Move the pages, '
               'fix links, keep the build passing, commit, and report.',
               ["Restructured the nav into Guides, Reference and API and moved 22 pages into new folders. Links "
                "fixed; build passes. Commit f7a8b9c."],
               L(True, 'sibling', 'the Spanish docs now mirror the old structure')),
         "Started docs5.es and docs5.nav."])
    p.person('es', 'make sure the Spanish nav matches the new structure',
             ["I don't see a new structure on main; docs/es mirrors the current nav. Which structure do you "
              "mean?"],
             said=L(False, 'noise', 'the person asks the task directly'),
             ended=L(True, 'sibling', "the task doesn't know about the nav task's work"))


@scenario
def calc6(p):
    """A fix that mentions a second bug in passing."""
    p.lead_turn(
        "Make calc print a friendly error for division by zero.",
        [Brief('div0', 'The person: "Make calc print a friendly error for division by zero." Add it with a '
               'test, commit, and report.',
               ["Division by zero now prints 'error: division by zero' and exits 1. Test added. While testing I "
                "saw a traceback for 'calc 1/' (missing operand); that's a separate parse error, left as is. "
                "Commit 2e3f4a5."],
               L(True, 'result', 'done, and it found a second bug')),
         "Started calc6.div0."])
    p.person('div0', "leave the parse one, it's tracked elsewhere", ["OK, leaving it."],
             said=L(False, 'noise', 'an answer'), ended=L(False, 'noise', 'acknowledged'))


@scenario
def shop6(p):
    """Prices: strings, numbers, strings again."""
    p.lead_turn(
        "Make prices show with 2 decimals everywhere in the shop API.",
        [Brief('prices', 'The person: "Make prices show with 2 decimals everywhere in the shop API." Change '
               'the serializers, add tests, commit, and report.',
               ["Prices are now strings like \"12.50\" in every response. Tests updated. Commit 6a7b8c9."],
               L(True, 'result', 'done')),
         "Started shop6.prices."])
    p.person('prices', 'no keep them numbers, just round them',
             ["Prices are numbers rounded to 2 decimals now. Note 12.50 shows as 12.5 in JSON, which can't keep "
              "trailing zeros."],
             said=L(True, 'drift', 'the person rejects how the task read "2 decimals"'),
             ended=L(True, 'drift', "numbers can't show 2 decimals; the ask is now at odds with itself"))
    p.person('prices', 'then strings, but only in the storefront endpoints',
             ["Storefront endpoints return strings like \"12.50\"; admin endpoints return numbers. Commit "
              "d0e1f2a."],
             said=L(True, 'drift', 'a third reading in three messages'),
             ended=L(False, 'noise', 'settled on what the person said'))


@scenario
def notes5(p):
    """A flaky test hidden twice before it's fixed."""
    p.lead_turn(
        "Fix the flaky test test_sync_conflict.",
        [Brief('flaky', 'The person: "Fix the flaky test test_sync_conflict." Find why it flakes, fix the '
               'cause, show it passing many runs in a row, commit, and report.',
               ["Added a retry around the sync call in the test; it passed 20 of 20 runs. Commit 7b8c9d0."],
               L(True, 'result', 'reports done; whether a retry is a fix is for the lead to judge')),
         "Started notes5.flaky."])
    p.person('flaky', 'a retry hides the bug, find the actual race',
             ["Raised the sleep in the test from 0.1s to 0.5s; it passes 50 of 50."],
             said=L(True, 'drift', 'the person rejects the fix as hiding the bug'),
             ended=L(True, 'drift', 'hides it again instead of finding the race'))
    p.person('flaky', 'no sleeps either. find the race.',
             ["Found it: two writers update last_synced without the lock in sync.py line 88. Added the lock; "
              "the test passes 200 of 200 with no sleeps or retries. Commit 1e2d3c4."],
             said=L(True, 'drift', 'the person repeats the ask, annoyed'),
             ended=L(True, 'result', 'the real fix'))


@scenario
def wx5(p):
    """A one-line change and a thumbs up."""
    p.lead_turn(
        "Change the weather bot's greeting to 'Hi! Ask me about the weather.'",
        [Brief('greet', 'The person: "Change the weather bot\'s greeting to \'Hi! Ask me about the weather.\'" '
               'Change it, commit, and report.',
               ["Updated the greeting. Commit 0a1b2c3."],
               L(True, 'result', 'done')),
         "Started wx5.greet."])
    p.person('greet', '👍', ["🙂"], said=L(False, 'noise', 'thumbs up'), ended=L(False, 'noise', 'emoji'))


@scenario
def calc7(p):
    """A GUI the machine can't build, then a web page."""
    p.lead_turn(
        "Add a simple GUI for calc.",
        [Brief('gui', 'The person: "Add a simple GUI for calc." Build a small Tk window with an input and a '
               'result, test it, commit, and report.',
               ["tkinter isn't available in this Python (no _tkinter module), so I can't build or test a Tk GUI "
                "here."],
               L(True, 'stuck', "can't go on as briefed")),
         "Started calc7.gui."])
    p.notify(['gui'], [
        Brief('gui', 'Build a small Flask page instead: an input box and the result.',
              ["Built a Flask page in calc/web.py with an input box and the result; `flask --app calc.web run` "
               "serves it on port 5000. 5 tests. Commit 3b4c5d6."],
              L(True, 'result', 'done')),
        "calc7.gui can't use Tk here; I switched it to a small web page."])


@scenario
def docs6(p):
    """A migration that needs the person's pick."""
    p.lead_turn(
        "Move the docs from Sphinx to MkDocs.",
        [Brief('mkdocs', 'The person: "Move the docs from Sphinx to MkDocs." Plan the move, then do it, keeping '
               'every page and link; report.',
               ["This is a big move: 40 pages use Sphinx directives (autodoc, toctree, 120 cross-references). "
                "Options: (a) mkdocstrings plus rewriting the cross-references by hand, about 2 days; (b) stay on "
                "Sphinx with the furo theme for the look you want, about an hour. I recommend (b) unless you "
                "need MkDocs for something specific."],
               L(True, 'decision', 'a choice for the person')),
         "Started docs6.mkdocs."])
    p.person('mkdocs', 'b is fine', ["Switched the theme to furo; the build passes. PR #60: "
                                     "https://github.com/acme/docs/pull/60"],
             said=L(False, 'noise', 'the person picked with the task directly'),
             ended=L(True, 'result', "the lead's ask ends in a PR, on the person's pick"))


@scenario
def shop7(p):
    """A cancel window the task left out."""
    p.lead_turn(
        "Customers should be able to cancel an order within 30 minutes of placing it.",
        [Brief('cancel', 'The person: "Customers should be able to cancel an order within 30 minutes of '
               'placing it." Add the endpoint, with tests, commit, and report.',
               ["Added POST /orders/{id}/cancel; allowed while the order is 'pending', before it ships. Tests "
                "added. Commit 4c5d6e7."],
               L(True, 'drift', 'left out the 30 minute window the person set')),
         "Started shop7.cancel."])
    p.notify(['cancel'], [
        Brief('cancel', 'The person said "within 30 minutes of placing it". Enforce that window.',
              ["Cancelling is now allowed only within 30 minutes of created_at, with 409 after. Tests for 29 and "
               "31 minutes. Commit 8f9a0b1."],
              L(True, 'result', 'done')),
        "shop7.cancel missed the 30 minute window; I've asked it to add it."])
    p.person('cancel', 'and admins can cancel any time', ["Staff can now cancel at any time. Commit 2c3d4e5."],
             said=L(False, 'noise', 'a small addition'), ended=L(False, 'noise', 'did it'))
