"""Deterministic text-only subset of the OpenAI Responses streaming protocol."""

import re


def prompt(config, agent, turn):
    return f"BENCH agent={agent} turn={turn}\n" + "x" * config["history_bytes"]


class Transcript:
    """Reject retries, missing context, cross-agent context and overlapping turns."""

    def __init__(self, config):
        self.config = config
        self.completed = {}
        self.active = {}

    def accept(self, request):
        if (request.get("model") != "bench-model" or request.get("stream") is not True
                or request.get("previous_response_id")):
            raise ValueError("unsupported model request")
        items = request.get("input")
        if not isinstance(items, list):
            raise ValueError("explicit conversation required")
        conversation = []
        for item in items:
            if item.get("type", "message") != "message":
                raise ValueError("only text messages supported")
            role, content = item.get("role"), item.get("content", [])
            text = content if isinstance(content, str) else "".join(
                part.get("text", "") for part in content)
            if role == "assistant" or (role == "user" and text.startswith("BENCH agent=")):
                conversation.append((role, text))
        if not conversation or conversation[-1][0] != "user":
            raise ValueError("missing current prompt")
        match = re.match(r"BENCH agent=(\d+) turn=(\d+)\n", conversation[-1][1])
        if not match:
            raise ValueError("missing workload identity")
        agent, turn = map(int, match.groups())
        if (agent >= self.config["concurrency"] or turn >= self.config["turns"]
                or agent in self.active or self.completed.get(agent, -1) + 1 != turn):
            raise ValueError("unexpected workload identity or retry")
        expected = []
        for index in range(turn + 1):
            expected.append(("user", prompt(self.config, agent, index)))
            if index < turn:
                expected.append(("assistant", "x" * (self.config["chunks"] * self.config["chunk_bytes"])))
        if conversation != expected:
            raise ValueError("conversation history mismatch")
        self.active[agent] = turn
        return agent, turn

    def complete(self, agent, turn):
        if self.active.pop(agent) != turn:
            raise ValueError("wrong completed turn")
        self.completed[agent] = turn


class Frames:
    def __init__(self, config, agent, turn):
        self.config = config
        self.response_id = f"resp_bench_{agent}_{turn}"
        self.item_id = f"msg_bench_{agent}_{turn}"

    def events(self):
        sequence = 0

        def event(kind, **fields):
            nonlocal sequence
            value = {"type": kind, "sequence_number": sequence, **fields}
            sequence += 1
            return value

        def response(status, output):
            return {"id": self.response_id, "object": "response", "created_at": 0,
                    "model": "bench-model", "status": status, "output": output,
                    "error": None, "incomplete_details": None,
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2,
                              "input_tokens_details": {"cached_tokens": 0},
                              "output_tokens_details": {"reasoning_tokens": 0}}}

        item = {"id": self.item_id, "type": "message", "role": "assistant",
                "status": "in_progress", "content": []}
        part = {"type": "output_text", "text": "", "annotations": [], "logprobs": []}
        indices = {"item_id": self.item_id, "output_index": 0, "content_index": 0}
        yield 0, event("response.created", response=response("in_progress", []))
        yield 0, event("response.output_item.added", output_index=0, item=item)
        yield 0, event("response.content_part.added", **indices, part=part)
        for _ in range(self.config["chunks"]):
            yield self.config["chunk_delay_ms"] / 1000, event(
                "response.output_text.delta", **indices, delta="x" * self.config["chunk_bytes"],
                logprobs=[])
        text = "x" * (self.config["chunk_bytes"] * self.config["chunks"])
        part = {**part, "text": text}
        item = {**item, "status": "completed", "content": [part]}
        yield 0, event("response.output_text.done", **indices, text=text, logprobs=[])
        yield 0, event("response.content_part.done", **indices, part=part)
        yield 0, event("response.output_item.done", output_index=0, item=item)
        yield 0, event("response.completed", response=response("completed", [item]))
