"""Resolve installed engines without reading personal provider configuration."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
from .profiles import profile


def file_hash(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def clean_env():
    # Do not inherit API credentials, proxies, Node injection, or tracing config.
    return {key: os.environ[key] for key in ("PATH", "TMPDIR", "LANG", "LC_ALL", "SYSTEMROOT")
            if key in os.environ}


def engine_target(engine, root, binary=None):
    if engine == "rust":
        executable = binary.resolve() if binary else root / ".local/target/release/agent"
        if not executable.exists():
            raise ValueError("build the Rust release target with cargo build --release --locked first")
        metadata = {"engine": "rust", "comparison_profile": profile(engine), "durability": "ephemeral", "tools_exercised": False,
                    "binary_sha256": file_hash(executable),
                    "cargo_lock_sha256": None if binary else file_hash(root / "Cargo.lock"),
                    "version": subprocess.check_output([str(executable), "--version"],
                        env=clean_env(), text=True, timeout=5).strip()}
        return [str(executable), "benchmark"], metadata, None
    node = shutil.which("node")
    if not node:
        raise ValueError("Node.js is required for engine adapters")
    metadata = {"engine": engine, "comparison_profile": profile(engine), "node_sha256": file_hash(Path(node).resolve()),
                "node_version": subprocess.check_output([node, "--version"],
                    env=clean_env(), text=True, timeout=5).strip(),
                "durability": "ephemeral", "tools_exercised": False}
    command = [node, str(root / "bench" / "adapters" / f"{engine}.mjs")]
    executable = None
    if engine == "pi":
        import json
        packages = root / "bench/adapters/node_modules/@earendil-works"
        for package in ("pi-agent-core", "pi-ai"):
            path = packages / package / "package.json"
            if not path.exists():
                raise ValueError("install the pinned benchmark adapter dependencies first")
            version = json.loads(path.read_text())["version"]
            if version != "0.85.1":
                raise ValueError("Pi installed version differs from benchmark pin")
            metadata[package] = version
        metadata["dependency_lock_sha256"] = file_hash(root / "bench/adapters/pnpm-lock.yaml")
    elif engine == "codex":
        executable = shutil.which("codex")
        if not executable:
            raise ValueError("Codex is not installed")
        executable = Path(executable).resolve()
        if executable.suffix == ".js":
            # npm packaging: measure the native server directly, no npm launcher.
            candidates = list(executable.parent.parent.glob(
                "node_modules/@openai/codex-*/vendor/*/bin/codex"))
            if len(candidates) != 1:
                raise ValueError("cannot unambiguously locate the native Codex executable")
            executable = candidates[0]
        metadata["codex_sha256"] = file_hash(executable)
        metadata["codex_version"] = subprocess.check_output([str(executable), "--version"],
            env=clean_env(), text=True, stderr=subprocess.DEVNULL, timeout=5).strip()
    return command, metadata, str(executable) if executable else None


def validate_responses_workload(config):
    # Responses includes full history on every turn and repeats final text in
    # terminal events. Bound that cost separately from the binary fixture.
    output = config["chunks"] * config["chunk_bytes"]
    per_turn = config["history_bytes"] + output + 256
    turns = config["turns"]
    if (output > 1024 * 1024 or per_turn * turns > 8 * 1024 * 1024
            or config["concurrency"] * per_turn * turns * (turns + 1) // 2 > 512 * 1024 * 1024):
        raise ValueError("Responses workload exceeds full-history bounds")
