Feature: Fork a task from a turn it completed
Sonny no longer keeps a task's files after each turn it completes: forking a task from an earlier turn is gone. The commits it kept are still in the repositories the person's tasks work in, under `refs/agent/snapshots`, holding disk that nothing reads.

Tell the person, and with their OK drop them in each repository of a worktree in `~/.agent/worktrees`: `git -C REPO for-each-ref --format='delete %(refname)' refs/agent/snapshots | git -C REPO update-ref --stdin`. Git frees what nothing else holds at its next garbage collection.
