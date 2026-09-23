"""Run bounded workloads with the provider and observer outside the target tree."""

import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

import psutil

from .daemon_driver import DaemonDriver
from .events import Events
from .processes import Tree, snapshot
from .targets import clean_env


def stop(process, tree):
    """Reclaim the owned group plus observed descendants that changed groups."""
    if process is None:
        return
    for sig, grace in ((signal.SIGTERM, 0.3), (signal.SIGKILL, 0)):
        process.poll()  # Reap an exited group leader before signalling again.
        try:
            rows = snapshot((tree,))
        except (OSError, ValueError, psutil.Error, subprocess.SubprocessError):
            rows = []
        members = tree.members(rows)
        live = []
        for row in members:
            try:
                child = psutil.Process(row.pid)
                if str(child.create_time()) == row.birth and child.status() != psutil.STATUS_ZOMBIE:
                    live.append((row, child))
            except psutil.NoSuchProcess:
                pass
        if any(row.group == process.pid for row, _ in live):
            try:
                os.killpg(process.pid, sig)
            except ProcessLookupError:
                pass
        for row, child in live:
            if row.group == process.pid:
                continue
            try:
                child.send_signal(sig)
            except psutil.NoSuchProcess:
                pass
        if grace:
            time.sleep(grace)
    process.wait(timeout=3)
    if process.stdin:
        process.stdin.close()
    if process.stdout:
        process.stdout.close()


def provider_ready(process):
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        deadline = time.monotonic() + 5
        buffer = b""
        while time.monotonic() < deadline:
            if not selector.select(timeout=max(0, deadline - time.monotonic())):
                break
            chunk = os.read(process.stdout.fileno(), 1024)
            if not chunk:
                break
            buffer += chunk
            if len(buffer) > 1024:
                break
            if b"\n" in buffer:
                value = json.loads(buffer.split(b"\n", 1)[0])
                port = value["port"]
                if type(port) is int and 0 < port < 65536:
                    return port
                break
    raise RuntimeError("synthetic provider did not become ready")


