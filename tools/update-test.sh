#!/bin/sh
# The programs updating themselves, end to end, against a "newer release" made on the spot:
# `bb update` puts it in the programs' place (also of the one that runs), a worker that the
# main machine asks to update fetches it and starts again by itself, `bb-worker update` has
# the worker that runs start again, and a download that does not match its checksum changes
# nothing.  Everything lies in one temporary folder.
#
#   tools/update-test.sh FOLDER_WITH_THE_PROGRAMS
#
# Needs python (python3) for the archive and the little web server.
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
bin=$(cd "${1:?the folder with the programs}" && pwd)
case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN*) os=windows arch=x86_64 ext=.exe win=1 ;;
Darwin) os=macos arch=$(uname -m | sed 's/arm64/aarch64/') ext="" win=0 ;;
*) os=linux arch=$(uname -m) ext="" win=0 ;;
esac
py=python3
command -v python3 >/dev/null 2>&1 || py=python
web=${BB_UPDATE_PORT:-7794}
port=${BB_UPDATE_RELAY_PORT:-7795}
work=$(mktemp -d) || exit 1
# (everything below is cleared away by the name of this folder: without one, nothing may run)
[ -n "$work" ] && [ -d "$work" ] || {
    echo "no temporary folder could be made"
    exit 1
}
[ "$win" = 1 ] && work=$(cygpath -m "$work") && bin=$(cygpath -m "$bin") && root=$(cygpath -m "$root")
tag=$(basename "$work")
export MSYS_NO_PATHCONV=1
export BB_CONFIG="$work/main"
export BB_RELEASE_URL="http://127.0.0.1:$web"
inst="$work/programs"
bb="$inst/bb$ext"
fails=0

cleanup() {
    # Everything this test started names its folder on its command line -- also the worker
    # that started itself again, which is no child of this script any more.  The folder's
    # name is the pattern: never an empty or a short one, which would match what is not ours.
    case "$tag" in tmp.????*) ;; *) return ;; esac
    if [ "$win" = 1 ]; then
        powershell -NoProfile -Command "Get-CimInstance Win32_Process | Where-Object { \$_.CommandLine -like '*$tag*' -and \$_.ProcessId -ne \$PID } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" >/dev/null 2>&1
    else
        pkill -9 -f "$tag" 2>/dev/null
    fi
    sleep 1
    rm -rf "$work"
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

# the programs as they are installed on a machine, and a release that claims to be newer
mkdir -p "$inst" "$work/pack" "$work/release"
for p in bb bb-worker bb-relay BuildBalancer; do
    cp "$bin/$p$ext" "$inst/"
    cp "$bin/$p$ext" "$work/pack/"
done
"$py" - "$work" "$os" "$arch" <<'EOF'
import hashlib, os, shutil, sys
work, system, arch = sys.argv[1:4]
name = "buildbalancer-%s-%s" % (system, arch)
made = shutil.make_archive(os.path.join(work, "release", name), "zip" if system == "windows" else "gztar", os.path.join(work, "pack"))
with open(made, "rb") as f:
    digest = hashlib.sha256(f.read()).hexdigest()
with open(os.path.join(work, "release", "SHA256SUMS"), "w", newline="\n") as f:
    f.write("%s  %s\n" % (digest, os.path.basename(made)))
with open(os.path.join(work, "release", "VERSION"), "w", newline="\n") as f:
    f.write("99.0.0\n")
EOF
"$py" "$root/tools/serve.py" "$web" "$work/release" >/dev/null 2>&1 &
# (it listens at once; the wait is for a machine that is slow to start python)
served() { "$py" -c "import sys, urllib.request; urllib.request.urlopen(sys.argv[1], timeout=3).read()" "$BB_RELEASE_URL/VERSION" >/dev/null 2>&1 && echo yes; }
wait_for 180 served >/dev/null || die "the test's own web server did not come up" /dev/null

# --- this machine
check "bb update --check sees the newer release" 1 "$("$bb" update --check 2>&1 | grep -c "Version 99.0.0 is available")"
# (the release holds the same programs as the folder, so what shows that they were replaced
# is that the files are new ones)
: >"$work/before"
sleep 2
"$bb" update >"$work/update.out" 2>&1
check "bb update installs it" 0 $?
check "... and says so" 1 "$(grep -c "Version 99.0.0 is installed" "$work/update.out")"
for p in bb bb-worker bb-relay BuildBalancer; do
    check "$p was replaced by the release's" "new and equal" "$([ "$inst/$p$ext" -nt "$work/before" ] && cmp -s "$inst/$p$ext" "$work/pack/$p$ext" && echo "new and equal")"
done
check "the programs still run" 1 "$("$bb" --version 2>/dev/null | grep -c "^bb ")"
left() { ls -a "$inst" | grep -c '\.old$\|\.new$\|update\.journal$\|update\.probe$'; }
check "nothing of the update is left lying around" 0 "$(left)"
# (the lock's file stays for the next update: a lock is on the file, not on its name)
check "... but the file its lock is on" 1 "$(ls -a "$inst" | grep -c 'update\.lock$')"

# --- an update that was cut short is taken back whole by the next program that starts.  Made
# by hand: the relay's program replaced and its old one kept, the other three not yet reached,
# and the note of what the update was about to do still there.
cp "$inst/bb-relay$ext" "$inst/bb-relay$ext.old"
echo "half an update" >"$inst/bb-relay$ext"
for p in bb bb-worker bb-relay BuildBalancer; do printf '%s\t%s\n' "$p$ext" "$p$ext.old"; done >"$inst/.buildbalancer-update.journal"
"$bb" --version >/dev/null 2>&1
check "an update that was cut short is taken back by the next program that starts" same "$(cmp -s "$inst/bb-relay$ext" "$work/pack/bb-relay$ext" && echo same)"
check "... and leaves nothing behind" 0 "$(left)"

