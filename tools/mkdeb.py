#!/usr/bin/env python3
"""A Debian package (.deb) of the four programs, with the app in the desktop's menu.

    python tools/mkdeb.py FOLDER_WITH_THE_PROGRAMS VERSION [--arch amd64|arm64] [--out FILE]

The programs go to /usr/bin, the app gets a menu entry (BuildBalancer.desktop: the name of
its program, which the app gives its windows as their app id and class) and the logo as its
icon (the 256 px picture of gui/res/app.ico and docs/img/logo.svg).  Nothing runs at
installation: the relay and a worker become services with `service install` as before.

Only Python is needed, no dpkg: a .deb is an `ar` archive of three members (debian-binary,
control.tar.gz, data.tar.gz).  The files belong to root and carry the time of the last
commit (SOURCE_DATE_EPOCH, else now), so the same programs give the same package.
"""
import argparse
import gzip
import hashlib
import io
import os
import struct
import sys
import tarfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PROGRAMS = ["bb", "bb-worker", "bb-relay", "BuildBalancer"]
PACKAGE = "buildbalancer"
HOMEPAGE = "https://github.com/J4FF/Build-Balancer"
MAINTAINER = "J4FF <86849624+J4FF@users.noreply.github.com>"

DESKTOP = """[Desktop Entry]
Type=Application
Name=Build Balancer
GenericName=Build distribution
Comment=Spread builds over the machines you own
Exec=BuildBalancer
Icon=buildbalancer
Terminal=false
Categories=Development;Building;
Keywords=build;compile;worker;relay;
StartupWMClass=BuildBalancer
"""

DESCRIPTION = """Spread builds over the machines you own
 Build Balancer sends a project's builds and tests to other machines of its
 owner and brings the results back: a relay that every machine can reach, a
 main machine that hands out the work (bb, and the app), and workers that lend
 processors, memory and disk within the limits their owner sets (bb-worker,
 and the app).  Everything between the machines is end-to-end encrypted.
 .
 This package holds the desktop app (BuildBalancer) and the programs bb,
 bb-worker and bb-relay.  A worker or the relay starts with the machine after
 `sudo bb-worker service install` or `sudo bb-relay service install`."""


def icon_png():
    """the 256 x 256 picture of the app's icon (a PNG inside gui/res/app.ico)"""
    ico = open(os.path.join(ROOT, "gui", "res", "app.ico"), "rb").read()
    _, _, count = struct.unpack("<HHH", ico[:6])
    for i in range(count):
        w, h, _, _, _, _, size, off = struct.unpack("<BBBBHHII", ico[6 + 16 * i:22 + 16 * i])
        data = ico[off:off + size]
        if (w or 256) == 256 and data[:8] == b"\x89PNG\r\n\x1a\n":
            return data
    sys.exit("gui/res/app.ico holds no 256 px PNG picture (tools/mkicon.py makes it)")


def copyright_text():
    """the copyright file Debian asks for: the project's two licences"""
    head = (
        "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n"
        "Upstream-Name: Build Balancer\n"
        "Source: " + HOMEPAGE + "\n\n"
        "Files: *\n"
        "Copyright: the Build Balancer authors\n"
        "License: MIT or Apache-2.0\n\n"
    )
    parts = [head]
    for name in ["LICENSE-MIT", "LICENSE-APACHE"]:
        parts.append("---- " + name + " ----\n\n" + open(os.path.join(ROOT, name), encoding="utf-8").read() + "\n")
    return "".join(parts).encode("utf-8")


def tar_gz(entries, mtime):
    """a .tar.gz of (path, data or None for a folder, mode) -- every file root's"""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as t:
        for path, data, mode in entries:
            info = tarfile.TarInfo(path)
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = mtime
            info.mode = mode
            if data is None:
                info.type = tarfile.DIRTYPE
                t.addfile(info)
            else:
                info.size = len(data)
                t.addfile(info, io.BytesIO(data))
    out = io.BytesIO()
    # (no file name and a fixed time in the gzip header: the same input, the same bytes)
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=mtime, filename="") as g:
        g.write(raw.getvalue())
    return out.getvalue()


