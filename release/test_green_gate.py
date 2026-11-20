import unittest

import green_gate

COMMIT = "548534aa5179ed554562c84be26b531071b35b12"


def step(name, conclusion="success"):
    return {"name": name, "status": "completed", "conclusion": conclusion}


def job(name, *steps, conclusion="success", status="completed"):
    return {"name": name, "status": status, "conclusion": conclusion,
            "steps": [step("Set up job"), *steps, step("Complete job")]}


def run(number, status="completed", attempt=1):
    return {"id": number, "status": status, "run_attempt": attempt, "created_at": f"2026-11-19T12:0{number}:00Z",
            "html_url": f"https://github.com/o/r/actions/runs/{number}"}


GREEN = [job("rust", step("Rust tests")), job("runtime", step("Install bubblewrap"), step("Runtime tests"))]
# 0.10.2's commit: the apt mirror failed the install and the tests were skipped.
SKIPPED = [job("rust", step("Rust tests")),
           job("runtime", step("Install bubblewrap", "failure"), step("Runtime tests", "skipped"),
               conclusion="failure")]


class GreenGateTest(unittest.TestCase):
    def test_a_run_with_every_job_and_step_a_success_passes(self):
        passed, message = green_gate.verdict(COMMIT, [run(1)], {1: GREEN})
        self.assertTrue(passed)
        self.assertIn("runs/1", message)

    def test_a_skipped_step_refuses_the_release_and_says_where(self):
        passed, message = green_gate.verdict(COMMIT, [run(1)], {1: SKIPPED})
        self.assertFalse(passed)
        self.assertIn("runtime: Install bubblewrap failure", message)
        self.assertIn("runtime: Runtime tests skipped", message)
        self.assertNotIn("rust:", message)

    def test_a_skipped_or_cancelled_job_is_not_a_success(self):
        for conclusion in ("skipped", "cancelled", "failure"):
            jobs = [job("rust", step("Rust tests")), {"name": "macos", "status": "completed",
                                                      "conclusion": conclusion, "steps": []}]
            passed, message = green_gate.verdict(COMMIT, [run(1)], {1: jobs})
            self.assertFalse(passed)
            self.assertIn(f"macos: {conclusion}", message)

    def test_a_re_run_that_passed_lets_the_release_go(self):
        passed, _ = green_gate.verdict(COMMIT, [run(1, attempt=2)], {1: GREEN})
        self.assertTrue(passed)
        passed, _ = green_gate.verdict(COMMIT, [run(2), run(1)], {2: SKIPPED, 1: GREEN})
        self.assertTrue(passed)

    def test_no_run_a_running_one_or_one_with_no_jobs_refuses(self):
        self.assertIn("no test.yml run was pushed", green_gate.verdict(COMMIT, [], {})[1])
        running = [job("rust", step("Rust tests")), job("runtime", status="in_progress", conclusion=None)]
        passed, message = green_gate.verdict(COMMIT, [run(1, "in_progress")], {1: running})
        self.assertFalse(passed)
        self.assertIn("run is still in_progress", message)
        self.assertIn("runtime: still in_progress", message)
        self.assertFalse(green_gate.verdict(COMMIT, [run(1)], {1: []})[0])


if __name__ == "__main__":
    unittest.main()
