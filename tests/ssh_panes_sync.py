#!/usr/bin/env python3
"""Exercise the CLI/sync process boundary with fake SSH and real local rsync.

Run: python3 tests/ssh_panes_sync.py target/debug/herdr
No server or network is used. Each subprocess gets an isolated config and HOME;
failures after the first mapping must revoke the old publication marker.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import signal
import socket
import threading
import time

binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="herdr-sp-test-") as temp:
    root = Path(temp)
    fake = root / "bin"
    fake.mkdir()
    remote_home = root / "remote"
    remote_home.mkdir()
    first, second = root / "first", root / "second"
    first.mkdir()
    second.mkdir()
    (first / "a").write_text("A")
    (root / "target").write_text("symlink bytes")
    (first / "link").symlink_to(root / "target")
    (second / "b").write_text("A")
    (root / "alias").symlink_to(first, target_is_directory=True)
    real_rsync = shutil.which("rsync")
    assert real_rsync
    (fake / "ssh").write_text("""#!/usr/bin/env python3
import os,subprocess,sys,json,socket,time,re
if '-t' in sys.argv and 'remote-attach' in sys.argv[-1]:
    spec=json.loads(bytes.fromhex(re.search(r'--spec ([0-9a-f]+)',sys.argv[-1]).group(1)))
    if os.environ.get('HERDR_TEST_REPORT'):
        peer=socket.socket(socket.AF_UNIX);peer.connect(os.environ['HERDR_SOCKET_PATH'])
        peer.sendall((json.dumps({'id':'report','method':'ssh_panes','params':{'op':'report','link_id':spec['link']}})+'\\n').encode())
        peer.recv(4096);peer.close()
    code=int(os.environ.get('HERDR_TEST_SHELL_EXIT','0'))
    prefix='\\x1b]777;herdr-sp-'+spec['link']+';'
    if os.environ.get('HERDR_TEST_NO_MARKERS'):
        sys.exit(code)
    print(prefix+('ready' if os.environ.get('HERDR_TEST_REPORT') else 'stale')+'\\x07',end='',flush=True)
    if not os.environ.get('HERDR_TEST_TRANSPORT_FAILURE'):
        print(prefix+'done;'+str(code)+'\\x07',end='',flush=True)
    sys.exit(code)
if os.environ.get('HERDR_TEST_SYNC_WAIT'):
    open(os.environ['HERDR_TEST_SYNC_MARKER'],'w').close()
    time.sleep(30)
assert sys.argv[-2] == 'fixture', sys.argv
os.environ['HOME'] = os.environ['HERDR_TEST_REMOTE_HOME']
sys.exit(subprocess.call(['/bin/bash','-c',sys.argv[-1]]))
""")
    (fake / "rsync").write_text("""#!/usr/bin/env python3
import os,subprocess,sys
args=sys.argv[1:]; source,target=args[-2:]
assert target.startswith('fixture:'), target
os.environ['HOME']=os.environ['HERDR_TEST_REMOTE_HOME']
target=subprocess.check_output(['/bin/bash','-c',"printf '%s' "+target.split(':',1)[1]],text=True)
if os.environ.get('HERDR_TEST_FAIL_DEST') and target.endswith('/second/'):
    print('injected second mapping failure',file=sys.stderr);sys.exit(23)
