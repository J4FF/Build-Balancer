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
work=$(mktemp -d)
[ "$win" = 1 ] && work=$(cygpath -m "$work") && bin=$(cygpath -m "$bin")
export MSYS_NO_PATHCONV=1
export BB_CONFIG="$work/main"
export BB_RELEASE_URL="http://127.0.0.1:$web"
inst="$work/programs"
bb="$inst/bb$ext"
fails=0

cleanup() {
    # everything this test started names its folder on its command line -- also the worker
    # that started itself again, which is no child of this script any more
    if [ "$win" = 1 ]; then
        powershell -NoProfile -Command "Get-CimInstance Win32_Process | Where-Object { \$_.CommandLine -like '*$(basename "$work")*' -and \$_.ProcessId -ne \$PID } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" >/dev/null 2>&1
    else
        pkill -9 -f "$work" 2>/dev/null
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
"$py" -m http.server "$web" --bind 127.0.0.1 --directory "$work/release" >/dev/null 2>&1 &
sleep 2

# --- this machine
check "bb update --check sees the newer release" 1 "$("$bb" update --check 2>&1 | grep -c "Version 99.0.0 is available")"
"$bb" update >"$work/update.out" 2>&1
check "bb update installs it" 0 $?
check "... and says so" 1 "$(grep -c "Version 99.0.0 is installed" "$work/update.out")"
check "the programs still run" 1 "$("$bb" --version 2>/dev/null | grep -c "^bb ")"
check "nothing of the update is left lying around" 0 "$(ls -a "$inst" | grep -c '\.old$\|\.new$')"

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

"$bb" update -w update-worker >/dev/null
check "bb update -w asks the worker" 0 $?
check "the worker fetches the release and starts again by itself" yes "$(wait_for 150 restarted 1)"
wait_for 30 online >/dev/null
check "... and is back: a job runs on it" hello "$(cd "$work" && "$bb" run -q --no-sync --sh "echo hello" 2>/dev/null | tr -d '\r')"

# --- `bb-worker update` on the worker's machine: the programs, then the worker that runs
"$inst/bb-worker$ext" update --data "$work/worker" >"$work/wupdate.out" 2>&1
check "bb-worker update installs" 0 $?
check "... and has the running worker start again" yes "$(wait_for 60 restarted 2)"
wait_for 30 online >/dev/null
check "... which is back as well" hello "$(cd "$work" && "$bb" run -q --no-sync --sh "echo hello" 2>/dev/null | tr -d '\r')"

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
