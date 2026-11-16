#!/bin/sh
# A whole fleet on one machine, end to end: relay, main machine, one worker, a few jobs.
# Run it after a build to see that the three programs still work together.
#
#   tools/smoke.sh [FOLDER WITH THE PROGRAMS]      default: dist/<this system>
#
# Works on Linux and in Git Bash on Windows.  Everything it makes lies in one temporary
# folder; it ends the relay and the worker it started.
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN*) sys=windows-x86_64 ext=.exe win=1 ;;
*) sys=linux-x86_64 ext="" win=0 ;;
esac
bin=${1:-$root/dist/$sys}
export MSYS_NO_PATHCONV=1
port=${BB_SMOKE_PORT:-7791}
work=$(mktemp -d)
[ "$win" = 1 ] && work=$(cygpath -m "$work")
export BB_CONFIG="$work/main"
bb="$bin/bb$ext"
fails=0

cleanup() {
    kill "$relay_pid" "$worker_pid" 2>/dev/null
    [ "$win" = 1 ] && taskkill //F //PID "$relay_winpid" //PID "$worker_winpid" >/dev/null 2>&1
    rm -rf "$work"
}
check() { # description, expected, got
    if [ "$2" = "$3" ]; then echo "ok    $1"; else echo "FAIL  $1: expected \"$2\", got \"$3\""; fails=$((fails + 1)); fi
}
winpid() { [ "$win" = 1 ] && cat "/proc/$1/winpid" 2>/dev/null; }

"$bin/bb-relay$ext" --listen "127.0.0.1:$port" --data "$work/relay" >"$work/relay.out" 2>&1 &
relay_pid=$!
sleep 2
relay_winpid=$(winpid $relay_pid)
worker_pid="" worker_winpid=""
trap cleanup EXIT

key=$(grep "bb setup" "$work/relay.out" | awk '{print $4}')
"$bb" setup "127.0.0.1:$port" "$key" --name smoke-main >/dev/null
check "setup" 0 $?

"$bb" pair >"$work/pair.out" 2>&1 &
pair_pid=$!
sleep 3
code=$(grep "bb-worker join" "$work/pair.out" | awk '{print $4}')
"$bin/bb-worker$ext" join "127.0.0.1:$port" "$code" --data "$work/worker" --name smoke-worker --cpu 2 --ram 2G --disk 1G --priority normal >/dev/null
check "pairing" 0 $?
wait "$pair_pid" 2>/dev/null

"$bin/bb-worker$ext" run --data "$work/worker" >"$work/worker.out" 2>&1 &
worker_pid=$!
sleep 4
worker_winpid=$(winpid $worker_pid)
check "worker online" 1 "$("$bb" workers --json | grep -c '"online": true')"

mkdir -p "$work/proj/src" "$work/proj/ignored"
cd "$work/proj" || exit 1
printf 'ignored/\n' >.gitignore
echo "hello" >src/in.txt
echo "no" >ignored/x.txt
"$bb" init --name smoke >/dev/null

if [ "$win" = 1 ]; then
    script='if exist ignored (echo ignored-was-sent) else (type src\in.txt > out.txt)'
    fail='exit 7'
else
    script='if [ -e ignored ]; then echo ignored-was-sent; else cat src/in.txt > out.txt; fi'
    fail='exit 7'
fi
"$bb" run -q --out out.txt --sh "$script"
check "job runs" 0 $?
check "output came back" "hello" "$(tr -d '\r' <out.txt 2>/dev/null)"

"$bb" run -q --sh "$fail" 2>/dev/null
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
check "job state as JSON" 1 "$("$bb" status "$job" --json | grep -c '"state": "done"')"

echo
if [ "$fails" = 0 ]; then echo "all checks passed"; else echo "$fails check(s) failed (logs: $work is removed; rerun with the programs' own output above)"; fi
exit "$fails"