flags=args[:-2];i=flags.index('-e');del flags[i:i+2]
sys.exit(subprocess.call([os.environ['HERDR_TEST_RSYNC']]+flags+[source,target]))
""")
    for path in fake.iterdir():
        path.chmod(0o755)
    config = root / "config.toml"
    config.write_text(f'''[ssh_panes]
enabled = true
remote_root = "~/.local/share/herdr-ssh-panes-dev"
[ssh_panes.sync]
paths = [{{src = "{root / 'alias'}", dest = "first"}}, {{src = "{second}", dest = "second"}}]
''')
    env = dict(os.environ)
    for name in ["HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH", "HERDR_SESSION", "HERDR_PANE_ID"]:
        env.pop(name, None)
    env.update(PATH=str(fake) + os.pathsep + env["PATH"], HERDR_CONFIG_PATH=str(config), XDG_CONFIG_HOME=str(root / "config"), HERDR_TEST_REMOTE_HOME=str(remote_home), HERDR_TEST_RSYNC=real_rsync)
    def invoke(*args, fail=False):
        call_env = dict(env)
        if fail:
            call_env['HERDR_TEST_FAIL_DEST'] = '1'
        result = subprocess.run([str(binary), "ssh", *args], env=call_env, text=True, capture_output=True, timeout=60)
        print(json.dumps(dict(args=args, code=result.returncode, out=result.stdout, err=result.stderr)))
        return result
    disabled = root / "disabled.toml"
    disabled.write_text("")
    disabled_env = dict(env, HERDR_CONFIG_PATH=str(disabled))
    result = subprocess.run([str(binary), "ssh", "fixture"], env=disabled_env, text=True, capture_output=True)
    assert result.returncode == 2 and not result.stdout and "SSH panes are disabled" in result.stderr
    for args, expected in [(["list", "--json"], '{"enabled":false,"links":[]}\n'), (["list"], "SSH panes are disabled\n")]:
        result = subprocess.run([str(binary), "ssh", *args], env=disabled_env, text=True, capture_output=True)
        assert result.returncode == 0 and result.stdout == expected and not result.stderr
    result = subprocess.run([str(binary), "--help"], env=disabled_env, text=True, capture_output=True)
    assert result.returncode == 0 and "herdr ssh " in result.stdout
    result = invoke("sync", "fixture", "--json")
    assert result.returncode == 0, result.stderr
    result = invoke("sync", "fixture", "--json")
    assert result.returncode == 0 and not json.loads(result.stdout)['changed']
    (first / "a").write_text("B")
    (second / "b").write_text("B")
    result = invoke("sync", "fixture", "--json", fail=True)
    assert result.returncode == 1 and "injected" in result.stderr
    payload = remote_home / ".local/share/herdr-ssh-panes-dev"
    assert not (payload / "manifest.json").exists(), "failed sync left an authorized marker"
    assert (payload / "first/link").is_file() and not (payload / "first/link").is_symlink()
    assert (payload / "first/link").read_text() == "symlink bytes"
    assert (payload / "first/a").read_text() == "B", "failure injection did not follow a partial transfer"
    (first / "a").write_text("A")
    (second / "b").write_text("A")
    result = invoke("sync", "fixture", "--json")
    assert result.returncode == 0 and json.loads(result.stdout)['changed']
    assert (payload / "first/a").read_text() == "A"
    assert (payload / "second/b").read_text() == "A"
    assert (payload / "manifest.json").exists()
    # Home-relative mappings may copy into existing user directories but never
    # erase their unrelated files. Root-relative mappings still mirror deletions.
    home_config = remote_home / ".config/nvim"
    home_config.mkdir(parents=True)
    (home_config / "keep").write_text("remote only")
    original_config = config.read_text()
    config.write_text(original_config.replace('dest = "first"', 'dest = "~/.config/nvim"'))
    result = invoke("sync", "fixture", "--json")
    assert result.returncode == 0 and "without --delete" in result.stderr
    assert (home_config / "keep").read_text() == "remote only"
    config.write_text(original_config)
    for bad_root in ["", "~", "~/", "/", "relative", "/tmp/../"]:
        config.write_text(original_config.replace('~/.local/share/herdr-ssh-panes-dev', bad_root))
        result = invoke("sync", "fixture", "--json")
        assert result.returncode == 2 and "remote_root" in result.stderr
    config.write_text(original_config)

    # A real JSON-line API peer separates accepted remote sessions from pre-exec
    # stale-payload statuses, and simulates a server that starts serving late.
    class ApiPeer:
        def __init__(self, delay=0, path=None, reject_first=False, fail_list=False):
            self.path = path or root / "api.sock"
            self.path.unlink(missing_ok=True)
            self.sock = socket.socket(socket.AF_UNIX)
            self.sock.bind(str(self.path)); self.sock.listen(); self.sock.settimeout(.1)
            self.links = {}; self.attaches = 0; self.delay = delay; self.stop = False
            self.report_times = []; self.reject_first = reject_first; self.fail_list = fail_list
            self.thread = threading.Thread(target=self.serve, daemon=True); self.thread.start()
        def serve(self):
            while not self.stop:
                try:
                    conn, _ = self.sock.accept()
                except socket.timeout:
                    continue
                with conn:
                    req = json.loads(conn.makefile('rb').readline())
                    params = req['params']; op = params['op']
                    if op == 'attach':
                        self.attaches += 1
                        self.links[params['link_id']] = None
                        if self.attaches == 1:
                            time.sleep(self.delay)
                        result = {}
                    elif op == 'report':
                        self.links[params['link_id']] = 0
                        self.report_times.append(time.monotonic())
                        result = {'accepted': not (self.reject_first and len(self.report_times) == 1)}
                    elif op == 'list':
                        if self.fail_list: continue
                        result = {'links': [{'link_id': k, 'last_report_ms_ago': v} for k, v in self.links.items()]}
                    else:
                        self.links.pop(params['link_id'], None); result = {}
                    try:
                        conn.sendall((json.dumps({'id': req['id'], 'result': {'type':'ssh_panes','ssh_panes':result}})+'\n').encode())
                    except BrokenPipeError:
                        pass
        def close(self):
            self.stop = True; self.thread.join(3); self.sock.close(); self.path.unlink(missing_ok=True)
    reset = "\x1b[?1049l\x1b[<u\x1b[?1000;1002;1003;1006l\x1b[?2004l\x1b[?25h"
    for code in [3, 127, 255]:
        peer = ApiPeer()
        run_env = dict(env, HERDR_SOCKET_PATH=str(peer.path), HERDR_PANE_ID='w1:p1', HERDR_TEST_REPORT='1', HERDR_TEST_SHELL_EXIT=str(code))
        result = subprocess.run([str(binary), 'ssh', 'fixture'], env=run_env, text=True, capture_output=True, timeout=15)
        assert result.returncode == code and peer.attaches == 1, result
        assert 'stale' not in result.stderr and 'lost connection' not in result.stderr, result.stderr
        assert result.stdout == reset, repr(result.stdout)
        peer.close()
    peer = ApiPeer()
    run_env = dict(env, HERDR_SOCKET_PATH=str(peer.path), HERDR_PANE_ID='w1:p1', HERDR_TEST_REPORT='1', HERDR_TEST_SHELL_EXIT='255', HERDR_TEST_TRANSPORT_FAILURE='1', HERDR_SSH_PANES_SPAWNED='1')
    result = subprocess.run([str(binary), 'ssh', 'fixture'], input='\n', env=run_env, text=True, capture_output=True, timeout=15)
    assert result.returncode == 255 and '[r] reconnect' in result.stderr and 'lost connection' in result.stderr, result
    peer.close()
    for code in [3, 127]:
        peer = ApiPeer()
        run_env = dict(env, HERDR_SOCKET_PATH=str(peer.path), HERDR_PANE_ID='w1:p1', HERDR_TEST_SHELL_EXIT=str(code))
        result = subprocess.run([str(binary), 'ssh', 'fixture'], env=run_env, text=True, capture_output=True, timeout=30)
        assert result.returncode == code and peer.attaches == 2 and 'stale' in result.stderr, result
        peer.close()
    peer = ApiPeer(fail_list=True)
    run_env = dict(env, HERDR_SOCKET_PATH=str(peer.path), HERDR_PANE_ID='w1:p1', HERDR_TEST_SHELL_EXIT='3', HERDR_TEST_NO_MARKERS='1')
    result = subprocess.run([str(binary), 'ssh', 'fixture'], env=run_env, text=True, capture_output=True, timeout=15)
    assert result.returncode == 3 and peer.attaches == 1 and 'stale' not in result.stderr, result
    peer.close()
    peer = ApiPeer(delay=2)
    run_env = dict(env, HERDR_SOCKET_PATH=str(peer.path), HERDR_PANE_ID='w1:p1', HERDR_SSH_PANES_SPAWNED='1')
    result = subprocess.run([str(binary), 'ssh', 'fixture'], env=run_env, text=True, capture_output=True, timeout=15)
    assert result.returncode == 0 and peer.attaches >= 2, result
    peer.close()

    # Watchers must recover from a rejected report within five seconds, while
    # still noticing that their shell died during the backoff.
    link = os.urandom(12).hex()
    peer = ApiPeer(path=Path('/tmp/herdr-sp-'+link+'.sock'), reject_first=True)
    shell = subprocess.Popen(['/bin/sleep', '15'])
    state_dir = remote_home / '.local/state' / binary.parent.parent.name / 's/0123456789abcdef'
    (state_dir / 'links').mkdir(parents=True, exist_ok=True)
    (state_dir / 'links' / (link+'.json')).write_text(json.dumps(dict(socket=str(peer.path), shell_pid=shell.pid, started_at=1)))
    watcher = subprocess.Popen([str(binary), 'ssh', 'remote-watch', '--link', link, '--shell-pid', str(shell.pid), '--server-key', '0123456789abcdef', '--socket', str(peer.path)], env=dict(env, HOME=str(remote_home)), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        deadline = time.monotonic()+8
        while len(peer.report_times) < 2 and time.monotonic() < deadline:
            time.sleep(.05)
        assert len(peer.report_times) >= 2 and peer.report_times[1]-peer.report_times[0] <= 5, peer.report_times
        deadline = time.monotonic()+2
        while not (state_dir/'herdr.sock').is_symlink() and time.monotonic()<deadline: time.sleep(.05)
        assert (state_dir/'herdr.sock').is_symlink(), 'successful reports did not repair scratch routing'
        shell.terminate(); shell.wait(timeout=3)
        assert watcher.wait(timeout=3) == 0
    finally:
        if shell.poll() is None: shell.kill(); shell.wait()
        if watcher.poll() is None: watcher.kill(); watcher.wait()
        peer.close()

    # Ctrl-C reaches sync SSH even when the invoking process inherited SIG_IGN.
    def ignored_interrupts():
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGQUIT, signal.SIG_IGN)
    proc = subprocess.Popen([str(binary), 'ssh', 'sync', 'fixture', '--force'], env=dict(env, HERDR_TEST_SYNC_WAIT='1', HERDR_TEST_SYNC_MARKER=str(root/'sync-ready')), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True, preexec_fn=ignored_interrupts)
    started = time.monotonic()
    # A file marker observes readiness without depending on captured SSH stderr.
    while not (root/'sync-ready').exists() and time.monotonic()-started < 10:
        time.sleep(.05)
    assert (root/'sync-ready').exists(), 'sync child did not start'
    os.killpg(proc.pid, signal.SIGINT)
    proc.communicate(timeout=5)
    assert proc.returncode != 0 and time.monotonic()-started < 10
    print("PASS: exit statuses 3/127/255, stale retry, startup retry, interruptible sync, deletion guard, invalid roots, disabled list/help; ")
    print("PASS: disabled parity, unchanged sync, source symlink, failed publication, reverted payload repair")
