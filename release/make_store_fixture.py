#!/usr/bin/env python3
"""Make the synthetic store a release writes, as a SQL dump the tests load.

    make_store_fixture.py TAG [--binary PATH]

runs TAG's daemon (fetched with release_binary.py, or PATH) against a
scripted model and writes tests/fixtures/store-TAG.sql. The store holds
what a person's does, made up: a lead in code mode that declares Sonny's
`exec` with every field a declared tool has, a browser and a desktop tool,
and one that answers alone; a finished turn with a script call answered; a
turn parked on a call nobody answers; a message queued behind it; a helper
with a tool call in its history; and a fork of it.
tests/test_release_stores.py opens it with the daemon under test, runs
turns, takes it back down with store_down.py and opens it with TAG's.
"""

import http.server
import json
import queue
import sqlite3
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "release"))

from bench.runtime_client import Client  # noqa: E402
from release_binary import fetch  # noqa: E402
from tests.test_runtime import Model  # noqa: E402

# The folder every fixture bot works in; the test makes it too.
WORKSPACE = "/tmp/agent-store-fixture"
MODEL = "synthetic-model"


def tools(protocol):
    """What the lead declares, in Sonny's shapes. #1203 (protocol 92) named
    `lift` `yields`."""
    yields = "yields" if protocol >= 92 else "lift"
    exec_tool = {
        "name": "exec",
        "description": "Run JavaScript that calls tools.",
        "parameters": {
            "type": "object",
            "properties": {"input": {"type": "string", "description": "Raw JavaScript."}},
            "required": ["input"],
            "additionalProperties": False,
        },
        "grammar": 'start: SOURCE\nSOURCE: /(.|\\n)+/',
        "answered_by": "sonny",
        "expire_ms": 3_600_000,
        "inherit": 1,
        "steps": True,
        "gate_daemon_steps": True,
        yields: True,
        "kind": "mcp",
    }
    page = {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]}
    return [
        exec_tool,
        {"name": "browser_open", "description": "Open a page.", "parameters": page,
         "answered_by": "sonny", "kind": "browser"},
        {"name": "desktop_click", "description": "Click a point.",
         "parameters": {"type": "object", "properties": {"x": {"type": "number", "minimum": 1e-7},
                                                          "y": {"type": "number"}}},
         "answered_by": "sonny", "kind": "desktop"},
        {"name": "note", "description": "Keep a note.", "parameters": {"type": "object"},
         "result": "Noted.", "inherit": False, "kind": "file"},
    ]


def respond(request, user):
    """The scripted model: a script call then a word on its result, a call
    left waiting, or a plain answer; None leaves `calls:` prompts to Model."""
    items = request["input"]
    start = max(n for n, i in enumerate(items) if i.get("role") == "user")
    answered = [i for i in items[start:] if i.get("type", "").endswith("call_output")]
    if user == "run a script":
        return "the script said " + answered[-1]["output"] if answered else [["exec", 'text("ok")']]
    if user == "wait on a script":
        return "the script ended" if answered else [["exec", "await yield_control()"]]
    if user.startswith("calls:"):
        return None
    return "heard: " + user


def make(binary, out):
    Path(WORKSPACE).mkdir(parents=True, exist_ok=True)

    class Server(http.server.ThreadingHTTPServer):
        daemon_threads = True

    model = Server(("127.0.0.1", 0), Model)
    model.requests = queue.Queue()
    model.respond = respond
    threading.Thread(target=model.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{model.server_port}/v1"
    with tempfile.TemporaryDirectory() as folder:
        store = Path(folder) / "state.sqlite"
        client = Client(binary, store, url, tools="echo,shell")
        try:
            protocol, schema = client.ready["protocol"], client.ready["schema"]

            def ok(response):
                if "result" not in response:
                    raise SystemExit(f"the daemon refused: {response}")
                return response["result"]

            def turn(bot, request_id, prompt, **more):
                return ok(client.request("submit", bot=bot, request_id=request_id, prompt=prompt,
                                         **more))["message"]

            def ended(message):
                status = client.finished(message, timeout=15)["data"]["status"]
                if status != "completed":
                    raise SystemExit(f"turn {message} ended {status}")

            ok(client.request("create", bot="Lead", workspace=WORKSPACE, declare=tools(protocol),
                              settings={"steps_only": ["*"], "approval_hold_ms": 0}))
            ok(client.request("create", bot="Lead.task", workspace=WORKSPACE,
                              declare=tools(protocol)[:1], settings={"steps_only": ["*"]}))
            ok(client.request("create", bot="Helper", workspace=WORKSPACE))
            # A script call its client answers.
            first = turn("Lead", "script", "run a script")
            client.receive(lambda m: m.get("event") == "tool_started" and m.get("turn") == first, 15)
            ok(client.request("tool_result", bot="Lead", turn=first, call_id="exec-1", output="ok"))
            ended(first)
            ended(turn("Lead.task", "hello", "hello"))
            ended(turn("Helper", "echo", "calls:" + json.dumps([["echo", {"text": "hi"}]])))
            ok(client.request("fork", source="Helper", bot="Helper.fork"))
            # A call nobody answers parks its turn, and a message waits behind it.
            parked = turn("Lead", "park", "wait on a script")
            client.receive(lambda m: m.get("event") == "turn_waiting" and m.get("turn") == parked, 15)
            turn("Lead", "queued", "after the script", delivery="queue")
        finally:
            client.close()
        dump(store, schema, out)
    model.shutdown()
    return out


def dump(store, schema, out):
    conn = sqlite3.connect(store)
    try:
        if conn.execute("PRAGMA user_version").fetchone()[0] != schema:
            raise SystemExit("the store's schema is not the one its daemon named")
        lines = [f"PRAGMA user_version={schema};", *conn.iterdump()]
    finally:
        conn.close()
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text("\n".join(lines) + "\n")


def main(argv):
    args = list(argv)
    binary = None
    if "--binary" in args:
        at = args.index("--binary")
        binary = Path(args[at + 1]).resolve()
        del args[at : at + 2]
    if len(args) != 1:
        raise SystemExit(__doc__)
    tag = args[0]
    with tempfile.TemporaryDirectory() as fetched:
        binary = binary or fetch(tag, fetched)
        version = subprocess.run([binary, "--version"], capture_output=True, text=True, check=True)
        if version.stdout.split()[1] != tag[1:]:
            raise SystemExit(f"{binary} is {version.stdout.strip()}, not {tag}")
        print(make(binary, ROOT / "tests" / "fixtures" / f"store-{tag}.sql"))


if __name__ == "__main__":
    main(sys.argv[1:])
