"""Resolve installed engines without reading personal provider configuration."""

import hashlib
import os
import json
import platform
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
        metadata = {"engine": "rust", "comparison_profile": profile(engine), "durability": "sqlite_full", "tools_exercised": False,
                    "binary_sha256": file_hash(executable),
                    "cargo_lock_sha256": None if binary else file_hash(root / "Cargo.lock"),
                    "version": subprocess.check_output([str(executable), "--version"],
                        env=clean_env(), text=True, timeout=5).strip()}
        # The daemon itself; the runner appends the store path and provider
        # endpoint once they exist and drives the stdio protocol.
        return [str(executable), "serve", "--tools", "echo"], metadata, None
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
    elif engine == 'fx':
        package = root / 'bench/adapters/node_modules/libfx'
        if not (package / 'package.json').exists():
            raise ValueError('install the pinned benchmark adapter dependencies first')
        version = json.loads((package / 'package.json').read_text())['version']
        if version != '0.0.8':
            raise ValueError('FX installed version differs from benchmark pin')
        arch = {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'x64', 'AMD64': 'x64'}.get(platform.machine())
        system = {'Darwin': 'darwin', 'Linux': 'linux'}.get(platform.system())
        addon = package / f'libfx.{system}-{arch}.node'
        if not addon.exists():
            raise ValueError('FX pinned native addon unavailable on this platform')
        metadata.update(libfx_version=version, backend='native',
                        release_source_revision='43c11dcc34a94a76df870af70bdb824579bf18a0',
                        native_addon_sha256=file_hash(addon),
                        sdk_sources_sha256={p.name: file_hash(p) for p in sorted(package.glob('*.js'))},
                        dependency_lock_sha256=file_hash(root / 'bench/adapters/pnpm-lock.yaml'))
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
