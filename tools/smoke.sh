#!/bin/sh
# A whole fleet on one machine, end to end: relay, main machine, one worker, a few jobs.
# Run it after a build to see that the three programs still work together.
#
#   tools/smoke.sh [FOLDER WITH THE PROGRAMS]      default: dist/<this system>
#
# Works on Linux, on macOS and in Git Bash on Windows.  Everything it makes lies in one
# temporary folder; it ends the relay and the worker it started.
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN*) sys=windows-x86_64 ext=.exe win=1 ;;
Darwin) sys=macos-$(uname -m | sed 's/arm64/aarch64/') ext="" win=0 ;;
*) sys=linux-$(uname -m) ext="" win=0 ;;
esac
bin=${1:-$root/dist/$sys}
# absolute: the test changes into its project folder further down
bin=$(cd "$bin" 2>/dev/null && pwd) || {
    echo "no folder with the programs: ${1:-$root/dist/$sys} (build first, or name the folder)"
    exit 2
}
export MSYS_NO_PATHCONV=1
port=${BB_SMOKE_PORT:-7791}
work=$(mktemp -d)
[ "$win" = 1 ] && work=$(cygpath -m "$work")
export BB_CONFIG="$work/main"
bb="$bin/bb$ext"
fails=0
relay_pid="" relay_winpid="" worker_pid="" worker_winpid=""