# --- a worker that runs: the main machine asks it to update, and it starts again by itself
"$inst/bb-relay$ext" --listen "127.0.0.1:$port" --data "$work/relay" >"$work/relay.out" 2>&1 &
setup_key() { grep "bb setup" "$work/relay.out" 2>/dev/null | head -1 | awk '{print $4}'; }
key=$(wait_for 20 setup_key) || die "the relay printed no setup key" "$work/relay.out"
"$bb" setup "127.0.0.1:$port" "$key" --name update-main >/dev/null || die "setup failed" "$work/relay.out"
"$bb" pair >"$work/pair.out" 2>&1 &
pair_code() { grep "bb-worker join" "$work/pair.out" 2>/dev/null | awk '{print $4}'; }
code=$(wait_for 20 pair_code) || die "bb pair printed no code" "$work/pair.out"
"$inst/bb-worker$ext" join "127.0.0.1:$port" "$code" --data "$work/worker" --name update-worker --cpu 2 --priority normal >"$work/worker.out" 2>&1 &
online() { "$bb" workers --json | grep '"online": true'; }
wait_for 30 online >/dev/null || die "the worker did not come online" "$work/worker.out"
log="$work/worker/logs/worker.log"
restarts() { grep -c "starting again with the new programs" "$log" 2>/dev/null; }
started() { grep -c "connected to the relay" "$log" 2>/dev/null; }
restarted() { [ "$(restarts)" = "$1" ] && [ "$(started)" -gt "$1" ] && echo yes; }

# the word to a worker needs no way to the releases from here: the worker looks for itself
BB_RELEASE_URL="http://127.0.0.1:9" "$bb" update -w update-worker >/dev/null
check "bb update -w asks the worker, also where the releases cannot be reached" 0 $?
check "the worker fetches the release and starts again by itself" yes "$(wait_for 150 restarted 1)"
wait_for 30 online >/dev/null
check "... and is back: a job runs on it" hello "$(cd "$work" && "$bb" run -q --no-sync --sh "echo hello" 2>/dev/null | tr -d '\r')"

# --- `bb-worker update` on the worker's machine: the programs, then the worker that runs
"$inst/bb-worker$ext" update --data "$work/worker" >"$work/wupdate.out" 2>&1
check "bb-worker update installs" 0 $?
check "... and has the running worker start again" yes "$(wait_for 60 restarted 2)"
wait_for 30 online >/dev/null
check "... which is back as well" hello "$(cd "$work" && "$bb" run -q --no-sync --sh "echo hello" 2>/dev/null | tr -d '\r')"

# --- an update cuts no job off: one that runs is waited for, one that arrives is not lost
if [ "$win" = 1 ]; then nap='ping -n 13 127.0.0.1 >nul'; else nap='sleep 12'; fi
job_id() { grep '"job"' | sed 's/.*: "\(.*\)".*/\1/'; }
state_of() { "$bb" status "$1" --json | grep '"state"' | sed 's/.*: "\(.*\)".*/\1/'; }
in_state() { [ "$(state_of "$1")" = "$2" ] && echo yes; }
running=$(cd "$work" && "$bb" run -d --json --no-sync --sh "$nap" | job_id)
# (without a job that runs, nothing below would test what it says)
wait_for 30 in_state "$running" running >/dev/null || die "the job did not get to run before the update was asked for" "$work/worker.out"
BB_RELEASE_URL="http://127.0.0.1:9" "$bb" update -w update-worker >/dev/null
check "... the job still runs when the update is asked for" running "$(state_of "$running")"
arriving=$(cd "$work" && "$bb" run -d --json --no-sync --sh "echo late" | job_id)
(cd "$work" && "$bb" wait "$running" "$arriving" --timeout 3m >/dev/null 2>&1)
check "a job that runs when the update is asked for runs to its end" done "$(state_of "$running")"
check "a job that arrives meanwhile is not lost" done "$(state_of "$arriving")"
check "... and the worker updates and starts again once they are through" yes "$(wait_for 150 restarted 3)"

# --- a worker sees by itself that its program was replaced: an update made from elsewhere
# (`bb update` here; an installer, another user) has no way to ask it, and needs none
wait_for 30 online >/dev/null
"$bb" update >/dev/null 2>&1
check "a worker whose program another update replaced starts again by itself" yes "$(wait_for 90 restarted 4)"
wait_for 30 online >/dev/null
check "... and is back as well" hello "$(cd "$work" && "$bb" run -q --no-sync --sh "echo hello" 2>/dev/null | tr -d '\r')"

# --- a download that does not match its checksum changes nothing
before=$(cksum <"$bb")
echo "x" >>"$work/release/$(ls "$work/release" | grep buildbalancer-)"
"$bb" update >"$work/bad.out" 2>&1
check "an archive that does not match its checksum is refused" 1 "$(grep -c "does not match the release's checksum" "$work/bad.out")"
check "... and the programs stay as they were" "$before" "$(cksum <"$bb")"

echo
if [ "$fails" = 0 ]; then echo "all update checks passed"; else
    echo "$fails update check(s) failed"
    for f in update.out wupdate.out bad.out worker.out; do
        echo "--- $f"
        cat "$work/$f" 2>/dev/null
    done
    echo "--- worker.log"
    tail -30 "$log" 2>/dev/null
fi
exit "$fails"
