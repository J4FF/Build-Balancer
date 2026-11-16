#!/bin/sh
# The programs as system services on Linux, end to end -- what tools/smoke.sh cannot check,
# because it needs root: a relay that was first started by hand and then installed as a service
# keeps its data folder and its setup line, the worker's service runs jobs as the person who
# installed it (not as root) with that person's PATH, a server is set up by one command -- and
# brought up to date by the same one: the install line run again starts the relay again.
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
root=$(cd "$(dirname "$0")/.." && pwd)
bin=$(cd "${1:?the folder with the programs}" && pwd)
me=$(id -un)
own="$HOME/.local/share/buildbalancer"
system=/var/lib/buildbalancer
if [ -e "$own" ] || sudo test -e "$system" || [ -e /etc/systemd/system/buildbalancer-relay.service ] || [ -e /etc/systemd/system/buildbalancer-worker.service ]; then
    echo "this machine has Build Balancer data or services already: not a place for this test"
    exit 2
fi
port=${BB_SERVICE_PORT:-7792}
web=${BB_SERVICE_WEB_PORT:-7791}
work=$(mktemp -d) || exit 1
[ -n "$work" ] && [ -d "$work" ] || {
    echo "no temporary folder could be made"
    exit 1
}
export BB_CONFIG="$work/main"
bb="$bin/bb"
fails=0
relay_pid=""
web_pid=""

