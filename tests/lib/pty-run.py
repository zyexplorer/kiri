#!/usr/bin/env python3
"""Runs a command on a terminal of its own, as `script -q /dev/null CMD...`
does on macOS: what comes on stdin is typed into the terminal, what the
terminal shows goes to stdout, and the exit code is the command's. For
tests/e2e.sh's terminal checks where BSD `script` is missing (Linux and WSL
have util-linux's, which takes other arguments).

    python3 tests/lib/pty-run.py docker run -it --rm alpine tty < /dev/null
"""
import os
import pty
import select
import sys

if len(sys.argv) < 2:
    sys.exit("usage: pty-run.py COMMAND [ARG...]")
pid, fd = pty.fork()
if pid == 0:
    try:
        os.execvp(sys.argv[1], sys.argv[1:])
    except OSError as e:
        sys.stderr.write("pty-run: %s: %s\n" % (sys.argv[1], e.strerror))
        os._exit(127)
out = sys.stdout.buffer
typing = True
while True:
    ready, _, _ = select.select([fd] + ([0] if typing else []), [], [], 0.2)
    if fd in ready:
        try:
            data = os.read(fd, 65536)
        except OSError:  # EIO: the command and everything it started closed the terminal
            data = b""
        if not data:
            break
        out.write(data)
        out.flush()
    if 0 in ready:
        data = os.read(0, 65536)
        if data:
            os.write(fd, data)
        else:
            # The end of stdin is not the end of the session: the command
            # keeps its terminal until it exits, as under `script`.
            typing = False
_, status = os.waitpid(pid, 0)
sys.exit(os.WEXITSTATUS(status) if os.WIFEXITED(status) else 128 + os.WTERMSIG(status))
