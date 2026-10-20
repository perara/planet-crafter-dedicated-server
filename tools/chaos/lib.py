"""Shared helpers of the chaos and perf tools: start a private server, talk to its admin API,
read /proc, run the bots, check a save file. Python 3.9+, standard library only.

Not run by itself; the tools in tools/chaos/ and tools/perf/ import it:

    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "chaos"))
    import lib

What a tool needs (every tool takes these as options, see add_server_args):
  --bin    the pcds program (default server/target/release/pcds, `make build`)
  --data   the game tables (default $PCDS_TEST_DATA, else run/data/2.103 of the repository)
  --world  a save to run on; it is COPIED into the tool's work folder, never written
           (default run/worlds/world.json). No save ships with the repository.
  --work   a scratch folder for the servers' homes and logs (default: pcds-tools in the
           system's temp folder). Put it on tmpfs (/dev/shm) to keep disk I/O out of a measurement.
  --port   the first of the tool's ports (UDP for players; the admin API is --port + 1). The
           defaults are in 8420-8439. Ports 7777 and 7782 (a real server's) are refused.

A server the tool starts is its own: its own home (PCDS_HOME), world copy, admin token (random
per start) and process group; the tool signals only processes whose command line is
`<program> serve|bank DATA <its world copy>`, and ends them all when it exits. It refuses to start when its port
is already taken, so it never meets a server it did not start.

The bots are `pcds load` (the program) and the ignored cargo tests load::tests::rejoin and
load::tests::churn (server/src/load/tests.rs); the tests need the cargo toolchain, or a test
binary built earlier (--test-bin).
"""
import atexit
import glob
import json
import os
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

REPO = os.path.realpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
FORBIDDEN_PORTS = (7777, 7782)
LOOPBACK = "127.0.0.1"


# ---- options ---------------------------------------------------------------------------

def add_server_args(ap, port=8420, world=True):
    ap.add_argument("--bin", default=os.path.join(REPO, "server", "target", "release", "pcds"), help="the pcds program (default: %(default)s)")
    ap.add_argument("--data", default=os.environ.get("PCDS_TEST_DATA") or os.path.join(REPO, "run", "data", "2.103"), help="the game tables folder (default: $PCDS_TEST_DATA, else run/data/2.103)")
    if world:
        ap.add_argument("--world", default=os.path.join(REPO, "run", "worlds", "world.json"), help="the save to run on, copied first (default: %(default)s)")
    ap.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "pcds-tools"), help="scratch folder for homes and logs (default: %(default)s)")
    ap.add_argument("--port", type=int, default=port, help="the server's UDP port; the admin API is on port+1 (default: %(default)s)")
    ap.add_argument("--test-bin", default=None, help="a cargo test binary built earlier (cargo test --release --no-run); default: build/find it with cargo when the bots need it")


def check_inputs(args, need_world=True):
    """Makes the paths absolute; exits with a message when the program, the tables or the save are missing."""
    for k in ("bin", "data", "work", "world", "test_bin"):
        if getattr(args, k, None):
            setattr(args, k, os.path.abspath(getattr(args, k)))
    if not os.access(args.bin, os.X_OK):
        sys.exit(f"--bin {args.bin}: not an executable (build it with `make build`)")
    if not os.path.isfile(os.path.join(args.data, "netdata.json")):
        sys.exit(f"--data {args.data}: no game tables there (PCDS_TEST_DATA, or --data DIR)")
    if need_world and not os.path.isfile(args.world):
        sys.exit(f"--world {args.world}: no such save (pass a save of your own; none ships with the repository)")
    for p in (args.port, args.port + 1):
        if p in FORBIDDEN_PORTS:
            sys.exit(f"port {p} is a real server's; use another (the defaults are in 8420-8439)")


# ---- log and verdict ---------------------------------------------------------------------

class Log:
    """Timestamped lines to stdout and, when given, a file."""

    def __init__(self, path=None, quiet=False):
        self.t0 = time.time()
        self.f = open(path, "w") if path else None
        self.quiet = quiet

    def __call__(self, *a):
        line = f"{time.time() - self.t0:8.1f} " + " ".join(str(x) for x in a)
        if self.f:
            self.f.write(line + "\n")
            self.f.flush()
        if not self.quiet:
            print(line, flush=True)