cleanup() {
    [ -n "$relay_pid" ] && kill -9 "$relay_pid" 2>/dev/null
    [ -n "$web_pid" ] && kill -9 "$web_pid" 2>/dev/null
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
# What a job prints -- nothing when no worker ran it within two minutes.  (A job without a
# worker waits for one for as long as it takes; this test must not.)
says() { # the shell command
    said=$("$bb" run -d --json --no-sync --sh "$1" 2>/dev/null | grep '"job"' | sed 's/.*: "\(.*\)".*/\1/')
    [ -n "$said" ] || return 0
    "$bb" wait "$said" --timeout 2m >/dev/null 2>&1 || "$bb" cancel "$said" >/dev/null 2>&1
    "$bb" logs "$said" 2>/dev/null
}
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

# --- the same command once more, as the install line gives it when it is run again to update
# a server: nothing is installed twice, the relay that runs is started again with the program
# that is there now, and it is still the relay it was
entered() { systemctl show -p ActiveEnterTimestampMonotonic --value buildbalancer-relay; }
was=$(entered)
sudo "$bin/bb-relay" service install --listen "127.0.0.1:$port" >"$work/again.out" 2>&1
check "bb-relay service install on a relay that has its service" 0 $?
check "... says that it was there" 1 "$(grep -c "was installed already" "$work/again.out")"
check "... has started the relay again" yes "$([ "$(entered)" != "$was" ] && echo yes)"
check "... which says what it runs now" 1 "$(grep -c "^It runs version " "$work/again.out")"
check "... and is the relay it was: its main machine is still admitted" 1 "$(grep -c "device(s) admitted" "$work/again.out")"

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
check "jobs run as that user, not as root" "$me" "$(says 'id -un')"
check "... with that user's own tools on the PATH" 1 "$(says 'echo "$PATH"' | grep -c "$HOME/.cargo/bin")"
check "... in that user's data folder" 1 "$(ls -d "$own/worker/ws" 2>/dev/null | wc -l | tr -d ' ')"
cd "$work" || exit 1
sudo "$bin/bb-worker" service install >"$work/wagain.out" 2>&1
check "bb-worker service install on a worker that has its service" 0 $?
check "... stops nothing: a worker comes to new programs by itself" 1 "$(grep -c "is installed already and running" "$work/wagain.out")"

# --- an update never stops the service: the worker sees that another file took its program's
# place (here a copy of itself), ends with an error once no job runs, and systemd starts it
starts() {
    n=$(grep -c "version .* started" "$own/worker/logs/worker.log" 2>/dev/null)
    echo "${n:-0}"
}
# (counted from here: a start before this one says nothing about the replacing)
before=$(starts)
started_again() { [ "$(starts)" -gt "$before" ] && echo yes; }
cp "$bin/bb-worker" "$bin/bb-worker.next" && mv "$bin/bb-worker.next" "$bin/bb-worker"
check "the worker's service starts again by itself when its program is replaced" yes "$(wait_for 60 started_again)"
wait_for 40 online >/dev/null
check "... and runs jobs again" "$me" "$(cd "$work/proj" && says 'id -un')"

sudo "$bin/bb-worker" service uninstall >/dev/null
check "bb-worker service uninstall" 0 $?
sudo "$bin/bb-relay" service uninstall >/dev/null
check "bb-relay service uninstall" 0 $?
rm -rf "$own"

# --- a server from nothing: one command installs the relay, starts it and prints the line.
# (From a folder of its own: the install line below replaces the programs in it as root.)
prog="$work/programs"
mkdir -p "$prog" "$work/pack" "$work/release"
for p in bb bb-worker bb-relay BuildBalancer; do
    cp "$bin/$p" "$prog/"
    cp "$bin/$p" "$work/pack/"
done
sudo "$prog/bb-relay" service install --listen "127.0.0.1:$port" >"$work/fresh.out" 2>&1
check "one command sets a server up" 0 $?
key=$(key_in "$work/fresh.out")
check "... and prints the line for the main machine" 1 "$([ -n "$key" ] && echo 1 || echo 0)"
check "... which bb-relay status shows again" "$key" "$(sudo "$prog/bb-relay" status | grep "bb setup" | head -1 | awk '{print $4}')"
check "asked without sudo, the relay is not simply missing" 1 "$("$prog/bb-relay" status 2>&1 | grep -c "$system/relay: run this command with sudo")"

# --- the install line run again on that server, to update it: it fetches a release (here one
# packed from the programs under test), replaces the programs and ends in `bb-relay service
# install` -- which finds the service there, and must leave the relay running what was fetched
python3 - "$work" "$(uname -m)" <<'EOF'
import hashlib, os, shutil, sys
work, arch = sys.argv[1:3]
made = shutil.make_archive(os.path.join(work, "release", "buildbalancer-linux-" + arch), "gztar", os.path.join(work, "pack"))
with open(made, "rb") as f:
    digest = hashlib.sha256(f.read()).hexdigest()
with open(os.path.join(work, "release", "SHA256SUMS"), "w", newline="\n") as f:
    f.write("%s  %s\n" % (digest, os.path.basename(made)))
EOF
python3 "$root/tools/serve.py" "$web" "$work/release" >/dev/null 2>&1 &
web_pid=$!
served() { python3 -c "import sys, urllib.request; urllib.request.urlopen(sys.argv[1], timeout=3).read()" "http://127.0.0.1:$web/SHA256SUMS" >/dev/null 2>&1 && echo yes; }
wait_for 60 served >/dev/null || die "the test's own web server did not come up" /dev/null
line() { sudo env BB_RELEASE_URL="http://127.0.0.1:$web" BB_INSTALL_DIR="$prog" sh "$root/install.sh" "$@"; }
was=$(entered)
line relay --listen "127.0.0.1:$port" >"$work/line.out" 2>&1
check "the install line run again on a server ends well" 0 $?
check "... has replaced the programs" 1 "$(grep -c "^Installed into $prog" "$work/line.out")"
check "... and started the relay again with them" yes "$([ "$(entered)" != "$was" ] && echo yes)"
check "... which says what it runs now" 1 "$(grep -c "^It runs version " "$work/line.out")"
check "... and shows the line for the main machine as before" "$key" "$(key_in "$work/line.out")"
# the line that only fetches the programs must not leave the old relay running, either
was=$(entered)
line >"$work/plain.out" 2>&1
check "the plain install line ends well on a server" 0 $?
check "... and starts its relay again, too" yes "$([ "$(entered)" != "$was" ] && echo yes)"

echo
if [ "$fails" = 0 ]; then echo "all service checks passed"; else
    echo "$fails service check(s) failed"
    for f in install.out again.out root.out worker.out wagain.out fresh.out line.out plain.out; do
        echo "--- $f"
        cat "$work/$f" 2>/dev/null
    done
fi
exit "$fails"
