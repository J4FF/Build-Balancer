#!/bin/sh
# Build Balancer for Linux and macOS: fetch the programs for this machine and, if asked, set
# the machine up in the same go.
#
#   curl -fsSL https://github.com/J4FF/Build-Balancer/releases/latest/download/install.sh | sh
#       only fetch the programs (bb, bb-worker, bb-relay, BuildBalancer)
#   ... | sudo sh -s -- relay
#       a server: install the relay as a service, start it, print the line for the main machine
#       (run again on that server, it brings the relay up to date and starts it again)
#   ... | sh -s -- worker RELAY CODE
#       a machine that lends its power: pair with the code of the main machine, start lending
#   ... | sh -s -- main RELAY KEY
#       the main machine, without a window: the line the relay printed
#
# The programs go to /usr/local/bin when this runs as root, to ~/.local/bin otherwise
# (BB_INSTALL_DIR names another folder).  What is fetched is checked against the release's
# SHA256SUMS.  BB_RELEASE_URL names another place to fetch from (a mirror, a test).
set -eu

repo="${BB_REPO:-https://github.com/J4FF/Build-Balancer}"
base="${BB_RELEASE_URL:-$repo/releases/latest/download}"

say() { printf '%s\n' "$*"; }
die() {
    say "Build Balancer install: $*" >&2
    exit 1
}

case "$(uname -s)" in
Linux) os=linux ;;
Darwin) os=macos ;;
*) die "this script is for Linux and macOS (Windows: install.ps1)" ;;
esac
case "$(uname -m)" in
x86_64 | amd64) arch=x86_64 ;;
aarch64 | arm64) arch=aarch64 ;;
*) die "no programs for $(uname -m) processors yet: build them from the source ($repo, \"Building\")" ;;
esac
name="buildbalancer-$os-$arch.tar.gz"

fetch() { # URL, file
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        die "neither curl nor wget is here to fetch the programs with"
    fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "Build Balancer: fetching the programs for $os on $arch ..."
fetch "$base/$name" "$tmp/$name" || die "$base/$name could not be fetched.
There may be no release for $os on $arch yet: see $repo/releases, or build from the source (\"Building\")."
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "the release's SHA256SUMS could not be fetched"

want=$(awk -v n="$name" '$2 == n || $2 == "*" n { print $1 }' "$tmp/SHA256SUMS")
if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "$tmp/$name" | awk '{ print $1 }')
else
    got=$(shasum -a 256 "$tmp/$name" | awk '{ print $1 }')
fi
[ -n "$want" ] || die "SHA256SUMS does not list $name"
[ "$want" = "$got" ] || die "$name does not match its checksum (expected $want, got $got): nothing was installed"

mkdir "$tmp/programs"
tar -xzf "$tmp/$name" -C "$tmp/programs"

if [ -n "${BB_INSTALL_DIR:-}" ]; then
    dir=$BB_INSTALL_DIR
elif [ "$(id -u)" = 0 ]; then
    dir=/usr/local/bin
else
    dir="$HOME/.local/bin"
fi
mkdir -p "$dir"
for p in bb bb-worker bb-relay BuildBalancer; do
    if [ -f "$tmp/programs/$p" ]; then
        # put beside the old file and renamed over it: a program that is running is not disturbed
        cp "$tmp/programs/$p" "$dir/.$p.new"
        chmod 755 "$dir/.$p.new"
        mv -f "$dir/.$p.new" "$dir/$p"
    fi
done
say "Installed into $dir:  bb  bb-worker  bb-relay  BuildBalancer  ($("$dir/bb" --version 2>/dev/null || echo "this machine cannot run them?"))"
case ":$PATH:" in
*":$dir:"*) ;;
*) say "Note: $dir is not on your PATH.  Add it (for example: echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile)
      or call the programs by their full path, as the lines below do." ;;
esac

action=${1:-}
[ $# -gt 0 ] && shift

# A relay or a worker that runs here as a service was started from the programs that were
# just replaced, and is still the version from before.  (The relay action does this below:
# `bb-relay service install` on a relay that has its service starts it again.)
service_runs() { "$dir/$1" service status 2>/dev/null | grep -q ': running$'; }
if [ "$action" != relay ] && service_runs bb-relay; then
    if [ "$(id -u)" = 0 ]; then
        # a relay holds no job: starting it again costs the machines a moment's reconnecting
        "$dir/bb-relay" service install || say "Note: the relay's service was not started again with the new programs (see above)."
    else
        say "Note: the relay on this machine still runs the version from before.  This starts it again with the new one:"
        say "      sudo $dir/bb-relay service install"
    fi
fi
if service_runs bb-worker; then
    say "Note: a worker runs here as a service.  It sees the new programs by itself and starts again with them"
    say "      as soon as no job runs.  (One from before 0.1.2 does not; that one needs, once -- it ends a job"
    say "      that runs:  sudo $dir/bb-worker service stop && sudo $dir/bb-worker service start)"
fi

case "$action" in
"")
    say ""
    say "Next:"
    say "  a server (the relay):          sudo $dir/bb-relay service install"
    say "  the main machine:              $dir/BuildBalancer   (or: $dir/bb setup RELAY KEY)"
    say "  a machine that lends power:    $dir/BuildBalancer   (or: $dir/bb-worker join RELAY CODE)"
    ;;
relay)
    [ "$(id -u)" = 0 ] || die "the relay becomes a system service, which needs root: run the line with 'sudo sh' instead of 'sh'"
    exec "$dir/bb-relay" service install "$@"
    ;;
worker)
    [ $# -ge 2 ] || die "worker needs the relay's address and the code: both are shown by 'bb pair' (or \"Add a machine\") on the main machine"
    exec "$dir/bb-worker" join "$@"
    ;;
main)
    [ $# -ge 2 ] || die "main needs the relay's address and its setup key: the line the relay printed"
    exec "$dir/bb" setup "$@"
    ;;
*) die "unknown action \"$action\" (relay, worker RELAY CODE, main RELAY KEY, or none)" ;;
esac
