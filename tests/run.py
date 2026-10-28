"""Run the test modules side by side, one process each.

Each module starts its own daemons on its own stores and ports, so modules
share nothing but the machine. A module's output is printed only when it
fails. Usage: python -m tests.run [--jobs N] [MODULE ...]
"""

import argparse
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def run(module):
    started = time.monotonic()
    done = subprocess.run([sys.executable, '-m', 'unittest', module], cwd=ROOT,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return module, done.returncode, time.monotonic() - started, done.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--jobs', type=int, default=4)
    parser.add_argument('modules', nargs='*')
    args = parser.parse_args()
    # The largest modules first, as the longest-running, so none starts last.
    paths = sorted((ROOT / 'tests').glob('test_*.py'), key=lambda path: -path.stat().st_size)
    modules = args.modules or [f'tests.{path.stem}' for path in paths]
    failed = []
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for future in as_completed([pool.submit(run, module) for module in modules]):
            module, code, seconds, output = future.result()
            summary = output.rstrip().splitlines()[-1] if output.strip() else ''
            print(f'{module}: {"ok" if code == 0 else "FAILED"} in {seconds:.0f} s ({summary})', flush=True)
            if code:
                failed.append((module, output))
    for module, output in failed:
        print(f'\n===== {module} =====\n{output}', flush=True)
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
