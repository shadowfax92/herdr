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
    (second / "b").write_text("A")
    (root / "alias").symlink_to(first, target_is_directory=True)
    real_rsync = shutil.which("rsync")
    assert real_rsync
    (fake / "ssh").write_text("""#!/usr/bin/env python3
import os,subprocess,sys
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
    assert (payload / "first/a").read_text() == "B", "failure injection did not follow a partial transfer"
    (first / "a").write_text("A")
    (second / "b").write_text("A")
    result = invoke("sync", "fixture", "--json")
    assert result.returncode == 0 and json.loads(result.stdout)['changed']
    assert (payload / "first/a").read_text() == "A"
    assert (payload / "second/b").read_text() == "A"
    assert (payload / "manifest.json").exists()
    print("PASS: disabled parity, unchanged sync, source symlink, failed publication, reverted payload repair")
