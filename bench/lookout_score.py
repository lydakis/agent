"""Score an `agent lookout --replay` run against hand labels.

Usage: python3 lookout_score.py REPLAY.jsonl LABELS.jsonl
LABELS has one {"n": N, "tell": true|false, "why": "..."} per check: whether
the lead should have been told about that event at all. A judge's Wake and
Hold both tell the lead (Hold later); Drop does not. The baseline is the app's
digest wake, which tells the lead about every turn a task ends.
"""
import json, statistics, sys
from collections import defaultdict

PRICE = {"openai-decisions": 0.10, "typesafe": 0.042}  # $ per million input tokens

replay = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
labels = {r["n"]: r["tell"] for r in (json.loads(l) for l in open(sys.argv[2]) if l.strip())}
by = defaultdict(list)
for r in replay:
    by[r["judge"]].append(r)

def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))] if xs else 0

checks = sorted({r["n"] for r in replay if r["n"] in labels})
tell = sum(labels[n] for n in checks)
ended = {r["n"] for r in replay if r["kind"] == "Ended"}
print(f"{len(checks)} labelled checks, {tell} the lead should hear about")
digest_hits = sum(labels[n] for n in checks if n in ended)
print(f"digest baseline: tells {len([n for n in checks if n in ended])}, "
      f"of them {digest_hits} right; misses {tell - digest_hits}")
for judge, rows in by.items():
    rows = [r for r in rows if r["n"] in labels]
    failed = [r for r in rows if "failed" in r]
    ok = [r for r in rows if "band" in r]
    wake = [r for r in ok if r["band"].startswith("Wake")]
    told = [r for r in ok if not r["band"].startswith("Drop")]
    tp_w = sum(labels[r["n"]] for r in wake)
    tp_t = sum(labels[r["n"]] for r in told)
    tokens = sum(r.get("tokens", 0) for r in ok)
    price = PRICE.get(judge.split("/")[0], 0.10)
    ms = [r["ms"] for r in ok]
    print(f"\n{judge}: {len(ok)} answered, {len(failed)} failed")
    print(f"  wake now {len(wake)} (precision {tp_w / max(1, len(wake)):.2f}, recall {tp_w / max(1, tell):.2f})")
    print(f"  told at all {len(told)} (precision {tp_t / max(1, len(told)):.2f}, recall {tp_t / max(1, tell):.2f})")
    print(f"  false wakes: {[r['n'] for r in wake if not labels[r['n']]]}")
    print(f"  missed: {[n for n in checks if labels[n] and n not in {r['n'] for r in told}]}")
    print(f"  latency p50 {pct(ms, 0.5)} ms, p99 {pct(ms, 0.99)} ms; "
          f"{tokens / max(1, len(ok)):.0f} input tokens a check, ${tokens * price / 1e6:.4f} in all")
    fired = defaultdict(int)
    for r in ok:
        for q, _ in r.get("reasons", []):
            fired[q] += 1
    print(f"  questions that fired (>= 0.35): {dict(fired)}")
