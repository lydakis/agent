import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import sys

import psutil

from .config import digest, workload
from .processes import Tree, snapshot
from .report import compare
from .runner import run_once
from .targets import engine_target, validate_responses_workload
from .profiles import profile


def bounded_float(low, high):
    def parse(text):
        value = float(text)
        if not low <= value <= high:
            raise argparse.ArgumentTypeError(f"must be between {low} and {high}")
        return value
    return parse


def bounded_int(low, high):
    def parse(text):
        value = int(text)
        if not low <= value <= high:
            raise argparse.ArgumentTypeError(f"must be between {low} and {high}")
        return value
    return parse


def run(args):
    root = Path(__file__).resolve().parent.parent
    if Path.cwd() != root:
        raise ValueError("run benchmarks from the repository root")
    output = args.out.resolve()
    if not output.is_relative_to(root / ".local"):
        raise ValueError("benchmark captures must be under the ignored .local directory")
    config = workload(args.workload)
    if args.binary and args.engine != 'rust':
        raise ValueError('--binary is only supported for Rust regression captures')
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if command and not args.revision:
        raise ValueError("custom targets require --revision for provenance")
    source_paths = []
    for folder, directories, files in os.walk(root / "bench"):
        directories[:] = [name for name in directories if name not in ("node_modules", "__pycache__")]
        source_paths.extend(Path(folder) / name for name in files
                            if Path(name).suffix in (".py", ".mjs", ".json", ".yaml", ".txt"))
    sources = {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
               for path in sorted(source_paths)}
    code_hash = digest(sources)
    revision = args.revision or code_hash
    command = command or [sys.executable, "-m", "bench.fixture"]
    metadata = {"engine": "fixture"}
    if not args.command:
        metadata['comparison_profile'] = profile('fixture')
    args.codex_executable = None
    args.protocol = "binary"
    args.driver = "daemon" if args.engine == "rust" else None
    if args.engine != "fixture":
        if args.command or args.revision:
            raise ValueError("engine adapters resolve their own command and revision")
        validate_responses_workload(config)
        args.protocol = "gateway" if args.engine == 'fx' else "responses"
        command, metadata, args.codex_executable = engine_target(args.engine, root, args.binary)
        revision = digest(metadata)
    probe = Tree(os.getpid()).sample(snapshot())
    if probe["unreadable_processes"] or not probe["processes"]:
        raise RuntimeError("process counters unavailable in this environment")
    battery = psutil.sensors_battery()
    result = {
        "schema": 1, "label": args.label or args.engine, "target_revision": revision,
        "target_metadata": metadata,
        "target_command_sha256": digest(command),
        "created_at": datetime.now(timezone.utc).isoformat(),
        "workload": config,
        "compatibility": {
            "workload_sha256": digest(config), "observer_sha256": code_hash,
            "python": platform.python_version(), "psutil": psutil.__version__,
            "host_id": digest(platform.node()), "system": platform.system(),
            "release": platform.release(), "architecture": platform.machine(),
            "logical_cpus": os.cpu_count(), "ram_bytes": psutil.virtual_memory().total,
            "on_external_power": battery.power_plugged if battery else None,
            "interval_seconds": args.interval, "timeout_seconds": args.timeout,
            "discovery_interval_seconds": args.discovery_interval,
            "rss_limit_mib": args.rss_limit_mib, "process_limit": args.process_limit,
            "warmup_runs": args.warmup,
            "provider_protocol": args.protocol,
            "event_protocol": "fixture-v1", "memory_metric": "summed_sampled_rss"},
        "runs": [],
    }
    output.mkdir(parents=True, exist_ok=False)
    (output / "workload.json").write_text(json.dumps(config, indent=2) + "\n")
    for index in range(args.warmup + args.repeat):
        record = run_once(command, config, args, output, index)
        record["warmup"] = index < args.warmup
        result["runs"].append(record)
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"run {index + 1}: {record['status']}"
              + (" (warmup)" if record["warmup"] else ""), file=sys.stderr)
        if record["status"] != "ok":
            print(f"Partial result: {output / 'result.json'}", file=sys.stderr)
            return 1
    print(output / "result.json")
    return 0


def main():
    parser = argparse.ArgumentParser(description="Agent workload measurement tools")
    sub = parser.add_subparsers(dest="action", required=True)
    run_parser = sub.add_parser("run", help="measure the synthetic client or a fixture adapter")
    run_parser.add_argument("--workload", type=Path, default=Path("bench/workloads/smoke.json"))
    run_parser.add_argument("--out", type=Path, required=True)
    run_parser.add_argument("--label")
    run_parser.add_argument("--engine", choices=("fixture", "pi", "codex", "rust", "fx"), default="fixture")
    run_parser.add_argument("--revision")
    run_parser.add_argument('--binary',type=Path,help='explicit Rust binary; historical Cargo.lock is unknown')
    run_parser.add_argument("--repeat", type=bounded_int(1, 30), default=3)
    run_parser.add_argument("--warmup", type=bounded_int(0, 5), default=1)
    run_parser.add_argument("--interval", type=bounded_float(0.02, 5), default=0.1)
    run_parser.add_argument("--discovery-interval", type=bounded_float(0.02, 5), default=1)
    run_parser.add_argument("--timeout", type=bounded_float(0.1, 300), default=30)
    run_parser.add_argument("--rss-limit-mib", type=bounded_int(1, 65536), default=1024)
    run_parser.add_argument("--process-limit", type=bounded_int(1, 4096), default=64)
    run_parser.add_argument("command", nargs=argparse.REMAINDER)
    comparison = sub.add_parser("compare", help="compare compatible saved runs")
    comparison.add_argument("baseline", type=Path)
    comparison.add_argument("candidate", type=Path)
    comparison.add_argument('--exploratory', action='store_true', help='show unmatched observations without percentage rankings')
    args = parser.parse_args()
    try:
        if args.action == "run":
            return run(args)
        result = compare(json.loads(args.baseline.read_text()),
                         json.loads(args.candidate.read_text()), exploratory=args.exploratory)
        print(json.dumps(result, indent=2))
        return 0
    except (ValueError, OSError, RuntimeError, psutil.Error) as error:
        print(f"bench: {type(error).__name__}: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
