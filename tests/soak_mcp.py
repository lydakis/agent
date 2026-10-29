"""Soak of Sonny's MCP host (bench only): many exec scripts through one
lookout, its RSS sampled, servers counted, a hard kill checked.

    AGENT_TEST_RUNTIME=1 SOAK_N=300 python -m unittest tests.soak_mcp -k test_soak
"""
import json
import os
import subprocess
import sys
import time
from pathlib import Path

from bench.targets import clean_env
from tests.test_served import ServedToolTests


def rss_mb(pid):
    out = subprocess.run(['ps', '-o', 'rss=', '-p', str(pid)], capture_output=True, text=True).stdout.strip()
    return int(out) // 1024 if out else None


def alive(pid):
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    state = subprocess.run(['ps', '-o', 'stat=', '-p', str(pid)], capture_output=True, text=True).stdout
    return bool(state.strip()) and not state.strip().startswith('Z')


class Soak(ServedToolTests):
    def test_soak(self):
        server = Path(__file__).with_name('mcp_server.py')
        (self.path / '.agent').mkdir(exist_ok=True)
        (self.path / '.agent/config.toml').write_text(
            f'[sonny.mcp_servers.test]\ncommand = {json.dumps(sys.executable)}\nargs = [{json.dumps(str(server))}]\n'
            f'env = {{ MCP_STARTS = {json.dumps(str(self.path / "starts"))} }}\n')
        script = json.loads(subprocess.run([str(self.sonny), 'mcp', 'tool'], env={**clean_env(), 'HOME': str(self.path)},
                                           check=True, capture_output=True, text=True).stdout)
        self.daemon.request('create', bot='calc.lead', workspace=str(self.path), model='openai/synthetic-model',
                            instructions='Lead.', declare=[script])
        service = self.lookout()
        body = ('const big = "x".repeat(10000);\n'
                'const rs = await Promise.all(Array.from({length: 20}, (_, i) => tools.mcp__test__echo({text: big + i})));\n'
                'let junk = []; for (let i = 0; i < 200000; i++) junk.push({i, s: "y" + i});\n'
                'text(rs.length + " " + junk.length + " " + "z".repeat(60000));')
        n = int(os.environ.get('SOAK_N', '300'))
        started = time.monotonic()
        for i in range(n):
            self.assertIn('Script completed', self.call('calc.lead', 'exec', body))
            if i % 25 == 0 or i == n - 1:
                print(f'run {i + 1}: lookout {rss_mb(service.pid)} MB, {time.monotonic() - started:.0f}s', flush=True)
        for cap in range(5):
            capped = self.call('calc.lead', 'exec', 'let a = []; while (true) a.push("q".repeat(1e6));')
            print(f'256 MB cap {cap + 1}: lookout {rss_mb(service.pid)} MB;', capped[:60].replace('\n', ' | '))
        for i in range(25):
            self.call('calc.lead', 'exec', body)
        time.sleep(1)
        print(f'25 scripts after the caps: lookout {rss_mb(service.pid)} MB')
        starts = (self.path / 'starts').read_text().split()
        print('server starts:', len(starts))
        saved = self.path / '.agent/sonny/exec/output'
        files = list(saved.iterdir()) if saved.exists() else []
        print('saved outputs:', len(files), sum(f.stat().st_size for f in files) // 1024, 'KB')
        server_pid = int(starts[-1])
        service.kill()
        service.wait()
        time.sleep(2)
        print('server alive after the lookout is SIGKILLed:', alive(server_pid))
