#!/bin/sh
# The programs as system services on Linux, end to end -- what tools/smoke.sh cannot check,
# because it needs root: a relay that was first started by hand and then installed as a service
# keeps its data folder and its setup line, the worker's service runs jobs as the person who
# installed it (not as root) with that person's PATH, and a server is set up by one command.
#
#   BB_SERVICE_TEST=1 tools/service-test.sh FOLDER_WITH_THE_PROGRAMS
#
# For a CI runner or a throwaway VM only: it needs systemd and sudo without a password, it
# installs and removes the two services, and it removes Build Balancer's data folders of this
# user and of the system.  It refuses to run where such folders exist already.
set -u
[ "${BB_SERVICE_TEST:-}" = 1 ] || {
    echo "this test installs services and removes Build Balancer's data folders: set BB_SERVICE_TEST=1 on a machine made for it"
    exit 2
}
[ "$(uname -s)" = Linux ] && [ -d /run/systemd/system ] || {
    echo "this test is for Linux with systemd"
    exit 2
}
bin=$(cd "${1:?the folder with the programs}" && pwd)
me=$(id -un)
own="$HOME/.local/share/buildbalancer"
system=/var/lib/buildbalancer
if [ -e "$own" ] || sudo test -e "$system" || [ -e /etc/systemd/system/buildbalancer-relay.service ] || [ -e /etc/systemd/system/buildbalancer-worker.service ]; then
    echo "this machine has Build Balancer data or services already: not a place for this test"
    exit 2
fi
port=${BB_SERVICE_PORT:-7792}
work=$(mktemp -d) || exit 1
[ -n "$work" ] && [ -d "$work" ] || {
    echo "no temporary folder could be made"
    exit 1
}
export BB_CONFIG="$work/main"
bb="$bin/bb"
fails=0
relay_pid=""

cleanup() {
    [ -n "$relay_pid" ] && kill -9 "$relay_pid" 2>/dev/null
    sudo "$bin/bb-worker" service uninstall >/dev/null 2>&1
    sudo "$bin/bb-relay" service uninstall >/dev/null 2>&1
    sudo rm -rf "$system"
    rm -rf "$own" "$work"
}
trap cleanup EXIT
check() { # description, expected, got
    if [ "$2" = "$3" ]; then echo "ok    $1"; else
        echo "FAIL  $1: expected \"$2\", got \"$3\""
        fails=$((fails + 1))
    fi
}
wait_for() { # seconds, command...
    # by the clock: on a busy machine every look takes its time, too
    end=$(($(date +%s) + $1))
    shift
    while :; do
        got=$("$@" 2>/dev/null)
        if [ -n "$got" ]; then
            printf '%s' "$got"
            return 0
        fi
        [ "$(date +%s)" -ge "$end" ] && return 1
        sleep 0.2
    done
}
die() {
    echo "FAIL  $1"
    [ -f "$2" ] && sed 's/^/      | /' "$2"
    exit 1
}
key_in() { grep "bb setup" "$1" 2>/dev/null | head -1 | awk '{print $4}'; }
online() { "$bb" workers --json | grep '"online": true'; }

# --- a relay first started by hand, as this user, then installed as a service with sudo
"$bin/bb-relay" --listen "127.0.0.1:$port" >"$work/relay.out" 2>&1 &
relay_pid=$!
key=$(wait_for 20 key_in "$work/relay.out") || die "the relay printed no setup key" "$work/relay.out"
kill -9 "$relay_pid" 2>/dev/null
wait "$relay_pid" 2>/dev/null
relay_pid=""
sudo "$bin/bb-relay" service install --listen "127.0.0.1:$port" >"$work/install.out" 2>&1
check "bb-relay service install" 0 $?
check "the service keeps the relay that was started by hand (same setup line)" "$key" "$(key_in "$work/install.out")"
check "... in the same data folder" 1 "$(grep -c "data folder *$own/relay" "$work/install.out")"
check "... and runs as its owner" "User=$me" "$(grep '^User=' /etc/systemd/system/buildbalancer-relay.service)"
check "the relay's service is running" active "$(systemctl is-active buildbalancer-relay)"
check "no second relay was made for root" no "$(sudo test -e "$system/relay" && echo yes || echo no)"
"$bb" setup "127.0.0.1:$port" "$key" --name service-main >/dev/null
check "the copied line still sets the main machine up" 0 $?

# --- the worker: paired as this user, installed with sudo, runs jobs as this user
code_in() { grep "bb-worker join" "$work/pair.out" 2>/dev/null | awk '{print $4}'; }
"$bb" pair >"$work/pair.out" 2>&1 &
pair_pid=$!
code=$(wait_for 20 code_in) || die "bb pair printed no code" "$work/pair.out"
"$bin/bb-worker" join "127.0.0.1:$port" "$code" --name service-worker --cpu 2 --priority normal --no-run >/dev/null
check "pairing as this user" 0 $?
wait "$pair_pid" 2>/dev/null
sudo env -u SUDO_USER "$bin/bb-worker" service install >"$work/root.out" 2>&1
check "root itself has to say whom jobs run as" 1 $?
check "... and is told how" 1 "$(grep -c -- "--as-root" "$work/root.out")"
sudo "$bin/bb-worker" service install >"$work/worker.out" 2>&1
check "bb-worker service install" 0 $?
check "the worker's service runs as the one who installed it" "User=$me" "$(grep '^User=' /etc/systemd/system/buildbalancer-worker.service)"
wait_for 40 online >/dev/null || die "the worker's service did not come online" "$work/worker.out"
mkdir -p "$work/proj" && cd "$work/proj" || exit 1
check "jobs run as that user, not as root" "$me" "$("$bb" run -q --no-sync --sh 'id -un' 2>/dev/null)"
check "... with that user's own tools on the PATH" 1 "$("$bb" run -q --no-sync --sh 'echo "$PATH"' 2>/dev/null | grep -c "$HOME/.cargo/bin")"
check "... in that user's data folder" 1 "$(ls -d "$own/worker/ws" 2>/dev/null | wc -l | tr -d ' ')"
cd "$work" || exit 1

sudo "$bin/bb-worker" service uninstall >/dev/null
check "bb-worker service uninstall" 0 $?
sudo "$bin/bb-relay" service uninstall >/dev/null
check "bb-relay service uninstall" 0 $?
rm -rf "$own"

# --- a server from nothing: one command installs the relay, starts it and prints the line
sudo "$bin/bb-relay" service install --listen "127.0.0.1:$port" >"$work/fresh.out" 2>&1
check "one command sets a server up" 0 $?
key=$(key_in "$work/fresh.out")
check "... and prints the line for the main machine" 1 "$([ -n "$key" ] && echo 1 || echo 0)"
check "... which bb-relay status shows again" "$key" "$(sudo "$bin/bb-relay" status | grep "bb setup" | head -1 | awk '{print $4}')"
check "asked without sudo, the relay is not simply missing" 1 "$("$bin/bb-relay" status 2>&1 | grep -c "$system/relay: run this command with sudo")"

echo
if [ "$fails" = 0 ]; then echo "all service checks passed"; else
    echo "$fails service check(s) failed"
    for f in install.out root.out worker.out fresh.out; do
        echo "--- $f"
        cat "$work/$f" 2>/dev/null
    done
fi
exit "$fails"
