#!/usr/bin/env bash
# Installs a Planet Crafter dedicated server and starts it, in one command:
#
#   curl -fsSL https://raw.githubusercontent.com/perara/planet-crafter-dedicated-server/main/install.sh | bash
#
# It checks the machine, offers to install what is missing (curl, 32-bit libraries) with your package
# manager, downloads the latest release into ~/planet-crafter-server, and runs `pcds up`: the
# game tables from your copy of the game, a world (one of your saves, or a new one), the
# systemd user service, a selftest, and the invite code and address players join with.
# Run again to upgrade; the worlds and settings in run/ stay.
#
# Options (curl ... | bash -s -- OPTIONS):
#   --dir DIR              where to install (default ~/planet-crafter-server)
#   --version vX.Y.Z       a given release instead of the latest
#   --yes                  install missing packages without asking
#   --game DIR             the game folder, when it is not in a Steam library here
#   --steam-user NAME      no game here: download it with steamcmd and your Steam login
#   --world SAVE.json      play this save
#   --new [--planet ID]    a new world
set -euo pipefail

REPO=perara/planet-crafter-dedicated-server

say() { echo "install: $*"; }
die() {
  echo "install: $*" >&2
  exit 1
}

# Questions go to the terminal: with `curl | bash` the script itself is on stdin.
have_tty() { (exec < /dev/tty) 2> /dev/null; }
ask() {
  local answer
  have_tty || return 1
  read -r -p "$1 [Y/n] " answer < /dev/tty
  [[ -z "$answer" || "$answer" == [yY]* ]]
}

# The packages this machine lacks, in its package manager's names.
missing_packages() {
  local pm=$1 need=()
  command -v curl > /dev/null || need+=(curl)
  command -v tar > /dev/null || need+=(tar)
  # steamcmd (invite codes without the Steam client, or --steam-user) is a 32-bit program.
  if [[ ! -f "$HOME/.steam/sdk64/steamclient.so" && ! -f "$HOME/.local/share/Steam/linux64/steamclient.so" ]] &&
    [[ ! -e /lib/ld-linux.so.2 && ! -e /lib32/ld-linux.so.2 && ! -e /usr/lib32/ld-linux.so.2 ]]; then
    need+=(lib32)
  fi
  local p
  for p in "${need[@]}"; do
    case "$pm:$p" in
      apt-get:lib32) echo lib32gcc-s1 ;;
      dnf:lib32) echo glibc.i686 libstdc++.i686 ;;
      pacman:lib32) echo lib32-gcc-libs ;;
      zypper:lib32) echo libgcc_s1-32bit ;;
      *) echo "$p" ;;
    esac
  done
}

install_packages() {
  local pm="" packages=()
  for pm in apt-get dnf pacman zypper; do
    command -v "$pm" > /dev/null && break
    pm=""
  done
  mapfile -t packages < <(missing_packages "${pm:-none}" | tr ' ' '\n')
  ((${#packages[@]})) || return 0
  local cmd
  case "$pm" in
    apt-get) cmd="sudo apt-get update && sudo apt-get install -y ${packages[*]}" ;;
    dnf) cmd="sudo dnf install -y ${packages[*]}" ;;
    pacman) cmd="sudo pacman -S --needed --noconfirm ${packages[*]}" ;;
    zypper) cmd="sudo zypper install -y ${packages[*]}" ;;
    *) die "missing: ${packages[*]}; install them with your package manager and run this again" ;;
  esac
  say "missing: ${packages[*]}"
  if ((YES)) || ask "install them now with: $cmd ?"; then
    bash -c "$cmd" < /dev/null
  else
    die "install them yourself (for example: $cmd) and run this again"
  fi
}

main() {
  local dir="$HOME/planet-crafter-server" version=latest up=()
  YES=0
  while (($#)); do
    case "$1" in
      --dir) dir="${2:?--dir DIR}"; shift ;;
      --version) version="${2:?--version vX.Y.Z}"; shift ;;
      --yes) YES=1 ;;
      *) up+=("$1") ;;
    esac
    shift
  done
  [[ $EUID -ne 0 ]] || die "run this as the user the server runs as, not as root (it asks for sudo only to install packages)"
  [[ "$(uname -sm)" == "Linux x86_64" ]] || die "the release runs on Linux x86_64 (this is $(uname -sm)); see the README for other ways"
  local glibc
  glibc="$(getconf GNU_LIBC_VERSION 2> /dev/null | awk '{print $2}')"
  [[ -n "$glibc" && "$(printf '%s\n2.34\n' "$glibc" | sort -V | head -1)" == 2.34 ]] ||
    die "glibc ${glibc:-unknown} is older than 2.34 (Ubuntu 22.04, Debian 12, Fedora 35 or newer); the Docker image works anywhere"
  command -v systemctl > /dev/null || die "no systemd here; the Docker image is the way (see the README)"
  install_packages

  local tag url
  if [[ "$version" == latest ]]; then
    tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p')"
    [[ -n "$tag" ]] || die "no latest release found at https://github.com/$REPO/releases"
  else
    tag="$version"
  fi
  TMP_DIR="$(mktemp -d)"
  trap 'rm -rf -- "${TMP_DIR:?}"' EXIT
  local tmp=$TMP_DIR
  url="https://github.com/$REPO/releases/download/$tag/pcds-$tag-linux-x86_64.tar.gz"
  say "downloading $tag"
  curl -fsSL -o "$tmp/pcds.tar.gz" "${PCDS_TARBALL_URL:-$url}"
  # Every release publishes its checksum: without it nothing is installed. PCDS_TARBALL_URL (a
  # test build) has none.
  if [[ -z "${PCDS_TARBALL_URL:-}" ]]; then
    curl -fsSL -o "$tmp/pcds.tar.gz.sha256" "$url.sha256" || die "no checksum for $tag at $url.sha256: nothing installed"
    (cd "$tmp" && sed "s|  .*|  pcds.tar.gz|" pcds.tar.gz.sha256 | sha256sum -c --quiet -) || die "the download does not match its checksum"
  fi
  mkdir -p "$dir"
  # Program files are replaced; run/ (worlds, settings, game tables) stays.
  tar -xzf "$tmp/pcds.tar.gz" -C "$dir" --strip-components=1
  say "installed $tag in $dir"
  cd "$dir"
  if have_tty; then
    ./pcds up "${up[@]}" < /dev/tty
  else
    ./pcds up "${up[@]}"
  fi
}

# Everything runs from here, so a download cut short runs nothing.
main "$@"