def ar(members, mtime):
    """an ar archive (the format of a .deb) of (name, data)"""
    out = io.BytesIO()
    out.write(b"!<arch>\n")
    for name, data in members:
        head = "%-16s%-12d%-6d%-6d%-8s%-10d`\n" % (name, mtime, 0, 0, "100644", len(data))
        out.write(head.encode("ascii"))
        out.write(data)
        if len(data) % 2:
            out.write(b"\n")
    return out.getvalue()


def main():
    ap = argparse.ArgumentParser(description="a .deb of Build Balancer's programs")
    ap.add_argument("folder", help="the folder with bb, bb-worker, bb-relay and BuildBalancer (Linux builds)")
    ap.add_argument("version", help="the version the programs have, like 0.1.6")
    ap.add_argument("--arch", default="amd64", choices=["amd64", "arm64"])
    ap.add_argument("--out", help="the file to write (default: buildbalancer-linux-x86_64.deb / -aarch64.deb)")
    a = ap.parse_args()

    mtime = int(os.environ.get("SOURCE_DATE_EPOCH") or time.time())
    files = []  # (path below /, data, mode)
    for p in PROGRAMS:
        path = os.path.join(a.folder, p)
        data = open(path, "rb").read()
        # a Linux program: ELF, not the Windows or macOS one of the same name
        if data[:4] != b"\x7fELF":
            sys.exit("%s is no Linux program" % path)
        files.append(("usr/bin/" + p, data, 0o755))
    files += [
        ("usr/share/applications/BuildBalancer.desktop", DESKTOP.encode("utf-8"), 0o644),
        ("usr/share/icons/hicolor/256x256/apps/buildbalancer.png", icon_png(), 0o644),
        ("usr/share/icons/hicolor/scalable/apps/buildbalancer.svg", open(os.path.join(ROOT, "docs", "img", "logo.svg"), "rb").read(), 0o644),
        ("usr/share/doc/buildbalancer/copyright", copyright_text(), 0o644),
    ]

    # the folders, each once and before what is in it
    folders = []
    for path, _, _ in files:
        parts = path.split("/")[:-1]
        for i in range(1, len(parts) + 1):
            d = "/".join(parts[:i])
            if d not in folders:
                folders.append(d)
    data_entries = [("./", None, 0o755)] + [("./" + d + "/", None, 0o755) for d in folders] + [("./" + p, b, m) for p, b, m in files]

    size_kib = (sum(len(b) for _, b, _ in files) + 1023) // 1024
    control = "".join([
        "Package: %s\n" % PACKAGE,
        "Version: %s\n" % a.version,
        "Architecture: %s\n" % a.arch,
        "Maintainer: %s\n" % MAINTAINER,
        "Installed-Size: %d\n" % size_kib,
        "Section: devel\n",
        "Priority: optional\n",
        "Homepage: %s\n" % HOMEPAGE,
        "Description: %s\n" % DESCRIPTION,
    ])
    md5sums = "".join("%s  %s\n" % (hashlib.md5(b).hexdigest(), p) for p, b, _ in files)
    control_entries = [("./", None, 0o755), ("./control", control.encode("utf-8"), 0o644), ("./md5sums", md5sums.encode("utf-8"), 0o644)]

    deb = ar([("debian-binary", b"2.0\n"), ("control.tar.gz", tar_gz(control_entries, mtime)), ("data.tar.gz", tar_gz(data_entries, mtime))], mtime)
    out = a.out or "buildbalancer-linux-%s.deb" % ("x86_64" if a.arch == "amd64" else "aarch64")
    with open(out, "wb") as f:
        f.write(deb)
    print("%s: %s %s (%s), %d KiB" % (out, PACKAGE, a.version, a.arch, len(deb) // 1024))


if __name__ == "__main__":
    main()
