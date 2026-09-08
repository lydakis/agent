import hashlib
import json
from pathlib import Path


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"))
                          .encode()).hexdigest()


def workload(path):
    data = json.loads(Path(path).read_text())
    bounds = {"version": (1, 1), "concurrency": (1, 4096), "turns": (1, 10000),
              "chunks": (1, 10000), "chunk_bytes": (1, 1048576),
              "chunk_delay_ms": (0, 1000), "history_bytes": (0, 1048576)}
    if not isinstance(data, dict) or data.keys() != bounds.keys():
        raise ValueError("workload must contain exactly the documented fields")
    for key, (low, high) in bounds.items():
        if type(data[key]) is not int or not low <= data[key] <= high:
            raise ValueError(f"workload field out of range: {key}")
    turns = data["concurrency"] * data["turns"]
    chunks = turns * data["chunks"]
    if (turns > 100000 or chunks > 1000000
            or chunks * data["chunk_bytes"] > 512 * 1024 * 1024
            or turns * data["history_bytes"] > 512 * 1024 * 1024):
        raise ValueError("workload exceeds bounded fixture limits")
    return data
