#!/usr/bin/env python3
"""A folder over http on this machine only, for the tests that fetch a release made on the spot.

    python tools/serve.py PORT FOLDER

Python's own `http.server` asks for the machine's full name before it listens, which takes
minutes on some systems (a Mac without a name server for its own address); this one listens
at once.
"""
import functools
import http.server
import socketserver
import sys


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def main():
    port, folder = int(sys.argv[1]), sys.argv[2]
    with Server(("127.0.0.1", port), functools.partial(Quiet, directory=folder)) as server:
        server.serve_forever()


if __name__ == "__main__":
    main()