stop() { # pid, Windows pid: end a program at once
    [ -n "$1" ] && kill -9 "$1" 2>/dev/null
    # (taskkill's options need the path conversion this script turns off for the programs)
    [ "$win" = 1 ] && [ -n "$2" ] && (unset MSYS_NO_PATHCONV && taskkill //F //PID "$2" >/dev/null 2>&1)
    return 0
}
cleanup() {
    stop "$relay_pid" "$relay_winpid"
    stop "$worker_pid" "$worker_winpid"
    rm -rf "$work"
}
trap cleanup EXIT
check() { # description, expected, got
    if [ "$2" = "$3" ]; then echo "ok    $1"; else
        echo "FAIL  $1: expected \"$2\", got \"$3\""
        fails=$((fails + 1))
    fi
}
winpid() { [ "$win" = 1 ] && cat "/proc/$1/winpid" 2>/dev/null; }
# Wait until a command prints something (at most SECONDS) -> what it printed.  The programs
# need a moment to start, and how long depends on the machine: nothing is read on a guess.
wait_for() { # seconds, command...
    tries=$(($1 * 5))
    shift
    while [ "$tries" -gt 0 ]; do
        got=$("$@" 2>/dev/null)
        if [ -n "$got" ]; then
            printf '%s' "$got"
            return 0
        fi
        sleep 0.2
        tries=$((tries - 1))
    done
    return 1
}
die() { # what is missing, the log that may say why
    echo "FAIL  $1"
    [ -f "$2" ] && sed 's/^/      | /' "$2"
    exit 1
}
setup_key() { grep "bb setup" "$work/relay.out" 2>/dev/null | awk '{print $4}'; }
pair_code() { grep "bb-worker join" "$work/pair.out" 2>/dev/null | awk '{print $4}'; }
online() { "$bb" workers --json | grep '"online": true'; }
state_of() { "$bb" status "$1" --json | grep '"state"' | sed 's/.*: "\(.*\)".*/\1/'; }
in_state() { [ "$(state_of "$1")" = "$2" ] && echo yes; }
kept() { ls "$work/worker/ws" 2>/dev/null | wc -l | tr -d ' '; }
nothing_kept() { [ "$(kept)" = 0 ] && echo yes; }
start_worker() {
    "$bin/bb-worker$ext" run --data "$work/worker" >>"$work/worker.out" 2>&1 &
    worker_pid=$!
    wait_for 30 online >/dev/null || die "the worker did not come online" "$work/worker.out"
    worker_winpid=$(winpid "$worker_pid")
}

start_relay() {
    "$bin/bb-relay$ext" --listen "127.0.0.1:$port" --data "$work/relay" >"$work/relay.out" 2>&1 &
    relay_pid=$!
    key=$(wait_for 20 setup_key) || die "the relay printed no setup key" "$work/relay.out"
    relay_winpid=$(winpid "$relay_pid")
}
start_relay
# the line someone copied must still work after the relay was restarted (a closed terminal, a
# service installed in between)
first_key=$key
exec 3>&2 2>/dev/null
stop "$relay_pid" "$relay_winpid"
wait "$relay_pid"
exec 2>&3 3>&-
start_relay
check "the setup key survives a restart of the relay" "$first_key" "$key"
check "bb-relay status shows the setup line again" 1 "$("$bin/bb-relay$ext" status --data "$work/relay" | grep -c "bb setup .* $key")"
"$bb" setup "127.0.0.1:$port" "$key" --name smoke-main >/dev/null
check "setup" 0 $?
check "a used setup key is not shown any more" 0 "$("$bin/bb-relay$ext" status --data "$work/relay" | grep -c "bb setup .* $key")"

"$bb" pair >"$work/pair.out" 2>&1 &
pair_pid=$!
code=$(wait_for 20 pair_code) || die "bb pair printed no code" "$work/pair.out"
# join pairs and goes on lending: a machine that joined is there without another command
"$bin/bb-worker$ext" join "127.0.0.1:$port" "$code" --data "$work/worker" --name smoke-worker --cpu 2 --ram 2G --disk 1G --priority normal >"$work/worker.out" 2>&1 &
worker_pid=$!
wait "$pair_pid" 2>/dev/null
check "pairing" 0 $?
wait_for 30 online >/dev/null || die "the worker did not come online after joining" "$work/worker.out"
worker_winpid=$(winpid "$worker_pid")
check "the machine lends right after joining" 1 "$(online | grep -c .)"

mkdir -p "$work/proj/src" "$work/proj/ignored"
cd "$work/proj" || exit 1
printf 'ignored/\n' >.gitignore
echo "hello" >src/in.txt
echo "no" >ignored/x.txt
"$bb" init --name smoke >/dev/null

if [ "$win" = 1 ]; then
    script='if exist ignored (echo ignored-was-sent) else (type src\in.txt > out.txt)'
    slow='ping -n 30 127.0.0.1 >nul'
else
    script='if [ -e ignored ]; then echo ignored-was-sent; else cat src/in.txt > out.txt; fi'
    slow='sleep 30'
fi
"$bb" run -q --out out.txt --sh "$script"
check "job runs" 0 $?
check "output came back" "hello" "$(tr -d '\r' <out.txt 2>/dev/null)"

"$bb" run -q --sh "exit 7" 2>/dev/null
check "exit code passes through" 7 $?

echo "changed" >src/in.txt
rm -f out.txt
"$bb" run -q --out out.txt --sh "$script"
check "changed file arrives" "changed" "$(tr -d '\r' <out.txt 2>/dev/null)"

"$bb" run -q -- no-such-program-here 2>/dev/null
check "missing program is an error" 1 $?

job=$("$bb" run -d --json --sh "$script" | grep '"job"' | sed 's/.*: "\(.*\)".*/\1/')
"$bb" wait "$job" >/dev/null
check "detached job, wait" 0 $?
check "job state as JSON" done "$(state_of "$job")"

# nothing stays behind: the main machine has the project removed, then the worker's owner does
check "the project is kept for the next job" 1 "$(kept)"
"$bb" clean --all >/dev/null
check "bb clean" 0 $?
check "removed on the main machine's word" yes "$(wait_for 20 nothing_kept)"
"$bb" run -q --sh "$script"
check "the project comes back with a job" 1 "$(kept)"
"$bin/bb-worker$ext" clean --all --data "$work/worker" >/dev/null
check "bb-worker clean (through the running worker)" 0 $?
check "removed on the worker" 0 "$(kept)"

# a worker that dies: the job that ran is reported, the one that waited runs after the restart
ran=$("$bb" run -d --json --sh "$slow" | grep '"job"' | sed 's/.*: "\(.*\)".*/\1/')
wait_for 30 in_state "$ran" running >/dev/null
waited=$("$bb" run -d --json --sh "$script" | grep '"job"' | sed 's/.*: "\(.*\)".*/\1/')
check "a second job waits its turn" yes "$(wait_for 30 in_state "$waited" queued)"
exec 3>&2 2>/dev/null # (the shell's own note that a program was killed is not a finding)
stop "$worker_pid" "$worker_winpid"
wait "$worker_pid"
exec 2>&3 3>&-
start_worker
"$bb" wait "$waited" >/dev/null
check "the job that waited runs after a restart" 0 $?
check "the job that ran is reported as interrupted" error "$(state_of "$ran")"

# a machine that only paired: the main machine says what is missing there
second_code() { grep "bb-worker join" "$work/pair2.out" 2>/dev/null | awk '{print $4}'; }
"$bb" pair >"$work/pair2.out" 2>&1 &
pair_pid=$!
code=$(wait_for 20 second_code) || die "bb pair printed no code" "$work/pair2.out"
"$bin/bb-worker$ext" join "127.0.0.1:$port" "$code" --data "$work/idle" --name smoke-idle --no-run >/dev/null
check "join --no-run pairs and ends" 0 $?
wait "$pair_pid" 2>/dev/null
check "a paired machine that never ran gets a hint" 1 "$("$bb" workers | grep -c "smoke-idle: paired, but it never came online")"

echo
if [ "$fails" = 0 ]; then echo "all checks passed"; else echo "$fails check(s) failed"; fi
exit "$fails"
