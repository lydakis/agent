#!/bin/sh
# A tiny project with one bug for the demo model to fix: setup-workspace.sh DIR
set -e
mkdir -p "$1" && cd "$1"
cat > calc.py <<'PY'
def average(values):
    return sum(values) / (len(values) - 1)
PY
cat > test_calc.py <<'PY'
from calc import average
cases = [([2, 4], 3), ([5], 5), ([1, 2, 3], 2)]
failed = 0
for values, want in cases:
    try:
        got = average(values)
    except ZeroDivisionError:
        got = 'ZeroDivisionError'
    if got != want:
        failed += 1
        print(f'FAIL average({values}) = {got}, want {want}')
print(f'{len(cases) - failed} passed, {failed} failed')
raise SystemExit(1 if failed else 0)
PY
