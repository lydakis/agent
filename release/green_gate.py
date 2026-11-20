#!/usr/bin/env python3
"""Refuse a release whose commit has no fully green Test run.

    green_gate.py OWNER/REPO COMMIT

passes when a run of test.yml pushed for COMMIT finished with every job,
and every step of every job, a success: a skipped step or job is a failure,
as when a failed install skipped the runtime tests. Otherwise it exits 1
naming each job and step that was not. Reads the Actions API with
GITHUB_TOKEN (actions: read) when set.
"""

import json
import os
import sys
import time
import urllib.request

WORKFLOW = "test.yml"


def red(job):
    """What in `job` was not a success: the job's own conclusion, then
    each such step's."""
    if job.get("status") != "completed":
        return [f"{job['name']}: still {job.get('status')}"]
    found = [
        f"{job['name']}: {step['name']} {step.get('conclusion') or step.get('status')}"
        for step in job.get("steps", [])
        if step.get("conclusion") != "success"
    ]
    if job.get("conclusion") != "success" and not found:
        found.append(f"{job['name']}: {job.get('conclusion') or job.get('status')}")
    return found


def verdict(commit, runs, jobs):
    """(passed, message): `runs` are test.yml's push runs for `commit`,
    newest first; `jobs` maps a run's id to its jobs' latest attempts."""
    if not runs:
        return False, f"no {WORKFLOW} run was pushed for {commit}; push it to main and let Test finish"
    reasons = []
    for run in runs:
        listed = jobs.get(run["id"], [])
        found = [line for job in listed for line in red(job)] if listed else ["no jobs ran"]
        if run.get("status") != "completed":
            found.insert(0, f"run is still {run.get('status')}")
        if not found:
            return True, f"{WORKFLOW} run {run['html_url']} passed every job for {commit}"
        reasons.append(f"{run['html_url']} (attempt {run.get('run_attempt', 1)}):\n  " + "\n  ".join(found))
    return False, (
        f"no {WORKFLOW} run for {commit} passed every job; re-run the failed jobs, then tag again.\n"
        + "\n".join(reasons)
    )


def get(url, token, attempts=4):
    request = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json"})
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except OSError as error:
            if attempt == attempts - 1:
                raise SystemExit(f"could not read {url}: {error}")
            time.sleep(2 ** attempt * 5)


def main(argv):
    if len(argv) != 2:
        raise SystemExit(__doc__)
    repository, commit = argv
    api = os.environ.get("GITHUB_API_URL", "https://api.github.com")
    token = os.environ.get("GITHUB_TOKEN")
    base = f"{api}/repos/{repository}/actions"
    runs = get(f"{base}/workflows/{WORKFLOW}/runs?head_sha={commit}&event=push&per_page=100", token)
    runs = sorted(runs["workflow_runs"], key=lambda run: run["created_at"], reverse=True)
    jobs = {
        run["id"]: get(f"{base}/runs/{run['id']}/jobs?filter=latest&per_page=100", token)["jobs"]
        for run in runs
    }
    passed, message = verdict(commit, runs, jobs)
    print(message, file=sys.stdout if passed else sys.stderr)
    if not passed:
        sys.exit(1)


if __name__ == "__main__":
    main(sys.argv[1:])
