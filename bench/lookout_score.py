"""Score a `sonny lookout --replay` run against hand labels.

Usage: python3 lookout_score.py REPLAY.jsonl LABELS.jsonl
LABELS has one label per check: whether the lead should have been told about
that event at all (`tell`), keyed by the check's `n`, or by its `task`, `turn`
and `kind` as `bench/lookout_cases` writes them, with an optional `cat`. A
judge's Wake and Hold both tell the lead (Hold later); Drop does not. The
baseline is the task updates, which tell the lead of every turn a task ends
and every message someone else writes in one, so each judge is also scored
with every Ended and Said check counted as told: what reaches the lead at
all, judged or not. The judge then decides only what steers it at once.

With a `lead` judge in the run, each other judge is also scored as the first
stage of a two-stage judge: a check whose highest answer reaches the
threshold goes on to the lead's fork, which decides it; the rest are dropped.
"""
import json, math, sys
from collections import defaultdict

PRICE = {"openai-decisions": 0.10}  # $ per million input tokens
STAGES = (0.10, 0.20, 0.35)

replay = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
labelled = [json.loads(l) for l in open(sys.argv[2]) if l.strip()]


by_n = bool(labelled) and "n" in labelled[0]


def key(row):
    return row["n"] if by_n else (row["task"], row["turn"], row["kind"])


labels = {key(r): r for r in labelled}
rows = defaultdict(dict)
for r in replay:
    if key(r) in labels:
        rows[r["judge"]][key(r)] = r
checks = sorted({key(r) for r in replay if key(r) in labels}, key=str)
tell = {k for k in checks if labels[k]["tell"]}
cats = sorted({labels[k].get("cat", "") for k in checks} - {""})


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))] if xs else 0


def ratio(a, b):
    return f"{a / b:.2f}" if b else "-"


def mcnemar(a, b):
    """Two-sided exact p for paired yes/no outcomes that disagree a and b times."""
    n = a + b
    if not n:
        return 1.0
    tail = sum(math.comb(n, i) for i in range(min(a, b) + 1)) / 2 ** n
    return min(1.0, 2 * tail)


def told(row):
    return "band" in row and not row["band"].startswith("Drop")


def woke(row):
    return "band" in row and row["band"].startswith("Wake")


def report(name, told_set, woke_set, answered, failed, ms, tokens, cached, extra=""):
    print(f"\n{name}: {answered} answered, {failed} failed{extra}")
    tp_w, tp_t = len(woke_set & tell), len(told_set & tell)
    print(f"  wake now {len(woke_set)} (precision {ratio(tp_w, len(woke_set))}, recall {ratio(tp_w, len(tell))})")
    print(f"  told at all {len(told_set)} (precision {ratio(tp_t, len(told_set))}, recall {ratio(tp_t, len(tell))})")
    updated = told_set | news
    tp_u = len(updated & tell)
    print(f"  told with task updates {len(updated)} (precision {ratio(tp_u, len(updated))}, "
          f"recall {ratio(tp_u, len(tell))}); missed: {sorted(tell - updated, key=str)}")
    for cat in cats:
        mine = {k for k in checks if labels[k].get("cat") == cat}
        if mine & tell:
            print(f"    {cat}: told {len(told_set & mine & tell)} of {len(mine & tell)}, "
                  f"woke {len(woke_set & mine & tell)}")
        else:
            print(f"    {cat} (should stay quiet): told {len(told_set & mine)} of {len(mine)}, "
                  f"woke {len(woke_set & mine)}")
    print(f"  false wakes: {sorted(woke_set - tell, key=str)}")
    print(f"  missed: {sorted(tell - told_set, key=str)}")
    print(f"  latency p50 {pct(ms, 0.5)} ms, p99 {pct(ms, 0.99)} ms; "
          f"{tokens / max(1, answered):.0f} input tokens a check, {cached / max(1, answered):.0f} of them cached")


print(f"{len(checks)} labelled checks, {len(tell)} the lead should hear about")
news = {k for k in checks for judge in rows.values()
        if k in judge and judge[k]["kind"] in ("Ended", "Said")}
print(f"task updates baseline: tells {len(news)}, of them {len(news & tell)} right; misses {len(tell - news)}")

outcomes = {}
for judge, mine in rows.items():
    ok = [r for r in mine.values() if "band" in r]
    told_set = {key(r) for r in ok if told(r)}
    woke_set = {key(r) for r in ok if woke(r)}
    outcomes[judge] = told_set
    tokens = sum(r.get("tokens", 0) for r in ok)
    cached = sum(r.get("cached", 0) for r in ok)
    price = PRICE.get(judge.split("/")[0])
    spend = f"; ${tokens * price / 1e6:.4f} in all" if price else ""
    report(judge, told_set, woke_set, len(ok), len([r for r in mine.values() if "failed" in r]),
           [r["ms"] for r in ok], tokens, cached, spend)
    fired = defaultdict(int)
    for r in ok:
        for q, _ in r.get("reasons", []):
            fired[q] += 1
    print(f"  questions that fired (>= 0.35): {dict(fired)}")

lead = rows.get("lead")
if lead:
    for judge, first in rows.items():
        if judge == "lead":
            continue
        for threshold in STAGES:
            # The first stage decides which checks the lead's fork sees;
            # only those need its answer.
            asked = [k for k in checks if "band" in first.get(k, {})]
            up = {k for k in asked if max(first[k]["answers"].values(), default=0) >= threshold}
            failed = {k for k in up if "band" not in lead.get(k, {})}
            done = [k for k in asked if k not in failed]
            told_set = {k for k in up - failed if told(lead[k])}
            woke_set = {k for k in up - failed if woke(lead[k])}
            ms = [first[k]["ms"] + (lead[k]["ms"] if k in up else 0) for k in done]
            tokens = sum(first[k].get("tokens", 0) + (lead[k].get("tokens", 0) if k in up else 0) for k in done)
            cached = sum(first[k].get("cached", 0) + (lead[k].get("cached", 0) if k in up else 0) for k in done)
            outcomes[f"{judge}>lead@{threshold}"] = told_set
            report(f"{judge} > lead at {threshold}", told_set, woke_set, len(done), len(failed), ms, tokens,
                   cached, f"; {len(up)} of {len(asked)} went on to the lead ({ratio(len(up), len(asked))})")

# Each judge against the first one the run names, as the one in use.
print("\npaired on the checks the lead should hear about (McNemar, told at all):")
names = list(outcomes)
for a in names[:1]:
    for b in names[1:]:
        only_a = len((outcomes[a] - outcomes[b]) & tell)
        only_b = len((outcomes[b] - outcomes[a]) & tell)
        print(f"  {a} vs {b}: only the first told {only_a}, only the second {only_b}, p {mcnemar(only_a, only_b):.3f}")
