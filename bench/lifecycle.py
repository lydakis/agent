"""Rust-only durable lifecycle measurements. No cross-engine ranking."""
import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import platform
import subprocess
import sys
import threading
import time

import psutil
from .config import digest
from .events import percentiles
from .processes import Tree, snapshot
from .runner import provider_ready, stop
from .runtime_client import Client
from .socket_client import SocketClient
from .targets import clean_env, file_hash
from .responses import prompt


def run_once(binary, directory, config, mode, toolset, transport='stdio', memory_detail=False):
    workload = directory / 'workload.json'
    workload.write_text(json.dumps(config))
    stats_path = directory / 'provider.json'
    provider = subprocess.Popen([sys.executable, '-m', 'bench.lifecycle_provider',
        '--workload', str(workload), '--stats', str(stats_path), '--mode', mode],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        start_new_session=True, env=clean_env())
    provider_tree = Tree(provider.pid)
    clients, trees, errors, samples = [], [], [], []
    done = threading.Event()
    started, observer_cpu = time.monotonic(), time.process_time()
    phase, sample_work = 'startup', 0

    def measure():
        nonlocal sample_work
        next_group_scan = 0
        while not done.is_set():
            before = time.monotonic()
            try:
                current = list(trees)
                rows = snapshot([*current, provider_tree], scan_groups=before >= next_group_scan)
                if before >= next_group_scan:
                    next_group_scan = before + .5
                target = [tree.sample(rows, memory_detail=memory_detail) for tree in current]
                fixture = provider_tree.sample(rows)
                if fixture['unreadable_processes'] or any(r['unreadable_processes'] for r in target):
                    raise RuntimeError('unavailable counters')
                row = {'elapsed':before-started, 'phase':phase,
                       'rss_bytes':sum(r['rss_bytes'] for r in target),
                       'daemon_rss_bytes':sum(r['root_rss_bytes'] for r in target),
                       'pss_bytes':sum(r['pss_bytes'] for r in target) if all(r['pss_bytes'] is not None for r in target) else None,
                       'private_bytes':sum(r['private_bytes'] for r in target) if all(r['private_bytes'] is not None for r in target) else None,
                       'threads':sum(r['threads'] for r in target) if all(r['threads'] is not None for r in target) else None,
                       'cpu_seconds':sum(r['observed_cpu_seconds'] for r in target),
                       'processes':sum(r['processes'] for r in target),
                       'provider_rss_bytes':fixture['rss_bytes'],
                       'provider_cpu_seconds':fixture['observed_cpu_seconds']}
                samples.append(row)
                if max(row['rss_bytes'], row['provider_rss_bytes']) > 512 * 1024**2 or row['processes'] > 48:
                    raise RuntimeError('resource limit')
                if row['elapsed'] > 30:
                    raise TimeoutError('run timeout')
            except Exception as exc:
                errors.append(type(exc).__name__)
                for client, tree in zip(clients, trees):
                    stop(client.process, tree)
                break
            sample_work += time.monotonic()-before
            done.wait(.2)

    sampler = None
    result = {'status':'failed'}
    try:
        url = f'http://127.0.0.1:{provider_ready(provider)}/v1'
        def connect():
            controller = SocketClient if transport == 'socket' else Client
            client = controller(binary, directory/'state.sqlite', url, tools=toolset)
            clients.append(client)
            trees.append(Tree(client.process.pid))
            return client
        client = connect()
        sampler = threading.Thread(target=measure, daemon=True)
        sampler.start()
        time.sleep(.45)
        phase = 'create'
        before = time.monotonic()
        for agent in range(config['concurrency']):
            workspace = directory/f'workspace-{agent}'
            workspace.mkdir()
            assert 'result' in client.request('create', bot=str(agent), workspace=str(workspace))
        create_ms = (time.monotonic()-before)*1000
        phase = 'turns'
        latencies, checkpoints = [], {}
        for turn in range(config['turns']):
            pending = []
            for agent in range(config['concurrency']):
                before = time.monotonic()
                response = client.request('submit', bot=str(agent), request_id=str(turn), prompt=prompt(config,agent,turn))
                pending.append((agent, response['result']['turn'], before))
            for agent, native, before in pending:
                event = client.finished(native)
                assert event['data']['status'] == 'completed'
                latencies.append((event['_received_at']-before)*1000)
                if turn == 0:
                    checkpoints[agent] = event['data']['checkpoint']
        if mode == 'shell':
            for agent in range(config['concurrency']):
                assert (directory/f'workspace-{agent}'/'artifact').read_text() == 'tool-ok'
        phase = 'idle'
        time.sleep(.45)
        pages = {str(agent):client.request('events', bot=str(agent), after=0, limit=256)['result']
                 for agent in range(config['concurrency'])}
        if transport == 'socket':
            client.verify_followers(pages)
        phase = 'restart_resume_replay_fork'
        before = time.monotonic()
        client.close(kill=True)
        client = connect()
        restart_ms = (time.monotonic()-before)*1000
        before = time.monotonic()
        for bot, page in pages.items():
            assert client.request('resume', bot=bot)['result']['status'] == 'completed'
            assert client.request('events', bot=bot, after=0, limit=256)['result'] == page
            for event in page['events']:
                if 'node' in event['data']:
                    assert 'result' in client.request('item', bot=bot, node=event['data']['node'])
            assert client.request('submit', bot=bot, request_id='0', prompt=prompt(config,int(bot),0))['result']['duplicate']
            branch = directory/f'fork-{bot}'
            branch.mkdir()
            fork = client.request('fork', source=bot, checkpoint=checkpoints[int(bot)], bot=f'fork-{bot}', workspace=str(branch))
            assert fork['result']['head'] == checkpoints[int(bot)]
            assert not (branch/'artifact').exists()
        lifecycle_ms = (time.monotonic()-before)*1000
        phase = 'forked_idle'
        if transport == 'socket':
            client.verify_followers(pages)
        time.sleep(.45)
        result = dict(status='ok', create_all_ms=create_ms, restart_ready_ms=restart_ms,
                      resume_replay_item_duplicate_fork_all_ms=lifecycle_ms,
                      turn_ms=percentiles(latencies), completed_turns=len(latencies))
    except Exception as exc:
        result = {'status':'failed','phase':phase,'error_type':type(exc).__name__}
    finally:
        done.set()
        if sampler:
            sampler.join(timeout=5)
        for client, tree in zip(clients, trees):
            stop(client.process, tree)
            client.close()
        stop(provider, provider_tree)
    if errors:
        result = {'status':'failed','phase':phase,'error_type':errors[0]}
    stats = json.loads(stats_path.read_text())
    expected = config['concurrency']*config['turns']
    if (stats['invalid_requests'] or stats['completed_requests'] != expected*(1 if mode == 'text' else 2)
            or stats['tool_results'] != (0 if mode == 'text' else expected)):
        result['status'] = 'provider_workload_mismatch'
    (directory/'samples.json').write_text(json.dumps(samples))
    result.update(provider=stats, target_peak_rss_bytes=max((s['rss_bytes'] for s in samples),default=0),
                  daemon_peak_rss_bytes=max((s['daemon_rss_bytes'] for s in samples),default=0),
                  target_peak_threads=max((s['threads'] for s in samples if s['threads'] is not None),default=None),
                  target_observed_cpu_seconds=max((s['cpu_seconds'] for s in samples),default=0),
                  target_peak_processes=max((s['processes'] for s in samples),default=0),
                  provider_peak_rss_bytes=max((s['provider_rss_bytes'] for s in samples),default=0),
                  phase_peak_rss_bytes={p:max(s['rss_bytes'] for s in samples if s['phase']==p) for p in {s['phase'] for s in samples}},
                  retained_store_bytes=sum(p.stat().st_size for p in directory.glob('state.*')),
                  observer_cpu_seconds=time.process_time()-observer_cpu,
                  sampler_wall_seconds=sample_work, wall_seconds=time.monotonic()-started,
                  quality_warnings=['sampler exceeded 10% of wall time'] if sample_work/(time.monotonic()-started)>.1 else [])
    if stats['peak_active_requests'] < config['concurrency']:
        result['quality_warnings'].append('configured provider concurrency not achieved')
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out',required=True,type=Path)
    parser.add_argument('--binary',type=Path,default=Path('.local/target/release/agent'))
    parser.add_argument('--agents',type=int,choices=(1,8,32),default=8)
    parser.add_argument('--mode',choices=('text','echo','shell'),default='shell')
    parser.add_argument('--tools',choices=('echo','echo,shell','echo,shell,read,write,edit'),default='echo,shell')
    parser.add_argument('--repeat',type=int,choices=range(1,6),default=3)
    parser.add_argument('--transport', choices=('stdio', 'socket'), default='stdio')
    parser.add_argument('--memory-detail', action='store_true', help='sample PSS/USS where supported; adds observer overhead')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    out = args.out.resolve()
    if Path.cwd() != root or not out.is_relative_to(root/'.local'):
        parser.error('run from repository root with a new output under .local')
    if args.mode == 'shell' and 'shell' not in args.tools.split(','):
        parser.error('shell workload requires the shell tool')
    out.mkdir(parents=True,exist_ok=False)
    config = dict(version=1,concurrency=args.agents,turns=3,chunks=20,chunk_bytes=256,
                  chunk_delay_ms=25,history_bytes=4096)
    sources = {str(p.relative_to(root)):file_hash(p) for p in sorted((root/'bench').glob('*.py'))}
    battery = psutil.sensors_battery()
    record = dict(schema='rust_lifecycle_v2',binary_sha256=file_hash(args.binary),
                  created_at=datetime.now(timezone.utc).isoformat(),
                  observer_sha256=digest(sources),workload=config,toolset=args.tools,mode=args.mode,
                  transport=args.transport,followers_per_bot=1 if args.transport == 'socket' else 0,
                  memory_detail=args.memory_detail,
                  contract='sqlite_full; exact resume/replay; historical completed fork; no repeated tools',
                  host=dict(system=platform.system(),architecture=platform.machine(),host_id=digest(platform.node()),
                            python=platform.python_version(),psutil=psutil.__version__,external_power=battery.power_plugged if battery else None),
                  sampling=dict(idle_seconds=.45,interval_seconds=.2,group_discovery_seconds=.5,timeout_seconds=30,rss_limit_mib=512,process_limit=48),runs=[])
    for index in range(args.repeat+1):
        directory = out/f'run-{index}'
        directory.mkdir()
        run = run_once(args.binary.resolve(),directory,config,args.mode,args.tools,args.transport,args.memory_detail)
        run['warmup'] = index == 0
        record['runs'].append(run)
        (out/'result.json').write_text(json.dumps(record,indent=2))
        print(f'run {index}: {run["status"]}',flush=True)
        if run['status'] != 'ok':
            return 1
    return 0

if __name__ == '__main__':
    sys.exit(main())
