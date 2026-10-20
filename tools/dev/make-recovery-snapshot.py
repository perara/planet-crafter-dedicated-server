#!/usr/bin/env python3
"""Makes a recovery snapshot with a given release: runs that release's front and banks on a
save, hot-updates once (the rollback flip to the other bank), kills the live bank with SIGKILL
and stops the front during its recovery. The front then writes the last snapshot of the world
as WORLD.json.snapshot (front/snapshot.rs); that file is the product: a snapshot in the format
of that release, for the tests and checks of what a newer build does with an older release's
snapshot (read it, refuse it with a clear message, or start from the save).

    make-recovery-snapshot.py RELEASE_DIR SAVE.json OUT_SNAPSHOT [--port N]

RELEASE_DIR   an unpacked release (pcds-vX.Y.Z-linux-x86_64/: pcds, libsteam_api.so, web/)
SAVE.json     the world to run (a save the release can load; a new world made by that release's
              `pcds new-save` does)
OUT_SNAPSHOT  where the snapshot is copied to
--port N      the game's direct-IP UDP port; the admin API takes N+1 (default 8428; test ports
              stay in 8420-8439)

Requires PCDS_TEST_DATA: the game tables of that release. Works in a temporary folder (removed
at the end). Only the processes this script started are signalled. A pass: it prints "flip:" with
the update's message, the live bank it killed, the front's log tail, and "snapshot N bytes ->
OUT_SNAPSHOT" with N > 0. Python 3 standard library only.
"""
import argparse
import http.client
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0], epilog=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('release')
    p.add_argument('save')
    p.add_argument('out')
    p.add_argument('--port', type=int, default=8428)
    a = p.parse_args()
    data = os.environ.get('PCDS_TEST_DATA')
    if not data or data == 'none':
        sys.exit('PCDS_TEST_DATA: the game tables of the release are needed')
    rel = os.path.abspath(a.release)
    work = tempfile.mkdtemp(prefix='pcds-snapshot-')
    front = None
    try:
        # Two banks, both the release, as an installation's run/bin/a and b.
        banks = os.path.join(work, 'bin')
        info = subprocess.run([os.path.join(rel, 'pcds'), 'bank-info'], capture_output=True, check=True).stdout
        for b in 'ab':
            d = os.path.join(banks, b)
            os.makedirs(d)
            for f in os.listdir(rel):
                os.symlink(os.path.join(rel, f), os.path.join(d, f))
            with open(os.path.join(d, 'bank.json'), 'wb') as f:
                f.write(info)
        with open(os.path.join(banks, 'active'), 'w') as f:
            f.write('a')
        world = os.path.join(work, 'world.json')
        shutil.copy(a.save, world)
        token = 'smoke-' + os.urandom(6).hex()
        env = dict(os.environ, PCDS_HOME=work, PCDS_LISTEN=f'127.0.0.1:{a.port}', PCDS_ADMIN=f'127.0.0.1:{a.port + 1}',
                   PCDS_ADMIN_TOKEN=token, PCDS_STEAM='off')
        log = open(os.path.join(work, 'front.log'), 'w')
        front = subprocess.Popen([os.path.join(rel, 'pcds'), 'serve', data, world], env=env, stdout=log, stderr=log, cwd=work)

        def api(method, path, body=None):
            try:
                c = http.client.HTTPConnection('127.0.0.1', a.port + 1, timeout=60)
                c.request(method, path, json.dumps(body or {}), {'Authorization': 'Bearer ' + token})
                r = c.getresponse()
                t = r.read().decode()
                return r.status, (json.loads(t) if t else None)
            except Exception:  # the front is not up yet (or went away): the caller polls
                return 0, None

        for _ in range(600):
            if api('GET', '/api/status')[0] == 200:
                break
            time.sleep(0.2)
        else:
            sys.exit('the release did not start:\n' + open(os.path.join(work, 'front.log')).read())
        _status, ans = api('POST', '/api/update', {'rollback': True})
        seq = ans['seq']
        for _ in range(600):
            _status, u = api('GET', '/api/update')
            h = [e for e in (u or {}).get('history', []) if e['seq'] == seq]
            if h:
                print('flip:', h[0].get('message'))
                break
            time.sleep(0.2)
        _status, s = api('GET', '/api/status')
        live = s['banks']['live']
        print('live bank', live)
        os.kill(live.get('pid') or live.get('PID'), signal.SIGKILL)
        time.sleep(0.02)
        front.send_signal(signal.SIGTERM)
        front.wait(timeout=120)
        front = None
        print(open(os.path.join(work, 'front.log')).read()[-1500:])
        shutil.copy(world + '.snapshot', a.out)
        print('snapshot', os.path.getsize(a.out), 'bytes ->', a.out)
    finally:
        if front is not None and front.poll() is None:
            front.kill()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    main()
