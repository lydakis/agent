"""Sonny in a pty on a daemon whose answers are held 5 s: what the screen shows
while the person types (docs/sonny/keys-held-daemon*.png).

    python3 docs/demo/held/drive.py SONNY AGENT PREFIX
    python3 docs/demo/held/render.py PREFIX-more   # its screen as HTML
    node docs/demo/held/shot.js PREFIX-more        # and as a PNG, with Playwright

Writes PREFIX-typed, -sent, -more and -after as text and pyte cells (JSON).
Needs pyte. A scripted model streams a long reply; slow.py sits between
Sonny and its daemon and holds every answer while the file `hold` exists.
"""
import os, pty, sys, time, select, json, subprocess, tempfile, shutil
import pyte
sonny, agent, prefix = sys.argv[1], sys.argv[2], sys.argv[3]
here = os.path.dirname(os.path.abspath(__file__))
d = tempfile.mkdtemp(prefix='slowdemo.')
os.makedirs(f'{d}/home/ledger')
open(f'{d}/home/ledger/README.md', 'w').write('# Ledger\n')
procs = []
model = subprocess.Popen([sys.executable, f'{here}/model.py', '0'], stdout=subprocess.PIPE, text=True)
procs.append(model)
port = model.stdout.readline().strip()
env = dict(os.environ, HOME=f'{d}/home', AGENT_STORE=f'{d}/state.sqlite', AGENT_SOCKET=f'{d}/agent.sock',
           AGENT_MODEL='demo/scripted', TERM='xterm-256color', COLORTERM='truecolor', LANG='C.UTF-8')
serve = subprocess.Popen([agent, 'serve', '--store', f'{d}/state.sqlite', '--socket', f'{d}/agent.sock',
                          '--provider', f'demo=responses,http://127.0.0.1:{port}/v1'], env=env,
                         stdout=open(f'{d}/serve.log', 'w'), stderr=subprocess.STDOUT)
procs.append(serve)
while not os.path.exists(f'{d}/agent.sock'):
    time.sleep(.1)
sys.path.insert(0, str(__import__('pathlib').Path(here).parents[2]))
from bench.socket_client import Connection
control = Connection(f'{d}/agent.sock')
made = control.request('create', bot='ledger', workspace=f'{d}/home/ledger', model='demo/scripted')
assert 'error' not in made, made
hold = f'{d}/hold'
slow = subprocess.Popen([sys.executable, f'{here}/slow.py', f'{d}/slow.sock', f'{d}/agent.sock', hold, '5'],
                        stdout=subprocess.PIPE, text=True)
procs.append(slow)
slow.stdout.readline()
COLS, ROWS = 100, 24
wenv = dict(env, AGENT_SOCKET=f'{d}/slow.sock')
class Screen(pyte.Screen):
    def report_device_status(self, *a, **k):
        k.pop('private', None)
        return super().report_device_status(*a, **k)
    def report_device_attributes(self, *a, **k):
        k.pop('private', None)
        return super().report_device_attributes(*a, **k)
    def write_process_input(self, data):
        os.write(fd, data.encode())
screen = Screen(COLS, ROWS); stream = pyte.ByteStream(screen)
pid, fd = pty.fork()
if pid == 0:
    os.chdir(f'{d}/home/ledger')
    os.execve(sonny, [sonny, '--agent', 'ledger', '--no-sound', '--no-layout'], wenv)
import fcntl, termios, struct
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', ROWS, COLS, 0, 0))
def pump(seconds):
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.02)
        if r:
            try: data = os.read(fd, 65536)
            except OSError: return
            # The terminal's colors, as a dark terminal answers.
            if b']11;?' in data:
                os.write(fd, b'\x1b]11;rgb:1f1f/1f1f/2121\x1b\\')
            if b']10;?' in data:
                os.write(fd, b'\x1b]10;rgb:e4e4/e4e4/e4e4\x1b\\')
            stream.feed(data)
def send(s, gap=0.08):
    for ch in s:
        os.write(fd, ch.encode()); pump(gap)
def dump(name):
    name = f'{prefix}-{name}'
    open(f'{name}.txt', 'w').write('\n'.join(screen.display))
    open(f'{name}.json', 'w').write(json.dumps([[ [c.data, c.fg, c.bg, c.bold, c.italics, c.underscore, c.reverse] for c in (screen.buffer[y][x] for x in range(COLS))] for y in range(ROWS)]))
try:
    pump(8)
    send('Count the tallies in the ledger'); send('\r'); pump(4)
    open(hold, 'w').close()
    pump(1)
    send('the third week looks off, start there')
    pump(0.3); dump('typed')
    send('\r'); pump(0.5); dump('sent')
    send('and the refunds too'); pump(0.3); dump('more')
    os.remove(hold)
    pump(12); dump('after')
finally:
    os.kill(pid, 15)
    for p in procs:
        p.terminate()
    shutil.rmtree(d, ignore_errors=True)
