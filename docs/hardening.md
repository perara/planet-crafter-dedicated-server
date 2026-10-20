# Hardening

What stops a bug or a hostile client in the server from becoming a problem for the rest of the
machine: the sandbox of the systemd unit `pcds` writes, the Docker defaults, the Kubernetes
settings, and what is left out on purpose and how to add it. `pcds doctor` says whether the unit on
disk is the one this release writes.

The measure used below is `systemd-analyze --user security pcds.service`, which scores a unit from
0 (nothing is open) to 10 (everything is). The unit of 0.8 scored **9.4 UNSAFE**; the unit of this
release scores **2.1 OK**.

## The systemd unit

`pcds setup`, `pcds install-service` and `pcds restart` write the unit with these options. All
of them were tried for real on a systemd 262 user manager with the server running: Steam logged
in and gave an invite code, a scripted player joined, saves and backups were written, a hot update
(the memfd hand-over) ran, the S3 copy and the webhook went out, chat hook programs ran (a shell
script with curl, Python, Node and Java), a plugin with `net.http` reached the Internet,
and `steamcmd` logged in anonymously.

| Option | What it stops |
|---|---|
| `NoNewPrivileges=yes` | `setuid` programs and file capabilities: nothing the server starts can gain rights |
| `ProtectSystem=strict` | everything outside `run/` is read-only: `/usr`, `/etc`, `/boot`, `/var`... |
| `ReadWritePaths=<run/>` | the one place the server writes: worlds, backups, the settings, the tables, the banks |
| `ProtectHome=read-only` | your files in `$HOME` can be read (the game, `~/.steam`) and not changed |
| `PrivateTmp=yes` | its own `/tmp` and `/var/tmp`, empty at every start (left out when `run/` itself is in `/tmp`) |
| `PrivateDevices=yes` | no disks, no GPU, no terminals: only `/dev/null`, `/dev/zero`, `/dev/urandom` and the like |
| `ProtectKernelTunables=yes`, `ProtectKernelModules=yes`, `ProtectKernelLogs=yes`, `ProtectControlGroups=yes`, `ProtectClock=yes`, `ProtectHostname=yes` | writing `/proc/sys` and `/sys`, loading modules, reading the kernel log, changing cgroups, setting the clock or the host name |
| `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK` | every other kind of socket (raw packets, Bluetooth, ...). `AF_NETLINK` stays: it is how a program lists the machine's network interfaces, which Steam's networking and many tools do |
| `RestrictNamespaces=yes` | making new namespaces (containers inside the server) |
| `RestrictRealtime=yes`, `RestrictSUIDSGID=yes`, `LockPersonality=yes` | realtime scheduling, files with the setuid bit, changing the execution domain |
| `CapabilityBoundingSet=` | every Linux capability, for the server and all it starts |
| `SystemCallFilter=@system-service`, `SystemCallErrorNumber=EPERM` | the system calls a service has no use for (mounting, rebooting, kernel modules, swap, raw I/O, debugging other processes); a refused call returns an error, it does not kill the process |
| `UMask=0077` | files the server and its hook programs make are for your user only |
| `LimitNOFILE=65536`, `TasksMax=1024` | room for the players, the admin connections and the hooks; a runaway cannot make the machine run out of processes |

The unit that checks for game updates (`pcds-update.service`) gets the subset that needs no
mount: no new privileges, no capabilities, only the socket families above, no realtime, no setuid
files, no namespaces, the system call filter and the umask. It runs `steamcmd`, which keeps its
state in `~/Steam` and writes the game, so it has no read-only file system.

### What a hook program or a plugin sees

A chat hook program (`chat.exec`) is a child of the server and has the same sandbox:

* it can write `run/` and nowhere else (not `$HOME`, not `/etc`); its `/tmp` is private;
* it cannot become another user (`sudo`, `su`), run `setuid` programs, or use `ping` with raw
  sockets;
* it can use the network with TCP and UDP, read the system, and run any ordinary program: a shell
  script with `curl`, Python, Node and Java were each tried;
* it can read `$HOME` (so a script in `~/bin` runs) but not write it. Write to `run/` or have the
  program send its output somewhere (a webhook, the chat).

A WebAssembly plugin does not run programs and writes nothing itself; its `net.http` goes out
through the server, which the sandbox lets reach TCP and UDP.

### What is not on, and why

These were tried and **break something the server runs**, so they are not in the unit. Add them in
a drop-in (`systemctl --user edit pcds`) if your hooks do not need what they block.

