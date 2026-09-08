"""Bounded validation of synthetic workload events and observer-side timings."""

import math
import re


def percentiles(values):
    if not values:
        return {"p50": None, "p95": None, "p99": None}
    ordered = sorted(values)
    return {f"p{p}": ordered[max(0, math.ceil(len(ordered) * p / 100) - 1)]
            for p in (50, 95, 99)}


class Events:
    def __init__(self, workload):
        self.workload = workload
        self.ready = None
        self.active = {}
        self.active_agents = set()
        self.seen = set()
        self.agents = {}
        self.first = []
        self.latencies = []
        self.bytes = 0
        self.peak = 0
        self.diagnostic = None

    def add(self, event, now):
        if not isinstance(event, dict):
            raise ValueError("event must be an object")
        kind = event.get("event")
        if kind == 'diagnostic':
            code = event.get('code')
            allowed = {'provider_connection_timeout', 'provider_connection_failed', 'provider_stream_failed',
                       'missing_completion', 'output_closed', 'benchmark_failed'}
            if (self.diagnostic is not None or event.get('stage') != 'benchmark'
                    or not isinstance(code, str) or not (code in allowed or
                    re.fullmatch(r'provider_connection_os_-?[0-9]{1,10}', code))):
                raise ValueError('invalid static diagnostic')
            self.diagnostic = {'stage': 'benchmark', 'code': code}
            return
        if kind == "ready":
            if self.ready is not None:
                raise ValueError("duplicate ready event")
            self.ready = now
            return
        if self.ready is None:
            raise ValueError("missing ready event")
        agent, turn = event.get("agent"), event.get("turn")
        if not all(isinstance(value, str) and 0 < len(value) <= 128
                   for value in (agent, turn)):
            raise ValueError("invalid agent or turn identity")
        key = (agent, turn)
        if kind == "turn_start":
            if key in self.seen or agent in self.active_agents:
                raise ValueError("duplicate or concurrent turn on one agent")
            if len(self.seen) >= self.workload["concurrency"] * self.workload["turns"]:
                raise ValueError("too many turns")
            self.agents[agent] = self.agents.get(agent, 0) + 1
            if (len(self.agents) > self.workload["concurrency"]
                    or self.agents[agent] > self.workload["turns"]):
                raise ValueError("workload identity count mismatch")
            self.seen.add(key)
            self.active[key] = [now, 0]
            self.active_agents.add(agent)
            self.peak = max(self.peak, len(self.active))
        elif kind == "chunk":
            if key not in self.active:
                raise ValueError("chunk for inactive turn")
            start, count = self.active[key]
            if (type(event.get("seq")) is not int or event["seq"] != count
                    or count >= self.workload["chunks"]
                    or type(event.get("bytes")) is not int
                    or event["bytes"] != self.workload["chunk_bytes"]):
                raise ValueError("chunk sequence or byte count mismatch")
            if count == 0:
                self.first.append((now - start) * 1000)
            self.bytes += event["bytes"]
            self.active[key][1] += 1
        elif kind == "turn_end":
            if key not in self.active or self.active[key][1] != self.workload["chunks"]:
                raise ValueError("incomplete or unknown turn")
            start, _ = self.active.pop(key)
            self.active_agents.remove(agent)
            self.latencies.append((now - start) * 1000)
        else:
            raise ValueError("unknown benchmark event")

    def finish(self):
        expected = self.workload["concurrency"] * self.workload["turns"]
        if self.diagnostic or self.ready is None or self.active or len(self.latencies) != expected:
            raise ValueError("workload did not complete")
        return {"completed_turns": len(self.latencies),
                "peak_in_flight_turns": self.peak,
                "stream_payload_bytes": self.bytes,
                "ready_seconds": self.ready,
                "observed_first_chunk_ms": percentiles(self.first),
                "observed_turn_ms": percentiles(self.latencies)}
