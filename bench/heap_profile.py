"""Attribute a dhat heap profile's bytes at peak to the code that allocated them.

Build the profiling daemon and point AGENT_HEAP_PROFILE at an output path:

    CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=1 \\
      cargo build --release --features heap-profile --target-dir .local/target-dhat
    AGENT_HEAP_PROFILE=.local/bench/heap.json .local/venv/bin/python -m bench.fleet_screen \\
      --binary .local/target-dhat/release/agent ...
    .local/venv/bin/python -m bench.heap_profile .local/bench/heap.json

Prints live bytes at the global peak (dhat's `gb`) grouped by the first frame
inside this crate, and the same by the innermost frame, so the answer is
"which subsystem" and "which allocation" rather than a whole-process RSS.
"""
import json
import sys
from collections import defaultdict

OURS = ('agent_runtime::', 'agent::')
ALLOCATOR = ('dhat::', 'alloc::', '<alloc::', 'core::alloc', '__rust_alloc', '<core::')


def name(frame):
    """A frame's symbol without its address and location."""
    return frame.split(': ', 1)[-1].split(' (')[0]


def crate(frame):
    symbol = name(frame).lstrip('<')
    return symbol.split('::', 1)[0].split(' as ')[0]


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    profile = json.load(open(sys.argv[1]))
    frames = profile['ftbl']
    top = int(sys.argv[2]) if len(sys.argv) > 2 else 25
    peak = sum(pp.get('gb', 0) for pp in profile['pps'])
    by_owner, by_site, by_crate = defaultdict(int), defaultdict(int), defaultdict(int)
    for pp in profile['pps']:
        live = pp.get('gb', 0)
        if not live:
            continue
        stack = [frames[i] for i in pp['fs']]  # innermost first
        real = [f for f in stack if not any(name(f).startswith(m) or m in name(f)[:12] for m in ALLOCATOR)]
        ours = [f for f in stack if any(marker in f for marker in OURS)]
        by_owner[name(ours[0]) if ours else (name(stack[-1]) if stack else '?')] += live
        by_site[name(real[0]) if real else '?'] += live
        by_crate[crate(real[0]) if real else '?'] += live
    print(f"bytes live at the global peak: {peak:,} ({peak / 2**20:.2f} MiB), reached "
          f"{profile.get('tg', 0) / 1e6:.1f} s into a {profile.get('te', 0) / 1e6:.1f} s run")
    for title, table in (('by crate of the innermost non-allocator frame', by_crate),
                         ('by innermost allocation site', by_site),
                         ('by innermost frame in this crate (the code path that asked)', by_owner)):
        print(f'\n{title}:')
        for label, live in sorted(table.items(), key=lambda kv: -kv[1])[:top]:
            print(f'{live / 2**20:9.2f} MiB {100 * live / peak:5.1f}%  {label[:110]}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
