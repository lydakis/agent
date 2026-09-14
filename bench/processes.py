"""Sample process counters without reading command arguments or environments."""

from dataclasses import dataclass
import os

import psutil


@dataclass
class Process:
    pid: int
    ppid: int
    group: int
    rss: int | None
    cpu: float | None
    birth: str
    threads: int | None = None


def snapshot(trees=(), *, scan_groups=True):
    rows = []
    observer = psutil.Process()
    processes = {p.pid: p for p in [observer, *observer.children(recursive=True)]}
    # Retain detached descendants and catch still-owned process-group members
    # after their immediate parent exits. Only query identities for candidates.
    groups = {tree.root for tree in trees}
    known = {pid for tree in trees for pid in tree.known} | groups
    for pid in (psutil.pids() if scan_groups else known) if groups else ():
        if pid <= 1:
            continue  # getpgid(0) refers to the observer, not the kernel.
        try:
            if pid in known or os.getpgid(pid) in groups:
                processes.setdefault(pid, psutil.Process(pid))
        except (psutil.NoSuchProcess, ProcessLookupError, PermissionError):
            continue
    # Do not request cmdline, environ, names, or counters for unrelated processes.
    for process in processes.values():
        try:
            if process.pid <= 1:
                continue
            with process.oneshot():
                rows.append(Process(process.pid, process.ppid(), os.getpgid(process.pid),
                                    None, None, str(process.create_time())))
        except (psutil.NoSuchProcess, psutil.AccessDenied, ProcessLookupError,
                PermissionError):
            continue
    return rows


def counters(row):
    if row.rss is not None and row.cpu is not None:
        return row.rss, row.cpu, row.threads
    process = psutil.Process(row.pid)
    with process.oneshot():
        if str(process.create_time()) != row.birth:
            raise psutil.NoSuchProcess(row.pid)
        cpu = process.cpu_times()
        return process.memory_info().rss, cpu.user + cpu.system, process.num_threads()


class Tree:
    def __init__(self, root):
        if root <= 1:
            raise ValueError("only spawned user processes can be measurement roots")
        self.root = root
        self.known = {}
        self.cpu = {}

    def members(self, rows):
        eligible = [row for row in rows if row.pid > 1 and (row.pid != self.root
                    or self.root not in self.known or self.known[self.root] == row.birth)]
        selected = {row.pid for row in eligible
                    if row.pid == self.root or row.group == self.root
                    or self.known.get(row.pid) == row.birth}
        while True:
            expanded = selected | {row.pid for row in eligible if row.ppid in selected}
            if expanded == selected:
                break
            selected = expanded
        return [row for row in eligible if row.pid in selected]

    def sample(self, rows):
        members = self.members(rows)
        rss_total = 0
        unreadable = 0
        live = 0
        thread_total = 0
        threads_known = True
        for row in members:
            self.known[row.pid] = row.birth
            key = (row.pid, row.birth)
            try:
                rss, cpu, threads = counters(row)
                live += 1
                rss_total += rss
                if threads is None:
                    threads_known = False
                else:
                    thread_total += threads
                self.cpu[key] = max(self.cpu.get(key, 0), cpu)
            except psutil.NoSuchProcess:
                continue
            except psutil.AccessDenied:
                unreadable += 1
        return {"processes": live + unreadable,
                "rss_bytes": None if unreadable else rss_total,
                "threads": thread_total if threads_known and not unreadable else None,
                "unreadable_processes": unreadable,
                "observed_cpu_seconds": sum(self.cpu.values()),
                "pss_bytes": None, "private_bytes": None}
