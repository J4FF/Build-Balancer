#!/bin/sh
# The install scripts against a release made on the spot: packs the programs of a folder the
# way the release workflow does, serves them from this machine, and lets install.sh (Linux,
# macOS) or install.ps1 (Windows, Git Bash) fetch, check and install them into a temporary
# folder.  Nothing outside that folder is touched (no PATH, no start menu entry).
#
#   tools/install-test.sh FOLDER_WITH_THE_PROGRAMS
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
port=${BB_INSTALL_PORT:-7793}
work=$(mktemp -d) || exit 1
[ -n "$work" ] && [ -d "$work" ] || {
    echo "no temporary folder could be made"
    exit 1
}
[ "$win" = 1 ] && work=$(cygpath -m "$work") && bin=$(cygpath -m "$bin") && root=$(cygpath -m "$root")
fails=0
server="" server_win=""
cleanup() {
    if [ -n "$server" ]; then
        kill -9 "$server" 2>/dev/null
        # (taskkill's options need the path conversion Git Bash would otherwise apply to them)
        [ "$win" = 1 ] && [ -n "$server_win" ] && taskkill //F //PID "$server_win" >/dev/null 2>&1
        wait "$server" 2>/dev/null
    fi
    rm -rf "$work"
}
trap cleanup EXIT
check() { # description, expected, got
    if [ "$2" = "$3" ]; then echo "ok    $1"; else
        echo "FAIL  $1: expected \"$2\", got \"$3\""
        fails=$((fails + 1))
    fi
}

mkdir -p "$work/pack" "$work/release" "$work/to"
for p in bb bb-worker bb-relay BuildBalancer; do cp "$bin/$p$ext" "$work/pack/"; done
"$py" - "$work" "$os" "$arch" <<'EOF'
import hashlib, os, shutil, sys
work, system, arch = sys.argv[1:4]
name = "buildbalancer-%s-%s" % (system, arch)
made = shutil.make_archive(os.path.join(work, "release", name), "zip" if system == "windows" else "gztar", os.path.join(work, "pack"))
with open(made, "rb") as f:
    digest = hashlib.sha256(f.read()).hexdigest()
with open(os.path.join(work, "release", "SHA256SUMS"), "w", newline="\n") as f:
    f.write("%s  %s\n" % (digest, os.path.basename(made)))
EOF
"$py" "$root/tools/serve.py" "$port" "$work/release" >/dev/null 2>&1 &
server=$!
[ "$win" = 1 ] && sleep 1 && server_win=$(cat "/proc/$server/winpid" 2>/dev/null)
# (it listens at once; the wait is for a machine that is slow to start python)
end=$(($(date +%s) + 180))
until "$py" -c "import sys, urllib.request; urllib.request.urlopen(sys.argv[1], timeout=3).read()" "http://127.0.0.1:$port/SHA256SUMS" >/dev/null 2>&1; do
    [ "$(date +%s)" -ge "$end" ] && echo "the test's own web server did not come up" && exit 1
    sleep 0.3
done

export BB_RELEASE_URL="http://127.0.0.1:$port" BB_INSTALL_DIR="$work/to" BB_NO_PATH=1
if [ "$win" = 1 ]; then
    powershell -NoProfile -ExecutionPolicy Bypass -File "$root/install.ps1" >"$work/out.txt" 2>&1
else
    sh "$root/install.sh" >"$work/out.txt" 2>&1
fi
check "the install script ends well" 0 $?
for p in bb bb-worker bb-relay BuildBalancer; do
    check "$p is installed" 1 "$(ls "$work/to/$p$ext" 2>/dev/null | wc -l | tr -d ' ')"
done
check "what was installed runs" "$("$bin/bb$ext" --version | tr -d '\r')" "$("$work/to/bb$ext" --version 2>/dev/null | tr -d '\r')"

# a download that was tampered with (or cut short) is not installed
rm -rf "$work/to" && mkdir "$work/to"
echo "x" >>"$work/release/$(ls "$work/release" | grep buildbalancer-)"
if [ "$win" = 1 ]; then
    powershell -NoProfile -ExecutionPolicy Bypass -File "$root/install.ps1" >"$work/bad.txt" 2>&1
else
    sh "$root/install.sh" >"$work/bad.txt" 2>&1
fi
check "a download that does not match its checksum is refused" 1 "$([ $? -ne 0 ] && echo 1 || echo 0)"
check "... and nothing of it is installed" 0 "$(ls "$work/to" | wc -l | tr -d ' ')"

echo
if [ "$fails" = 0 ]; then echo "all install checks passed"; else
    echo "$fails install check(s) failed"
    cat "$work/out.txt" "$work/bad.txt" 2>/dev/null
fi
exit "$fails"
