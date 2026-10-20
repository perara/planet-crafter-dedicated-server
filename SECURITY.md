# Security policy

## Reporting a vulnerability

Report it privately, not in a public issue: on the repository page open **Security**, then
**Report a vulnerability** (GitHub's private vulnerability reporting). Say which release it
affects (`pcds --version`), how to reproduce it, and what an attacker gets.

The maintainer answers within seven days, says whether it is a vulnerability, and fixes it in a
new release. The release notes (CHANGELOG.md) name the fix once it is out; the reporter is named
there too, unless they ask not to be.

## What is covered

The latest release only. The server is young and changes its formats between minor versions
(CHANGELOG.md), so an older release is fixed by updating, not by a patch.

In scope:

* The server (`pcds serve`): everything a player's game client or a stranger on the network can
  send to the UDP port (7777 by default) and, through Steam, to the invite-code connection.
* The admin API and web page (port 7782, local by default) and its tokens.
* The plugin host: a plugin getting more than the capabilities an admin approved.
* The updater (`pcds upgrade`, `install.sh`): a download that is installed without its checksum
  matching, or a way to make it run something else.
* The Docker image and the files of a release.

Out of scope: weaknesses of The Planet Crafter itself or of Steam (report them to Miju Games or
Valve), a server whose operator publishes the admin port and token on purpose, and anything that
needs access to the machine as the user the server runs as.

## For operators

* The admin API listens on `127.0.0.1:7782` unless `PCDS_ADMIN` says otherwise. Put a
  reverse proxy with TLS in front of it before you publish it; anyone with the token
  (`pcds token`) can stop the server and change every rule.
* `run/server.env` holds the token and is created with mode 600; keep it out of backups you share.
* Check a download before you run it: the tarball's sha256 is published next to it, and its build
  provenance is verified with `gh attestation verify` (docs/hot-update.md, "Verifying a release").