class Verdict:
    """Collects failures; exit() prints one JSON line and returns the exit status (0: pass)."""

    def __init__(self, name, log):
        self.name, self.log, self.failures = name, log, []

    def fail(self, what, **kw):
        self.failures.append(dict(t=round(time.time() - self.log.t0, 1), what=what, **kw))
        self.log("FAIL", what, json.dumps(kw, default=str)[:600])

    def exit(self, **extra):
        print(json.dumps({"tool": self.name, "pass": not self.failures, "failures": self.failures, **extra}, default=str))
        sys.exit(1 if self.failures else 0)


# ---- admin API ---------------------------------------------------------------------------

def api(port, token, method, path, body=None, timeout=30):
    """(HTTP status, parsed JSON or text); status 0 and the error text when nothing answers."""
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"http://{LOOPBACK}:{port}{path}", data=data, method=method, headers={"Authorization": f"Bearer {token}"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            txt = r.read().decode()
            return r.status, (json.loads(txt) if txt else None)
    except urllib.error.HTTPError as e:
        txt = e.read().decode()
        try:
            return e.code, json.loads(txt)
        except ValueError:
            return e.code, txt
    except Exception as e:  # refused, timed out, reset: the caller decides what that means
        return 0, str(e)


def wait_for(fn, timeout, step=0.2):
    end = time.time() + timeout
    while time.time() < end:
        v = fn()
        if v:
            return v
        time.sleep(step)
    return None


# ---- /proc -------------------------------------------------------------------------------

def proc(pid):
    """RSS kB, VSZ kB, threads, fds and cpu seconds of a pid; None if it is gone."""
    try:
        st = open(f"/proc/{pid}/status").read()
        g = lambda k: int(re.search(rf"^{k}:\s+(\d+)", st, re.M).group(1))
        stat = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        cpu = (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK")
        return dict(rss=g("VmRSS"), vsz=g("VmSize"), thr=g("Threads"), fds=len(os.listdir(f"/proc/{pid}/fd")), cpu=cpu)
    except Exception:
        return None


def udp_queues(port):
    """(rx_queue, tx_queue) bytes of the UDP socket bound to `port`; None if there is none."""
    try:
        for line in open("/proc/net/udp").read().splitlines()[1:]:
            f = line.split()
            if int(f[1].split(":")[1], 16) == port:
                tx, rx = f[4].split(":")
                return int(rx, 16), int(tx, 16)
    except Exception:
        pass
    return None


def tcp_count(port):
    """Sockets on `port` that are not listening."""
    n = 0
    try:
        for line in open("/proc/net/tcp").read().splitlines()[1:]:
            p = line.split()
            if int(p[1].split(":")[1], 16) == port and p[3] != "0A":
                n += 1
    except Exception:
        pass
    return n


def tcp_listening(port):
    try:
        return any(l.split()[3] == "0A" and int(l.split()[1].split(":")[1], 16) == port for l in open("/proc/net/tcp").read().splitlines()[1:])
    except Exception:
        return False


def port_free(port):
    """True when nothing holds UDP and TCP `port` on loopback."""
    for kind in (socket.SOCK_DGRAM, socket.SOCK_STREAM):
        s = socket.socket(socket.AF_INET, kind)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)  # as a server does: a TIME_WAIT of the last run is not "in use"
        try:
            s.bind((LOOPBACK, port))
        except OSError:
            return False
        finally:
            s.close()
    return True


def argv_of(pid):
    try:
        return open(f"/proc/{pid}/cmdline", "rb").read().rstrip(b"\0").split(b"\0")
    except Exception:
        return []


# ---- a private server --------------------------------------------------------------------

_started = []


def _cleanup():
    for s in _started:
        s.kill_all()


atexit.register(_cleanup)
for _sig in (signal.SIGINT, signal.SIGTERM):
    signal.signal(_sig, lambda *_: sys.exit(130))


def merge(a, b):
    """b merged over a (dicts deeply)."""
    out = dict(a)
    for k, v in b.items():
        out[k] = merge(out[k], v) if isinstance(v, dict) and isinstance(out.get(k), dict) else v
    return out


class Srv:
    """One front + bank on a copy of a save, in its own home. banks=True gives it the two
    bank folders of an installation (bin/a, bin/b: both this program), which flips, rollbacks
    and the crash recovery need. rules: a rules.json (dict) merged over "no autosave, no
    backups"; env: more environment (PCDS_* names only the server documents); dev: PCDS_DEV hooks."""

    def __init__(self, name, args, port=None, world=None, rules=None, env=None, banks=False, dev=None, home=None, fresh=True, listen=None):
        self.args, self.name = args, name
        self.port = port or args.port
        self.admin = self.port + 1
        self.token = secrets.token_hex(8)
        self.home = home or os.path.join(args.work, name)
        self.world = os.path.join(self.home, "world.json")
        self.log = os.path.join(self.home, "front.log")
        self.listen = listen or f"{LOOPBACK}:{self.port}"
        self.proc = None
        self.pid = None
        for p in (self.port, self.admin):
            if p in FORBIDDEN_PORTS:
                sys.exit(f"port {p} is a real server's")
        if fresh and os.path.exists(self.home):
            shutil.rmtree(self.home)
        os.makedirs(self.home, exist_ok=True)
        if fresh:
            shutil.copy(world or args.world, self.world)
            json.dump(merge({"save": {"autosaveMinutes": 0, "backupsKept": 0}}, rules or {}), open(os.path.join(self.home, "rules.json"), "w"))
        self.env = dict(PCDS_LISTEN=self.listen, PCDS_ADMIN=f"{LOOPBACK}:{self.admin}", PCDS_ADMIN_TOKEN=self.token, PCDS_STEAM="off", PCDS_HOME=self.home,
                        PCDS_RELEASES="http://127.0.0.1:1")
        if dev:
            self.env["PCDS_DEV"] = dev
        if banks:
            self.make_banks()
        if env:
            self.env.update(env)

    def make_banks(self):
        info = subprocess.run([self.args.bin, "bank-info"], capture_output=True, check=True).stdout
        exe = os.path.realpath(self.args.bin)
        steam = os.path.join(os.path.dirname(exe), "libsteam_api.so")
        for b in "ab":
            d = os.path.join(self.home, "bin", b)
            os.makedirs(d, exist_ok=True)
            for src, dst in ((exe, "pcds"), (steam, "libsteam_api.so")):
                if os.path.exists(src) and not os.path.lexists(os.path.join(d, dst)):
                    os.symlink(src, os.path.join(d, dst))
            open(os.path.join(d, "bank.json"), "wb").write(info)
        open(os.path.join(self.home, "bin", "active"), "w").write("a")

    # -- lifecycle
    def start(self, wait=60):
        if not port_free(self.port) or not port_free(self.admin):
            sys.exit(f"ports {self.port}/{self.admin} are in use: a server that is not this tool's runs there; pass another --port")
        out = open(self.log, "ab")
        env = dict(os.environ)
        env.update(self.env)
        self.proc = subprocess.Popen([self.args.bin, "serve", self.args.data, self.world], stdout=out, stderr=out, env=env, cwd=self.home, start_new_session=True)
        self.pid = self.proc.pid
        if self not in _started:
            _started.append(self)
        end = time.time() + wait
        while time.time() < end:
            if self.proc.poll() is not None:
                raise RuntimeError(f"the front exited with {self.proc.returncode}; see {self.log}")
            s, j = self.status()
            if s == 200 and isinstance(j, dict) and j.get("state") == "running":
                return j
            time.sleep(0.2)
        raise RuntimeError(f"no start within {wait} s; see {self.log}")

    def stop(self, wait=60):
        """POST /api/stop (saves the world); the front is killed if it does not end."""
        if self.proc and self.proc.poll() is None:
            self.post("/api/stop")
            try:
                self.proc.wait(wait)
            except subprocess.TimeoutExpired:
                pass
        self.kill_all()

    def pids(self):
        """{'serve': [...], 'bank': [...]}: the processes of THIS world copy (exact argv)."""
        out = {"serve": [], "bank": []}
        want = self.world.encode()
        for d in os.listdir("/proc"):
            if d.isdigit():
                a = argv_of(int(d))
                if len(a) == 4 and a[1] in (b"serve", b"bank") and a[3] == want:
                    out[a[1].decode()].append(int(d))
        return out

    def kill_all(self):
        for ps in self.pids().values():
            for p in ps:
                try:
                    os.kill(p, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        if self.proc:
            try:
                self.proc.wait(10)
            except Exception:
                pass

    # -- admin API
    def status(self, timeout=10):
        return api(self.admin, self.token, "GET", "/api/status", timeout=timeout)

    def post(self, path, body=None, timeout=60):
        return api(self.admin, self.token, "POST", path, body if body is not None else {}, timeout=timeout)

    def get(self, path, timeout=30):
        return api(self.admin, self.token, "GET", path, timeout=timeout)

    def bank_pid(self):
        s, j = self.status()
        try:
            return j["banks"]["live"]["pid"]
        except Exception:
            return None

    def logtext(self):
        return open(self.log, errors="replace").read()

    def final_rc(self):
        return self.proc.returncode if self.proc else None


class UnitSrv(Srv):
    """The server as an owner runs it: `pcds setup` in an installation folder (server.env, worlds/,
    data/, bin/ with both banks), then `pcds run` as a transient systemd USER unit with the
    properties of the service file `pcds setup` writes (Restart=on-failure after 15 s, five starts
    in ten minutes, KillMode=mixed, 90 s to stop). The unit must be named pcds-qa-*: the real
    server's `pcds` is never touched. The game folder is a made-up one whose build id the copied
    tables carry (as docker/test.sh does). Bank b is a copy of bank a with another release tag, so
    a flip is a flip between two releases. Same interface as Srv; supervision is systemd's."""

    def __init__(self, name, args, unit, rules=None, env=None, dev=None, port=None, world=None, cpus="8-15"):
        if not unit.startswith("pcds-qa-"):
            sys.exit(f"unit {unit}: a scratch unit must be named pcds-qa-*")
        self.unit = unit
        self.cpus = cpus
        super().__init__(name, args, port=port, world=world, rules=rules, env=env, dev=dev, banks=False)
        h = self.home
        os.makedirs(f"{h}/worlds")
        os.replace(self.world, f"{h}/worlds/world.json")
        self.world = f"{h}/worlds/world.json"
        self.game = f"{h}/game"
        os.makedirs(f"{self.game}/Planet Crafter_Data/Managed")
        open(f"{self.game}/Planet Crafter.exe", "w").close()
        open(f"{self.game}/Planet Crafter_Data/Managed/Assembly-CSharp.dll", "w").write(f"a made-up game build for {unit}\n")
        rel = f"{h}/release"
        os.makedirs(f"{rel}/web")
        exe = os.path.realpath(args.bin)
        d = os.path.dirname(exe)
        shutil.copy(exe, f"{rel}/pcds")
        shutil.copy(os.path.join(d, "libsteam_api.so"), f"{rel}/libsteam_api.so")
        shutil.copy(os.path.join(d, "web", "index.html") if os.path.exists(os.path.join(d, "web", "index.html")) else os.path.join(REPO, "server", "web", "index.html"), f"{rel}/web/index.html")
        self.exe = f"{rel}/pcds"
        self.cli_env = dict(os.environ, PCDS_HOME=h, PCDS_UNIT=unit, PCDS_GAME_DIR=self.game, XDG_CONFIG_HOME=f"{h}/xdg", PCDS_RELEASES=self.env["PCDS_RELEASES"])
        for k in ("PCDS_LISTEN", "PCDS_ADMIN", "PCDS_ADMIN_TOKEN", "PCDS_STEAM", "PCDS_DEV"):
            self.cli_env.pop(k, None)
        said = subprocess.run([self.exe, "status"], env=self.cli_env, capture_output=True, text=True)
        m = re.search(r"game build ([0-9a-f]+) has no tables", said.stdout + said.stderr)
        if not m:
            sys.exit("pcds status did not name the made-up game's build:\n" + said.stdout + said.stderr)
        shutil.copytree(args.data, f"{h}/data/test")
        open(f"{h}/data/test/buildid", "w").write(m.group(1))
        env = dict(PCDS_LISTEN=self.listen, PCDS_STEAM="off", PCDS_ADMIN=f"{LOOPBACK}:{self.admin}", PCDS_ADMIN_TOKEN=self.token, PCDS_WORLD="world", PCDS_RELEASES=self.env["PCDS_RELEASES"])
        if env_extra := {k: v for k, v in self.env.items() if k not in ("PCDS_LISTEN", "PCDS_ADMIN", "PCDS_ADMIN_TOKEN", "PCDS_STEAM", "PCDS_HOME", "PCDS_RELEASES", "PCDS_DEV")}:
            env.update(env_extra)
        with open(f"{h}/server.env", "w") as f:
            f.write("".join(f"{k}={v}\n" for k, v in env.items()))
        os.chmod(f"{h}/server.env", 0o600)
        r = subprocess.run([self.exe, "setup", "--game", self.game], env=self.cli_env, capture_output=True, text=True)
        if r.returncode:
            sys.exit("pcds setup failed:\n" + r.stdout + r.stderr)
        # Bank b: the same program as another release (the standby a flip needs).
        shutil.copytree(f"{h}/bin/a", f"{h}/bin/b", symlinks=True)
        bj = json.load(open(f"{h}/bin/b/bank.json"))
        bj["tag"] = "v0.0.0-qa-b"
        json.dump(bj, open(f"{h}/bin/b/bank.json", "w"))
        open(f"{h}/bin/active", "w").write("a")
        self.pcds = f"{h}/bin/pcds"

    # -- systemd
    def sysctl(self, *a, timeout=120):
        return subprocess.run(["systemctl", "--user", *a], capture_output=True, text=True, timeout=timeout)

    def unit_state(self):
        r = self.sysctl("show", "-p", "ActiveState", "-p", "SubState", "-p", "Result", "-p", "NRestarts", "-p", "ExecMainStatus", f"{self.unit}.service")
        return dict(l.split("=", 1) for l in r.stdout.splitlines() if "=" in l)

    def start(self, wait=90):
        if not port_free(self.port) or not port_free(self.admin):
            sys.exit(f"ports {self.port}/{self.admin} are in use")
        if self not in _started:
            _started.append(self)
        self.sysctl("reset-failed", f"{self.unit}.service")
        cmd = ["systemd-run", "--user", "--quiet", f"--unit={self.unit}", f"--description=pcds qa unit ({self.name})", "--slice=agents-planetcrafter.slice",
               "-p", f"AllowedCPUs={self.cpus}", "-p", f"EnvironmentFile={self.home}/server.env", "-p", "Restart=on-failure", "-p", "RestartSec=15",
               "-p", "StartLimitIntervalSec=600", "-p", "StartLimitBurst=5", "-p", "KillMode=mixed", "-p", "TimeoutStopSec=90",
               "-p", f"StandardOutput=append:{self.log}", "-p", f"StandardError=append:{self.log}",
               f"--setenv=PCDS_HOME={self.home}", f"--setenv=PCDS_UNIT={self.unit}", f"--setenv=PCDS_GAME_DIR={self.game}"]
        if "PCDS_DEV" in self.env:
            cmd.append(f"--setenv=PCDS_DEV={self.env['PCDS_DEV']}")
        r = subprocess.run(cmd + [self.pcds, "run"], capture_output=True, text=True)
        if r.returncode:
            raise RuntimeError("systemd-run: " + r.stderr)
        end = time.time() + wait
        while time.time() < end:
            s, j = self.status()
            if s == 200 and isinstance(j, dict) and j.get("state") == "running":
                return j
            if self.unit_state().get("ActiveState") == "failed":
                raise RuntimeError(f"the unit failed; see {self.log}")
            time.sleep(0.3)
        raise RuntimeError(f"no start within {wait} s; see {self.log}")

    def ensure_running(self):
        """What an owner (or a monitor) would do: nothing while systemd restarts the unit by itself;
        when it gave up (start limit) or ended, reset it and start it again. Returns the unit's Result when it had to."""
        st = self.unit_state()
        if st.get("ActiveState") in ("active", "activating", "deactivating") or st.get("SubState") == "auto-restart":
            return None
        why = st.get("Result", "?")
        self.sysctl("reset-failed", f"{self.unit}.service")
        if self.sysctl("restart", f"{self.unit}.service").returncode:
            self.sysctl("reset-failed", f"{self.unit}.service")
            self.start(wait=120)
        return why

    def stop(self, wait=100):
        if self.unit_state().get("ActiveState") in ("active", "activating"):
            self.post("/api/stop")
            end = time.time() + wait
            while time.time() < end and self.unit_state().get("ActiveState") not in ("inactive", "failed"):
                time.sleep(0.5)
        self.final = self.unit_state()
        self.kill_all()

    def final_rc(self):
        """The main process's exit status at the last stop (0: a clean stop)."""
        f = getattr(self, "final", None) or self.unit_state()
        return int(f.get("ExecMainStatus", "-1")) if f.get("Result") in ("success", "exit-code") else f.get("Result")

    def kill_all(self):
        self.sysctl("stop", f"{self.unit}.service")
        self.sysctl("reset-failed", f"{self.unit}.service")
        for ps in self.pids().values():
            for p in ps:
                try:
                    os.kill(p, signal.SIGKILL)
                except ProcessLookupError:
                    pass


def window(srv, secs, every=0.5):
    """Polls the status for `secs`: (longest loop ms, mean busy %, max busy %)."""
    import statistics
    mx, bs, t = 0, [], time.time()
    while time.time() - t < secs:
        s, j = srv.status(timeout=30)
        if s == 200:
            mx = max(mx, j["longestLoopMicros"])
            bs.append(j["busyPercent"])
        time.sleep(every)
    return round(mx / 1000, 1), (round(statistics.mean(bs), 2) if bs else None), (round(max(bs), 2) if bs else None)


# ---- the bots ----------------------------------------------------------------------------

def load_proc(srv_or_addr, args, players, seconds, action_ms=200, stdout=subprocess.PIPE, name=None):
    """`pcds load`: players that walk and act. Its summary (round trips, bytes per player) is
    printed at the end; exit status 1 when a player failed."""
    addr = srv_or_addr if isinstance(srv_or_addr, str) else f"{LOOPBACK}:{srv_or_addr.port}"
    return subprocess.Popen([args.bin, "load", addr, args.data, str(players), str(seconds), str(action_ms)] + ([name] if name else []), stdout=stdout, stderr=subprocess.STDOUT, text=True)


_test_bin = None


def test_binary(args):
    """The cargo test binary of the server crate (the ignored load tests are in it)."""
    global _test_bin
    if args.test_bin:
        return args.test_bin
    if _test_bin is None:
        r = subprocess.run(["cargo", "test", "--manifest-path", os.path.join(REPO, "server", "Cargo.toml"), "--release", "--locked", "--no-run"], cwd=REPO, capture_output=True, text=True)
        m = re.search(r"Executable unittests src/main\.rs \((.*)\)", r.stderr)
        if r.returncode or not m:
            sys.exit("could not build the test binary with cargo:\n" + r.stderr[-1500:])
        _test_bin = os.path.join(REPO, m.group(1))
    return _test_bin


def steam_env(exe, base=None):
    """The environment for a cargo test binary: libsteam_api.so sits in the steamworks crate's
    build output beside the binary's deps/ folder (the Makefile sets LD_LIBRARY_PATH the same way)."""
    e = dict(base if base is not None else os.environ)
    libs = glob.glob(os.path.join(os.path.dirname(os.path.dirname(exe)), "build", "steamworks-sys-*", "out"))
    if libs:
        e["LD_LIBRARY_PATH"] = os.pathsep.join(libs + [e.get("LD_LIBRARY_PATH", "")]).rstrip(os.pathsep)
    return e


class Bots:
    """An ignored test of load::tests (rejoin or churn) as a process whose JSON lines are
    collected in .events as (time, dict). env: the load variables of server/src/load/tests.rs (PCDS_LOAD_SEED...)."""

    def __init__(self, test, args, server, players=8, seconds=60, **env):
        exe = test_binary(args)
        e = steam_env(exe)
        e.update(PCDS_TEST_DATA=args.data, PCDS_LOAD_SERVER=server, PCDS_LOAD_PLAYERS=str(players), PCDS_LOAD_SECONDS=str(seconds))
        e.update({k: str(v) for k, v in env.items()})
        self.events = []
        self.p = subprocess.Popen([exe, f"load::tests::{test}", "--ignored", "--nocapture", "--exact"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=e, cwd=os.path.join(REPO, "server"))
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            if line.startswith("{"):
                try:
                    self.events.append((time.time(), json.loads(line)))
                except ValueError:
                    pass

    @property
    def pid(self):
        return self.p.pid

    def stats(self, since=0):
        ev = [d for (t, d) in self.events if t >= since and "summary" not in d]
        return dict(joins=sum(1 for d in ev if d.get("joined") is not None), fails=sum(1 for d in ev if d.get("failed")))

    def failures(self, since=0):
        return [d.get("failed") for (t, d) in self.events if t >= since and d.get("failed")]

    def poll(self):
        return self.p.poll()

    def stop(self):
        if self.p.poll() is None:
            self.p.terminate()
            try:
                self.p.wait(10)
            except subprocess.TimeoutExpired:
                self.p.kill()


# ---- save files --------------------------------------------------------------------------

def parse_save(path):
    t = open(path, encoding="utf-8-sig", newline="").read()
    t = t[1:] if t.startswith("\r") else t
    if t.endswith("\r@"):
        t = t[:-2]
    secs = t.split("\r@\r")
    lst = lambda s: [json.loads(x) for x in s.split("|\n") if x.strip()]
    names = ["world", "planets", "players", "wos", "inventories", "stats", "messages", "story", "settings", "instances"]
    return {n: (json.loads(s) if n in ("world", "settings") else lst(s)) for n, s in zip(names, secs)}


def check_save(path):
    """Invariants of a save: unique ids, every item held once, players' inventories there, links
    resolve. Returns (violations, counts). A save with violations of its own: compare against
    check_save(original) and ignore those."""
    s = parse_save(path)
    bad = []
    ids = [w["id"] for w in s["wos"]]
    if len(ids) != len(set(ids)):
        bad.append(f"duplicate wo ids: {len(ids) - len(set(ids))}")
    wset = set(ids)
    iids = [i["id"] for i in s["inventories"]]
    if len(iids) != len(set(iids)):
        bad.append(f"duplicate inventory ids: {len(iids) - len(set(iids))}")
    holder = {}
    for inv in s["inventories"]:
        for x in [int(x) for x in inv.get("woIds", "").split(",") if x]:
            if x in holder and holder[x] != inv["id"]:
                bad.append(f"wo {x} in inventories {holder[x]} and {inv['id']}")
            holder[x] = inv["id"]
            if x not in wset:
                bad.append(f"inventory {inv['id']} holds missing wo {x}")
    iset = set(iids)
    for p in s["players"]:
        for k in ("inventoryId", "equipmentId"):
            if p.get(k, 0) not in iset:
                bad.append(f"player {p.get('name')} {k} {p.get(k)} missing")
    pn = [p.get("name") for p in s["players"]]
    if len(pn) != len(set(pn)):
        bad.append("duplicate player names")
    for w in s["wos"]:
        li = w.get("liId", 0)
        if li and li not in iset:
            bad.append(f"wo {w['id']} linked inventory {li} missing")
    return bad, dict(wos=len(s["wos"]), inventories=len(s["inventories"]), players=len(s["players"]), held=len(holder), bytes=os.path.getsize(path))


def containers(path):
    return {w["id"] for w in parse_save(path)["wos"] if w["gId"].startswith("Container")}