| Option | Why not by default |
|---|---|
| `MemoryDenyWriteExecute=yes` | The server itself runs with it. Programs it starts do not: **Node and Java** (they compile code at run time) fail at start (`SIGTRAP`, "Error occurred during initialization of VM"), and so does **steamcmd**. Use it when your hooks are shell, Python or compiled programs, and you do not use `--steam-user` |
| `SystemCallArchitectures=native` | **steamcmd** is a 32-bit program and is killed (`Bad system call`) |
| `ProtectProc=invisible`, `ProcSubset=pid` | Work, but hide `/proc/meminfo` and other processes from the hooks' tools (`ps`, `free`); a small gain |
| `PrivateNetwork=yes`, `IPAddressDeny=any` | The server is a network service |
| `MemoryMax=`, `CPUQuota=` | Depend on your machine; `pcds doctor` reads `MemoryMax` and warns if a hot update would not fit (about twice the game process's memory beyond what runs) |
| `ProtectHome=yes` or `tmpfs` | Hides the game folder when it is under `$HOME` (the usual Steam place) |

```bash
systemctl --user edit pcds         # opens a drop-in; add:  [Service]  MemoryDenyWriteExecute=yes
systemctl --user restart pcds
systemd-analyze --user security pcds.service      # the score and the open items
```

A drop-in survives `pcds` writing the unit again; the unit file itself does not (it is overwritten
by `pcds restart` and `pcds install-service`).

### Where the sandbox does not apply

A `systemd --user` service gets a mount namespace through an unprivileged user namespace. A
machine that does not allow that (a kernel with `user.max_user_namespaces=0`, a distribution that
restricts unprivileged user namespaces with AppArmor, some containers) would fail to start the
unit with `status=226/NAMESPACE`. This was not seen on the machine the sandbox was tried on
(Arch, systemd 262), so the handling below was tried with a stand-in that refuses. `pcds install-service` therefore tries a one-second throw-away unit
with the mount options first. If it cannot run, it writes the unit with the options above that
need no namespace (no new privileges, no capabilities, the socket families, the system call
filter, the umask, the limits), says so, and `pcds doctor` keeps saying so (a `WARN` on `service`).
To run the full sandbox there, allow unprivileged user namespaces (the kernel setting that
applies is named in `systemd-run --user --wait -p PrivateTmp=yes true`'s message), or run the
server as a system service you write yourself (`User=` in a unit under `/etc/systemd/system`; a
system manager needs no user namespace for the options above. `pcds` writes user units only, and
this was not tried).

The unit file is written by the command line, outside the sandbox. The server itself cannot
rewrite it (its home is read-only to it, and a server that could rewrite its own unit could
take the sandbox off for the next start): `pcds restart`, `pcds start` and `pcds upgrade
--restart` write it again when a release changes it, and `pcds doctor` says when it differs.

## Docker and Compose

The image starts as root only long enough to give `/data` to `PUID:PGID`, then runs the server as
that user with `setpriv --no-new-privs` (no privilege can be gained inside the container). The
[compose.yaml](../compose.yaml) shipped here turns on what was measured to work, on rootless Docker
with `docker/test.sh` (both parts: the hot update in the container, the restart in place):

```yaml
    read_only: true                       # the server writes only /data
    tmpfs: ["/tmp"]                       # Steam's crash dump folder
    security_opt: ["no-new-privileges:true"]
    cap_drop: [ALL]
    cap_add: [CHOWN, SETUID, SETGID, DAC_OVERRIDE, FOWNER]   # what the entrypoint needs to give /data away, then runs without
    pids_limit: 1024
    ulimits: { nofile: { soft: 65536, hard: 65536 } }
```

With `--user UID:GID` (or Kubernetes `runAsUser`) nothing is given away and `cap_add` is not needed.
The same flags are on the container `docker/test.sh` runs for the hot update. The base images in the [Dockerfile](../Dockerfile) are pinned by digest, so a build is the same
build tomorrow; Dependabot's `docker` entry proposes the new digest when the tag moves.

## Kubernetes

[kubernetes.yaml](kubernetes.yaml) runs the pod as a non-root user with `fsGroup`, a read-only
root file system, `allowPrivilegeEscalation: false`, all capabilities dropped, the `RuntimeDefault`
seccomp profile and no service account token (`automountServiceAccountToken: false`). Add a
`NetworkPolicy` that lets UDP 7777 in from anywhere and the admin port from the proxy or the
monitoring namespace only.

## What is not covered

* Nothing here protects the world from the people who play on it: that is the access lists and
  the rules ([README](../README.md#administration)).
* The admin API is plain HTTP: [reverse-proxy.md](reverse-proxy.md).
* The release feed is trusted (the checksum comes from the same place as the tarball): the
  [audits](audits.md) list it as open (I3).