def run_once(command, config, options, directory, index):
    provider = target = None
    provider_tree = target_tree = None
    start = None
    cpu_start = time.process_time()
    observer_start = time.monotonic()
    sample_wall = 0
    sample_count = target_sample_count = 0
    observer_peak_rss = 0
    peaks = {"target": {"rss_bytes": 0, "processes": 0, "threads": 0},
             "provider": {"rss_bytes": 0, "processes": 0, "threads": 0}}
    counters = {}
    stats_path = directory / f"provider-{index}.json"
    workload_path = directory / "workload.json"
    events = Events(config)
    status = "error"
    detail = None
    event_summary = None
    exit_code = None
    elapsed = None
    provider_stats = None
    discovered = []
    next_discovery = 0
    protocol = getattr(options, "protocol", "binary")
    # src/output.rs MAX_EVENT includes the newline; adapter events are smaller.
    event_limit = 1024 * 1024 - 1 if getattr(options, "driver", None) == "daemon" else 65536
    driver = writer = workspace = None
    outgoing = None

    def write_requests():
        # The daemon reads stdin concurrently with writing stdout; a separate
        # writer keeps the observer loop from blocking on a full pipe.
        while True:
            lines = outgoing.get()
            if lines is None:
                return
            try:
                target.stdin.write("".join(lines).encode())
                target.stdin.flush()
            except (OSError, ValueError):
                return

    def sample(handle, force_discovery=False):
        nonlocal sample_count, target_sample_count, sample_wall, counters, observer_peak_rss
        nonlocal discovered, next_discovery
        before = time.monotonic()
        if force_discovery or before >= next_discovery:
            discovered = snapshot((target_tree, provider_tree))
            next_discovery = time.monotonic() + options.discovery_interval
        counters = {"target": target_tree.sample(discovered),
                    "provider": provider_tree.sample(discovered)}
        sample_wall += time.monotonic() - before
        observer_peak_rss = max(observer_peak_rss, psutil.Process().memory_info().rss)
        sample_count += 1
        target_sample_count += counters["target"]["processes"] > 0
        for name, values in counters.items():
            if values["unreadable_processes"]:
                return f"{name}_counters_unavailable"
            for field in peaks[name]:
                peaks[name][field] = max(peaks[name][field], values[field])
        handle.write(json.dumps({"elapsed_seconds": time.monotonic() - start,
                                 **counters}) + "\n")
        for name, values in counters.items():
            if values["rss_bytes"] > options.rss_limit_mib * 1024 * 1024:
                return f"{name}_rss_limit"
            if values["processes"] > options.process_limit:
                return f"{name}_process_limit"
        return None

    try:
        provider = subprocess.Popen(
            [sys.executable, "-m", "bench.provider", "--workload", str(workload_path),
             "--stats", str(stats_path), "--protocol", protocol], stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
        provider_tree = Tree(provider.pid)
        port = provider_ready(provider)
        env = {**os.environ, "AGENT_BENCH_PORT": str(port),
               "AGENT_BENCH_WORKLOAD": json.dumps(config, sort_keys=True)}
        if protocol != "binary":
            state = (directory / f"state-{index}").resolve()
            for name in ("home", "codex", "claude"):
                (state / name).mkdir(parents=True)
            # The workspace lives outside this repository: engines that probe
            # git from their working directory would otherwise walk up into
            # the checkout and charge its status and history to the target.
            workspace = Path(tempfile.mkdtemp(prefix="agent-bench-workspace-"))
            env = {**clean_env(), "HOME": str(state / "home"),
                   "CODEX_HOME": str(state / "codex"),
                   "CLAUDE_CONFIG_DIR": str(state / "claude"),
                   "AGENT_BENCH_STATE": str(state),
                   "AGENT_BENCH_WORKSPACE": str(workspace),
                   "AGENT_BENCH_PORT": str(port),
                   "AGENT_BENCH_WORKLOAD": json.dumps(config, sort_keys=True)}
            if getattr(options, "engine_executable", None):
                env["AGENT_BENCH_EXECUTABLE"] = options.engine_executable
        if getattr(options, "driver", None) == "daemon":
            # The real service surface: the daemon is the target, the observer
            # drives its stdio protocol and translates its events.
            driver = DaemonDriver(config, workspace)
            command = [*command, "--store", str(state / "state.sqlite"),
                       "--provider", f"openai=responses,http://127.0.0.1:{port}/v1"]
        start = time.monotonic()
        target = subprocess.Popen(command, stdin=subprocess.PIPE if driver else subprocess.DEVNULL,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  env=env, start_new_session=True)
        if driver:
            import queue
            outgoing = queue.Queue()
            writer = threading.Thread(target=write_requests, daemon=True)
            writer.start()
        target_tree = Tree(target.pid)
        buffer = b""
        eof = False
        next_sample = start
        with (selectors.DefaultSelector() as selector,
              (directory / f"samples-{index}.jsonl").open("w") as samples):
            selector.register(target.stdout, selectors.EVENT_READ)
            while True:
                now = time.monotonic()
                if now - start >= options.timeout:
                    status = "timeout"
                    break
                if provider.poll() is not None:
                    status = "provider_exited"
                    break
                if now >= next_sample:
                    limit = sample(samples)
                    next_sample = time.monotonic() + options.interval
                    if limit:
                        status = limit
                        break
                exit_code = target.poll()
                if exit_code is not None and eof:
                    elapsed = time.monotonic() - start
                    sample(samples, force_discovery=True)
                    if exit_code:
                        status = "target_failed"
                    elif buffer:
                        status = "unterminated_event"
                    elif counters["target"]["processes"]:
                        status = "descendants_after_exit"
                    elif target_sample_count < 2:
                        status = "insufficient_samples"
                    elif driver and driver.failed:
                        status = "target_failed"
                    else:
                        event_summary = events.finish()
                        status = "ok"
                    break
                wait = max(0, min(next_sample, start + options.timeout) - time.monotonic())
                for key, _ in selector.select(timeout=wait):
                    chunk = os.read(key.fileobj.fileno(), 16384)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        eof = True
                        continue
                    buffer += chunk
                    while b"\n" in buffer:
                        line, buffer = buffer.split(b"\n", 1)
                        if len(line) > event_limit:
                            raise ValueError("event line exceeds protocol limit")
                        now = time.monotonic() - start
                        if driver:
                            for event in driver.handle(json.loads(line)):
                                events.add(event, now)
                            lines = driver.drain()
                            if lines:
                                outgoing.put(lines)
                        else:
                            events.add(json.loads(line), now)
                    if len(buffer) > event_limit:
                        raise ValueError("event line exceeds protocol limit")
        if elapsed is None:
            elapsed = time.monotonic() - start
    except KeyboardInterrupt:
        status = "interrupted"
    except (OSError, ValueError, RuntimeError, psutil.Error, subprocess.SubprocessError) as error:
        # Never retain raw stdout, stderr, arguments, prompts, or exception text
        # that may contain target content. Only the error category is stored.
        status = "protocol_error" if isinstance(error, ValueError) else "runner_error"
        detail = type(error).__name__
    finally:
        if start is not None and elapsed is None:
            elapsed = time.monotonic() - start
        if writer is not None:
            outgoing.put(None)
            writer.join(timeout=1)
        for process, tree in ((target, target_tree), (provider, provider_tree)):
            try:
                stop(process, tree)
            except (OSError, psutil.Error, subprocess.SubprocessError) as error:
                status = "cleanup_failed"
                detail = type(error).__name__
        if workspace is not None:
            shutil.rmtree(workspace, ignore_errors=True)
        if stats_path.exists():
            provider_stats = json.loads(stats_path.read_text())
    if status == "ok":
        expected = config["concurrency"] * config["turns"]
        if (provider_stats is None or provider_stats["completed_requests"] != expected
                or provider_stats["requests"] != expected
                or provider_stats["aborted_requests"]
                or provider_stats["invalid_requests"]
                or provider_stats["output_text_bytes"] != event_summary["stream_payload_bytes"]):
            status = "provider_workload_mismatch"
    for name in peaks:
        peaks[name]["observed_cpu_seconds"] = counters.get(name, {}).get("observed_cpu_seconds")
    warnings = []
    if elapsed and sample_wall / elapsed > .1:
        warnings.append("sampler used more than 10% of target wall time")
    if provider_stats and provider_stats["peak_active_requests"] < config["concurrency"]:
        warnings.append("configured concurrency was not achieved at the provider")
    return {"status": status, "error_type": detail, "diagnostic": events.diagnostic, "exit_code": exit_code,
            "wall_seconds": elapsed, "target": peaks["target"],
            "provider_resources": peaks["provider"], "provider": provider_stats,
            "events": event_summary, "samples": sample_count,
            "observer": {"self_cpu_seconds": time.process_time() - cpu_start,
                         "peak_sampled_rss_bytes": observer_peak_rss,
                         "wall_seconds_including_setup_cleanup": time.monotonic() - observer_start,
                         "sampler_wall_seconds": sample_wall},
            "quality_warnings": warnings,
            "unavailable": ["private_memory", "pss", "wire_bytes", "allocation_count",
                            "exact_process_tree_cpu", "admission_latency", "cancellation_latency"]}
