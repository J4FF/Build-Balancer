#!/bin/sh
# The Debian package (tools/mkdeb.py) for real: it is built from a folder of programs, apt
# installs it, the programs are in /usr/bin and the app is in the menu, an update of their own
# is refused with what to do instead, and apt removes the package without a trace.
#
#   BB_DEB_TEST=1 tools/deb-test.sh FOLDER_WITH_THE_PROGRAMS
#
# For a CI runner or a throwaway machine only: it installs and removes a package with sudo.
set -u
[ "${BB_DEB_TEST:-}" = 1 ] || {
    echo "this test installs a package with sudo: set BB_DEB_TEST=1 on a machine made for it"
    exit 2
}
command -v dpkg >/dev/null 2>&1 || {
    echo "this test is for Debian and Ubuntu (dpkg)"
    exit 2
}
if dpkg -s buildbalancer >/dev/null 2>&1; then
    echo "the package buildbalancer is installed here already: not a place for this test"
    exit 2
fi
root=$(cd "$(dirname "$0")/.." && pwd)
bin=$(cd "${1:?the folder with the programs}" && pwd)
version=$(grep -m1 '^version' "$root/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')
port=${BB_DEB_PORT:-7790}
work=$(mktemp -d) || exit 1
[ -n "$work" ] && [ -d "$work" ] || {
    echo "no temporary folder could be made"
    exit 1
}
web_pid=""
fails=0

cleanup() {
    [ -n "$web_pid" ] && kill -9 "$web_pid" 2>/dev/null
    sudo apt-get remove -y buildbalancer >/dev/null 2>&1
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

deb="$work/buildbalancer-linux-x86_64.deb"
python3 "$root/tools/mkdeb.py" "$bin" "$version" --arch "$(dpkg --print-architecture)" --out "$deb" >"$work/mkdeb.out" 2>&1
check "tools/mkdeb.py builds the package" 0 $?
check "dpkg-deb reads it" "$version" "$(dpkg-deb -f "$deb" Version 2>/dev/null)"
check "... with every program" 4 "$(dpkg-deb -c "$deb" | grep -c '/usr/bin/\(bb\|bb-worker\|bb-relay\|BuildBalancer\)$')"

sudo apt-get install -y "$deb" >"$work/install.out" 2>&1
check "apt installs it" 0 $?
check "the programs are in /usr/bin and run" "bb $version" "$(/usr/bin/bb --version 2>/dev/null | awk '{print $1" "$2}')"
for p in bb-worker bb-relay BuildBalancer; do
    check "... $p as well" yes "$([ -x "/usr/bin/$p" ] && echo yes)"
done
check "the app is in the menu" yes "$([ -f /usr/share/applications/BuildBalancer.desktop ] && echo yes)"
check "... with its icon" yes "$([ -f /usr/share/icons/hicolor/256x256/apps/buildbalancer.png ] && echo yes)"
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate /usr/share/applications/BuildBalancer.desktop >"$work/desktop.out" 2>&1
    check "the menu entry is valid (desktop-file-validate)" 0 $?
fi

# an update of their own is refused: the package manager replaces what it installed (a "newer
# release" here only names its version -- the refusal comes before anything is fetched)
mkdir -p "$work/release" && echo "99.0.0" >"$work/release/VERSION"
python3 "$root/tools/serve.py" "$port" "$work/release" >/dev/null 2>&1 &
web_pid=$!
served() { python3 -c "import sys, urllib.request; urllib.request.urlopen(sys.argv[1], timeout=3).read()" "http://127.0.0.1:$port/VERSION" >/dev/null 2>&1 && echo yes; }
wait_for 60 served >/dev/null || {
    echo "FAIL  the test's own web server did not come up"
    exit 1
}
BB_RELEASE_URL="http://127.0.0.1:$port" /usr/bin/bb update >"$work/update.out" 2>&1
check "an update of its own is refused" 1 "$(grep -c "installed here as a Debian package" "$work/update.out")"
check "... and the programs stay as they are" "bb $version" "$(/usr/bin/bb --version 2>/dev/null | awk '{print $1" "$2}')"

sudo apt-get remove -y buildbalancer >"$work/remove.out" 2>&1
check "apt removes it" 0 $?
check "... and nothing of it is left" 0 "$(ls /usr/bin/bb /usr/bin/bb-worker /usr/bin/bb-relay /usr/bin/BuildBalancer /usr/share/applications/BuildBalancer.desktop 2>/dev/null | wc -l | tr -d ' ')"

echo
if [ "$fails" = 0 ]; then echo "all package checks passed"; else
    echo "$fails package check(s) failed"
    for f in mkdeb.out install.out desktop.out update.out remove.out; do
        echo "--- $f"
        cat "$work/$f" 2>/dev/null
    done
fi
exit "$fails"
