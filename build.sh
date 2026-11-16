#!/bin/sh
# Builds the three programs for Linux and Windows into dist/ (release, one file each).
#
#   ./build.sh            both systems
#   ./build.sh linux      Linux only          ./build.sh win      Windows only
#
# Needs rustup with the targets x86_64-unknown-linux-gnu and x86_64-pc-windows-msvc
# (rustup target add ...).  The linker is rust-lld, which rustup brings: no C toolchain and
# no Windows machine are needed.
set -e
root=$(cd "$(dirname "$0")" && pwd)
want=${1:-all}

# JFLib 0.2.1's build helper splits one linker argument of the Linux build at blanks, so a
# target folder below a path with a blank in it does not link.  Until that is fixed there,
# such a checkout builds into a folder without blanks.
target_dir="$root/target"
case "$target_dir" in
*" "*)
    target_dir="${TMPDIR:-/tmp}/buildbalancer-target"
    echo "note: building in $target_dir (the project path contains a blank)"
    ;;
esac
export CARGO_TARGET_DIR="$target_dir"

build() { # triple, folder in dist, file extension
    echo "== $1"
    (cd "$root" && cargo build --release --target "$1")
    mkdir -p "$root/dist/$2"
    for name in bb bb-relay bb-worker; do
        cp "$target_dir/$1/release/$name$3" "$root/dist/$2/"
    done
    ls -l "$root/dist/$2"
}

case "$want" in all | linux) build x86_64-unknown-linux-gnu linux-x86_64 "" ;; esac
case "$want" in all | win) build x86_64-pc-windows-msvc windows-x86_64 .exe ;; esac
