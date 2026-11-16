#!/bin/sh
# Builds the programs into dist/ (release, one file each): bb, bb-relay, bb-worker and the
# desktop app BuildBalancer.
#
#   ./build.sh            Linux and Windows -- on a Mac: macOS, Linux and Windows
#   ./build.sh linux      Linux only        ./build.sh win    Windows only
#   ./build.sh mac        macOS only (on a Mac)
#
# Needs rustup with the targets it builds (rustup target add x86_64-unknown-linux-gnu
# x86_64-pc-windows-msvc).  The linker for Linux and Windows is rust-lld, which rustup brings:
# no C toolchain and no Windows machine are needed.  The macOS programs link against the
# system's libraries, so they are built on a Mac (with Xcode's command line tools).
set -e
root=$(cd "$(dirname "$0")" && pwd)
want=${1:-all}
case "$want" in all | linux | win | mac) ;; *)
    echo "usage: ./build.sh [all | linux | win | mac]"
    exit 2
    ;;
esac

# JFLib 0.2.1's build helper splits one linker argument of the Linux build at blanks, so a
# target folder below a path with a blank in it does not link (fixed in JFLib after 0.2.1).
# Until then such a checkout builds into a folder without blanks.  A CARGO_TARGET_DIR the
# caller set is used as it is.
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
*" "*)
    # a folder of this user's own (a fixed name in /tmp could be someone else's), kept between
    # builds so they stay incremental
    target_dir="${XDG_CACHE_HOME:-$HOME/.cache}/buildbalancer-target"
    case "$target_dir" in *" "*) target_dir=$(mktemp -d) ;; esac
    case "$target_dir" in *" "*)
        echo "no folder without a blank to build in: set CARGO_TARGET_DIR to one"
        exit 1
        ;;
    esac
    mkdir -p "$target_dir" && chmod 700 "$target_dir"
    echo "note: building in $target_dir (the project path contains a blank)"
    ;;
esac

build() { # triple, folder in dist, file extension
    echo "== $1"
    (cd "$root" && CARGO_TARGET_DIR="$target_dir" cargo build --release --target "$1")
    mkdir -p "$root/dist/$2"
    for name in bb bb-relay bb-worker BuildBalancer; do
        cp "$target_dir/$1/release/$name$3" "$root/dist/$2/"
    done
    ls -l "$root/dist/$2"
}

on_mac=0
[ "$(uname -s)" = Darwin ] && on_mac=1
if [ "$want" = mac ] && [ "$on_mac" = 0 ]; then
    echo "the macOS programs are built on a Mac (they link against the system's libraries)"
    exit 1
fi
case "$want" in all | linux) build x86_64-unknown-linux-gnu linux-x86_64 "" ;; esac
case "$want" in all | win) build x86_64-pc-windows-msvc windows-x86_64 .exe ;; esac
if [ "$on_mac" = 1 ]; then
    case "$want" in all | mac)
        arch=$(uname -m | sed 's/arm64/aarch64/')
        build "$arch-apple-darwin" "macos-$arch" ""
        ;;
    esac
fi
