"""Attribute storage at fixed turn counts with retention enabled, no paid calls.

Eight shell bots emit a 1 MiB varied diagnostic fixture; eight text bots get
4 KiB prompts. Stop the daemon before profiling each boundary, then resume
the same identities. This measures storage growth, not uninterrupted soak
latency. Original history is never pruned or compressed.
"""
import argparse
import json
from pathlib import Path

from .runtime_client import Client
from .storage_corpus import synthetic
from .storage_profile import profile
from .synthetic_model import start
from .targets import file_hash


def run(binary, out, boundaries):
    out.mkdir(parents=True, exist_ok=False)
    store = out / 'state.sqlite'
    data = next(data for _, data in synthetic('varied', 4) if len(data) == 1048576)
    (out / 'output.txt').write_bytes(data)
    server, url = start()
    client = None
    completed, samples = 0, []
    try:
        for boundary in boundaries:
            client = Client(binary, store, url, 'shell', extra=(
                '--retain-turns', '4', '--context-bytes', '524288'))
            if completed == 0:
                for n in range(16):
                    response = client.request('create', bot=f'b{n}', workspace=str(out))
                    assert 'result' in response, response
            while completed < boundary:
                pending = []
                for n in range(min(16, boundary - completed)):
                    k = completed + n
                    bot = f'b{k % 16}'
                    prompt = (f'shell:cat output.txt # {k}' if k % 16 < 8
                              else f'Turn {k}: ' + 'Keep the public interface stable.\n' * 128)
                    response = client.request('submit', bot=bot, request_id=f't{k}', prompt=prompt)
                    assert 'result' in response, response
                    pending.append(response['result']['turn'])
                for turn in pending:
                    result = client.finished(turn)
                    assert result['data']['status'] == 'completed', result
                completed += len(pending)
                client.saved.clear()
            client.close()
            client = None
            sample = profile(store)
            sample['completed_turns'] = completed
            samples.append(sample)
            print(json.dumps({key: sample[key] for key in ('completed_turns', 'database_bytes',
                                                         'payload_bytes', 'duplicate_prompt_bytes')}), flush=True)
        result = {'schema': 'storage_growth_v1', 'binary_sha256': file_hash(binary),
                  'bots': 16, 'retain_turns': 4, 'samples': samples}
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        return result
    finally:
        if client:
            client.close(kill=True)
        server.shutdown()
        server.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--turns', type=int, nargs='+', default=[64, 256, 1024])
    args = parser.parse_args()
    if args.turns[0] < 1 or any(a >= b for a, b in zip(args.turns, args.turns[1:])):
        parser.error('--turns must be positive and strictly increasing')
    run(args.binary.resolve(), args.out.resolve(), args.turns)


if __name__ == '__main__':
    main()
