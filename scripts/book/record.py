#!/usr/bin/env python3
"""Record a book scenario as asciicast v2, wrapped for file:// loading.

    python3 scripts/book/record.py <scenario> [--demo-parent DIR] [--keep-demo]
    python3 scripts/book/record.py --check book/casts/<scene>.js ...

A scenario is `scripts/book/scenarios/<scenario>.py`. It defines
`SCENES` (the casts it produces) and `run(ctx)`, and drives one or more
pseudo-terminals through `ctx`. Every process runs in a throwaway demo
home with a scrubbed environment; see `demo_env`.

Each cast is written to `book/casts/<scene>.js` as
`(window.CLANK_CASTS ||= {})["<scene>"] = "<cast text>";` and only after
the reconstructed output passes `book/scrub-patterns.txt`.

Standard library only.
"""

import argparse
import bisect
import codecs
import collections
import fcntl
import getpass
import importlib.util
import json
import os
import random
import re
import select
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCENARIOS = Path(__file__).resolve().parent / "scenarios"
PATTERNS = REPO / "book" / "scrub-patterns.txt"
CASTS = REPO / "book" / "casts"

COLS, ROWS = 110, 32
IDLE_CAP = 1.5
# Anything quieter than this between two writes plays as one frame, which
# keeps busy TUI stretches from turning into thousands of cast lines.
COALESCE = 1 / 30
# clank caps `clank-<basename>` at this, hashing anything longer; a demo
# name must fit so the session name stays the run's own.
SESSION_NAME_MAX = 24
# Inherited identity that would bind a recording to a live session.
BANNED_ENV_PREFIXES = ("ZELLIJ", "CLANK", "CLAUDE", "CODEX")

ESCAPES = REPO / "book" / "scrub-escapes.txt"


def pattern_lines(path):
    return [
        line
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]


ANSI = re.compile("|".join(pattern_lines(ESCAPES)))


def load_patterns(path=PATTERNS):
    return [re.compile(line) for line in pattern_lines(path)]


def runtime_literals():
    """Machine and person names no fixed pattern can know."""
    names = {getpass.getuser()}
    host = socket.gethostname()
    names |= {host, host.split(".")[0]}
    try:
        real = subprocess.run(["id", "-F"], capture_output=True, text=True).stdout.strip()
    except OSError:
        real = ""
    if real:
        names.add(real)
        names |= set(real.split())
    return sorted(n for n in names if len(n) >= 4)


def scrub_hits(text, patterns, literals):
    """Every banned match in `text`, raw and with escapes stripped.

    The stripped pass is what catches a token a TUI drew in pieces with
    cursor moves between them.
    """
    hits = []
    for label, body in (("raw", text), ("stripped", ANSI.sub("", text))):
        for p in patterns:
            for m in p.finditer(body):
                hits.append((label, p.pattern, m.group(0)))
        lowered = body.lower()
        for lit in literals:
            if lit.lower() in lowered:
                hits.append((label, f"literal:{lit[:2]}…", lit))
    return hits


def redact(s):
    return s[:3] + "…" + f"({len(s)} chars)"


def rewrite_targets(demo_root):
    """(literal, replacement) pairs, longest literal first.

    Narrow panes elide long paths (`/…/tail` or `head…`), so each elided
    form of the demo root is mapped too; an elided real home is left for
    the scrub patterns to refuse.
    """
    pairs = {}

    def forms(p):
        out = {p.rstrip("/"), os.path.realpath(p).rstrip("/")}
        out |= {f[len("/private"):] for f in list(out) if f.startswith("/private/")}
        return {f for f in out if f and f != "/"}

    for root in forms(str(demo_root)):
        pairs[root] = "~"
        for i, ch in enumerate(root):
            if ch == "/" and i > 0:
                pairs["/…" + root[i:]] = "~"
                pairs["…" + root[i:]] = "~"
        for n in range(10, len(root)):
            pairs[root[:n] + "…"] = "~/…"
    for home in forms(str(Path.home())):
        pairs[home] = "~"
    return sorted(pairs.items(), key=lambda kv: len(kv[0]), reverse=True)


def rewrite_events(events, targets):
    """Apply `targets`' replacements, even where a PTY read split a match.

    An occurrence spanning an event boundary merges those events (keeping
    the earlier timestamp) before replacing, so no half path survives.
    """
    if not events:
        return events
    lookup = dict(targets)
    pat = re.compile("|".join(re.escape(k) for k, _ in targets))
    joined = "".join(d for _, d in events)
    starts = []
    off = 0
    for _, d in events:
        starts.append(off)
        off += len(d)
    merge_into_prev = [False] * len(events)
    for m in pat.finditer(joined):
        lo = bisect.bisect_right(starts, m.start())
        hi = bisect.bisect_left(starts, m.end())
        for i in range(lo, hi):
            merge_into_prev[i] = True
    merged = []
    for (t, d), join in zip(events, merge_into_prev):
        if join and merged:
            merged[-1] = (merged[-1][0], merged[-1][1] + d)
        else:
            merged.append((t, d))
    return [(t, pat.sub(lambda m: lookup[m.group(0)], d)) for t, d in merged]


class Term:
    """One pseudo-terminal: a recorded scene, or unrecorded plumbing."""

    def __init__(self, ctx, scene, argv, cwd, cols, rows, title, env):
        self.ctx, self.scene, self.cols, self.rows = ctx, scene, cols, rows
        self.title = title or scene
        self.rng = random.Random(scene or "plumbing")
        self.events = []
        self.realtime = []
        self.text = ""
        self.cursor = 0
        self.lock = threading.Lock()
        self.t0 = time.monotonic()
        pid, fd = os.forkpty()
        if pid == 0:
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
                os.chdir(cwd)
                os.execvpe(argv[0], argv, env)
            finally:
                os._exit(127)
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.tty = controlling_tty(pid)
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.alive = True
        self.closed = False
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def now(self):
        return time.monotonic() - self.t0

    def _answer_queries(self, chunk):
        # Without a real terminal behind the pty, TUIs that ask for the
        # cursor position or colours would stall waiting for an answer.
        replies = []
        if "\x1b[6n" in chunk:
            replies.append("\x1b[1;1R")
        if re.search(r"\x1b\[0?c", chunk):
            replies.append("\x1b[?62;22c")
        if "\x1b[>c" in chunk or "\x1b[>0c" in chunk:
            replies.append("\x1b[>0;0;0c")
        if "\x1b]10;?" in chunk:
            replies.append("\x1b]10;rgb:e6e6/e6e6/e6e6\x1b\\")
        if "\x1b]11;?" in chunk:
            replies.append("\x1b]11;rgb:1a1a/1b1b/1e1e\x1b\\")
        if "\x1b[18t" in chunk:
            replies.append(f"\x1b[8;{self.rows};{self.cols}t")
        if "\x1b[14t" in chunk:
            replies.append(f"\x1b[4;{self.rows * 18};{self.cols * 9}t")
        if "\x1b[16t" in chunk:
            replies.append("\x1b[6;18;9t")
        for r in replies:
            self._write(r)

    def _read(self):
        while True:
            try:
                r, _, _ = select.select([self.fd], [], [], 0.5)
                if not r:
                    if not self.alive:
                        return
                    continue
                data = os.read(self.fd, 65536)
            except OSError:
                data = b""
            if not data:
                self.alive = False
                return
            chunk = self.decoder.decode(data)
            if not chunk:
                continue
            self._answer_queries(chunk)
            with self.lock:
                self.events.append((self.now(), chunk))
                self.text += ANSI.sub("", chunk)

    def _write(self, s):
        try:
            os.write(self.fd, s.encode("utf-8"))
        except OSError:
            pass

    def _mark_realtime(self, start, end):
        self.realtime.append((start, end))

    def keys(self, s, after=0.6):
        start = self.now()
        self._write(s)
        time.sleep(after)
        self._mark_realtime(start, self.now())

    def type(self, text, enter=True, settle=1.2):
        """Type like a person: per-key jitter, a beat before Enter."""
        start = self.now()
        time.sleep(0.4)
        for ch in text:
            self._write(ch)
            base = 0.11 if ch == " " else 0.06
            time.sleep(base + self.rng.random() * 0.07)
        if enter:
            time.sleep(0.35)
            self._write("\r")
        time.sleep(settle)
        self._mark_realtime(start, self.now())

    def pause(self, seconds):
        """A hold the viewer sees at real speed."""
        start = self.now()
        time.sleep(seconds)
        self._mark_realtime(start, self.now())

    def expect(self, pattern, timeout=60, realtime=False):
        """Wait for `pattern` in the escape-stripped output after the last match."""
        rx = re.compile(pattern)
        start = self.now()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                m = rx.search(self.text, self.cursor)
                if m:
                    self.cursor = m.end()
                    if realtime:
                        self._mark_realtime(start, self.now())
                    return m
            if not self.alive:
                break
            time.sleep(0.2)
        tail = ANSI.sub("", self.text[-1500:])
        raise TimeoutError(f"{self.scene}: no /{pattern}/ within {timeout}s; tail:\n{tail}")

    def seen(self, pattern):
        with self.lock:
            return re.search(pattern, self.text) is not None

    def close(self):
        """Stop everything that was on this pty, then reap.

        The pty is ours by construction: while we hold its master no other
        terminal can be allocated that name, and its leader stays our
        unreaped child, so its pid cannot be reused either. A process seen
        on it stays ours while its pid keeps that start time, even after a
        hangup takes the terminal away from it.

        That authority ends here: once the master is released the tty name,
        the leader's pid and the fd number can all belong to someone else,
        so a second call does nothing.
        """
        if self.closed:
            return
        self.closed = True
        seen = {}
        for sig, grace in ((signal.SIGHUP, 2), (signal.SIGCONT, 0), (signal.SIGTERM, 3), (signal.SIGKILL, 1)):
            table = process_table()
            seen.update({pid: table[pid].start for pid in tty_members(table, self.tty)})
            targets = still_running(seen, table)
            if not targets:
                break
            for pid in targets:
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    pass
            deadline = time.monotonic() + grace
            while time.monotonic() < deadline and still_running(seen, process_table()):
                time.sleep(0.2)
        self.alive = False
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass
        self.reader.join(timeout=2)
        try:
            os.close(self.fd)
        except OSError:
            pass

    def cast(self, target_seconds, min_speed):
        """The asciicast v2 text, with idle capped and busy stretches sped up."""
        with self.lock:
            events = list(self.events)
        events = rewrite_events(events, self.ctx.rewrite)

        def is_rt(t):
            return any(a - 0.05 <= t <= b + 0.05 for a, b in self.realtime)

        gaps = []
        prev = 0.0
        for t, _ in events:
            gaps.append((min(t - prev, IDLE_CAP), is_rt(t)))
            prev = t
        rt_total = sum(g for g, rt in gaps if rt)
        ff_total = sum(g for g, rt in gaps if not rt)
        k = 1.0
        if ff_total > 0 and rt_total + ff_total > target_seconds:
            k = max(min_speed, (target_seconds - rt_total) / ff_total)
            k = min(k, 1.0)
        out = []
        clock = 0.0
        for (gap, rt), (_, data) in zip(gaps, events):
            step = gap if rt else gap * k
            if out and step < COALESCE:
                clock += step
                out[-1][1] += data
                continue
            clock += step
            out.append([clock, data])
        header = {
            "version": 2,
            "width": self.cols,
            "height": self.rows,
            "title": self.title,
            "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"},
        }
        lines = [json.dumps(header)]
        for t, data in out:
            lines.append(json.dumps([round(t, 3), "o", data], ensure_ascii=False))
        lines.append(json.dumps([round(clock + 2.5, 3), "o", ""]))
        return "\n".join(lines) + "\n", clock + 2.5


Proc = collections.namedtuple("Proc", "tty start zombie", defaults=(False,))


def parse_etime(s):
    days, _, rest = s.strip().rpartition("-")
    parts = [int(x) for x in rest.split(":")]
    while len(parts) < 3:
        parts.insert(0, 0)
    h, m, sec = parts
    return (int(days) if days else 0) * 86400 + h * 3600 + m * 60 + sec


def process_table():
    """pid -> Proc(controlling tty, start time in epoch seconds), one snapshot."""
    out = subprocess.run(["ps", "-axo", "pid=,tty=,stat=,etime="], capture_output=True, text=True).stdout
    now = time.time()
    table = {}
    for line in out.splitlines():
        fields = line.split()
        if len(fields) == 4 and fields[0].isdigit():
            table[int(fields[0])] = Proc(fields[1], now - parse_etime(fields[3]), fields[2].startswith("Z"))
    return table


def tty_members(table, tty):
    """Pids whose controlling terminal is `tty`, a pty this run holds."""
    if not tty or tty in ("?", "??", "-"):
        return []
    return [pid for pid, p in table.items() if p.tty == tty]


def controlling_tty(pid, timeout=3):
    """The pty `pid` was forked onto, once the child has acquired it."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        proc = process_table().get(pid)
        if proc and tty_members({pid: proc}, proc.tty):
            return proc.tty
        time.sleep(0.05)
    raise RuntimeError(f"pid {pid} never acquired its pty")


def still_running(seen, table, slack=1):
    """Pids from `seen` (pid -> start) that are live with the same start."""
    return sorted(
        pid
        for pid, start in seen.items()
        if pid in table and not table[pid].zombie and abs(table[pid].start - start) <= slack
    )


def verified_monitors(recorded, table, slack=3):
    """Recorded monitor pids still running with the start clank recorded.

    `recorded` maps pid -> birth (epoch seconds) as the repo's own monitor
    record states it; a pid now held by a process started at another time
    is someone else's.
    """
    return still_running(recorded, table, slack)


def demo_env(demo_root):
    """A from-scratch environment: nothing inherited names a live session."""
    env = {
        "HOME": str(demo_root),
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "USER": getpass.getuser(),
        "LOGNAME": getpass.getuser(),
        "SHELL": "/bin/bash",
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "LANG": "en_US.UTF-8",
        "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
        "BASH_SILENCE_DEPRECATION_WARNING": "1",
        "PS1": r"\[\e[1;34m\]\w\[\e[0m\] \[\e[1;32m\]$\[\e[0m\] ",
        "CLICOLOR": "1",
    }
    bad = [k for k in env if k.startswith(BANNED_ENV_PREFIXES)]
    assert not bad, bad
    return env


def zellij_sessions(env):
    r = subprocess.run(
        ["zellij", "list-sessions", "--short", "--no-formatting"],
        env=env,
        capture_output=True,
        text=True,
    )
    return {line.strip() for line in r.stdout.splitlines() if line.strip()}


class Ctx:
    """One recording run and everything it owns.

    Ownership is retained from creation, never inferred: the ptys it
    opened, the repos it made (named with this run's id, so the zellij
    sessions `clank open` starts for them are unique to the run), and the
    monitors those repos' own records name. Teardown signals nothing else.
    """

    def __init__(self, demo_root, list_sessions=None):
        self.demo = Path(demo_root)
        # The fresh demo directory's own unique name: two concurrent runs
        # cannot hold the same one, so cannot share a repo or session name.
        self.run_id = re.sub(r"[^a-z0-9]", "", self.demo.name.lower().removeprefix("bd-"))[:8]
        assert self.run_id, self.demo
        self.env = demo_env(self.demo)
        self.names = {}
        self.terms = []
        self.repos = []
        self.real_home = str(Path.home())
        self._list_sessions = list_sessions or zellij_sessions
        self.sessions_before = self._list_sessions(self.env)
        self.owned_sessions = set()
        self._seed_home()

    @property
    def rewrite(self):
        pairs = dict(rewrite_targets(self.demo))
        pairs.update(self.names)
        return sorted(pairs.items(), key=lambda kv: len(kv[0]), reverse=True)

    def name(self, base):
        """`base` made unique to this run and short enough for a session name.

        Casts show it as `base`: `book-demo-shop` is `shop-<run id>` on disk.
        """
        unique = f"{base.removeprefix('book-demo-')}-{self.run_id}"
        assert len("clank-" + unique) <= SESSION_NAME_MAX, unique
        self.names[unique] = base
        return unique

    def _seed_home(self):
        (self.demo / ".gitconfig").write_text(
            "[user]\n\tname = Demo\n\temail = demo\n"
            "[init]\n\tdefaultBranch = main\n"
            "[commit]\n\tgpgsign = false\n"
            "[advice]\n\tdetachedHead = false\n"
        )
        (self.demo / ".clank").mkdir(exist_ok=True)
        (self.demo / ".clank" / "config.json").write_text('{\n  "auto": "on"\n}\n')
        zconf = self.demo / ".config" / "zellij"
        zconf.mkdir(parents=True, exist_ok=True)
        (zconf / "config.kdl").write_text(
            # The session card and indicator would list the user's own
            # sessions on screen.
            "session_card false\nsession_indicator false\n"
            "show_startup_tips false\nshow_release_notes false\n"
            "mouse_hover_tips false\nswap_layout_notification false\n"
            'default_mode "normal"\npane_frames true\n'
        )

    def own_session(self, repo, planned=None):
        """Declare the zellij session `clank open` will start for `repo`.

        `planned` is the name clank itself would spawn; by default it is
        asked with `clank open --print`.
        """
        assert Path(repo) in self.repos, repo
        name = planned or self._planned_session(repo)
        assert name == "clank-" + Path(repo).name and name.endswith(self.run_id), name
        if name in self._list_sessions(self.env):
            raise RuntimeError(f"{name} already exists")
        self.owned_sessions.add(name)
        return name

    def _planned_session(self, repo):
        r = self.sh("clank open --print", cwd=repo)
        m = re.search(r"^spawn: zellij --session (\S+) ", r.stderr, re.M)
        if not m:
            raise RuntimeError(f"`clank open --print` named no session:\n{r.stderr}")
        return m.group(1)

    def term(self, scene=None, argv=None, cwd=None, cols=COLS, rows=ROWS, title=None, env=None):
        argv = argv or ["/bin/bash", "--noprofile", "--norc", "-i"]
        merged = {**self.env, **(env or {})}
        bad = [k for k in merged if k.startswith(BANNED_ENV_PREFIXES)]
        assert not bad, bad
        t = Term(self, scene, argv, str(cwd or self.demo), cols, rows, title, merged)
        self.terms.append(t)
        return t

    def sh(self, cmd, cwd=None, check=True, agent_home=False):
        """An unrecorded shell command in the demo environment."""
        env = dict(self.env)
        if agent_home:
            env["HOME"] = self.real_home
        r = subprocess.run(
            ["/bin/bash", "-c", cmd],
            cwd=str(cwd or self.demo),
            env=env,
            capture_output=True,
            text=True,
        )
        if check and r.returncode != 0:
            raise RuntimeError(f"`{cmd}` failed ({r.returncode}):\n{r.stdout}\n{r.stderr}")
        return r

    def make_repo(self, base, files=None, message="Initial commit"):
        repo = self.demo / self.name(base)
        repo.mkdir(parents=True)
        self.repos.append(repo)
        for rel, body in (files or {"README.md": f"# {base}\n"}).items():
            p = repo / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(body)
        self.sh(
            "git init -q && git config user.name Demo && git config user.email demo"
            f" && git add -A && git commit -qm '{message}'",
            cwd=repo,
        )
        return repo

    def add_agent(self, repo, label, tool, review=None, tool_args=()):
        """Put a real agent on a demo roster.

        Only the agent's own process gets the real HOME, because its tool
        needs the user's login; clank itself stays in the demo home.
        """
        cmd = f"clank agent add {label} --tool {tool} --env HOME={self.real_home}"
        if review:
            cmd += f" --review {review}"
        if tool_args:
            cmd += " -- " + " ".join(shlex.quote(a) for a in tool_args)
        return self.sh(cmd, cwd=repo)

    def wait_for(self, pred, timeout, what, poll=2.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if pred():
                return
            time.sleep(poll)
        raise TimeoutError(f"timed out after {timeout}s waiting for {what}")

    def recorded_monitors(self):
        """pid -> birth for each monitor the run's repos (and their forks) record."""
        found = {}
        for repo in self.repos:
            for root in [repo, *sorted((repo / ".clank" / "worktrees").glob("*"))]:
                r = self.sh("clank status -j", cwd=root, check=False)
                try:
                    agents = json.loads(r.stdout).get("agents", [])
                except ValueError:
                    continue
                for a in agents:
                    mon = ((a.get("state") or {}).get("state") or {}).get("monitor") or {}
                    birth = mon.get("birth") or {}
                    if isinstance(mon.get("pid"), int) and "sec" in birth:
                        found[mon["pid"]] = birth["sec"] + birth.get("usec", 0) / 1e6
        return found

    def teardown(self, quiet_rounds=2, budget=120):
        for t in self.terms:
            t.close()
        live = self._list_sessions(self.env)
        ours = sorted(s for s in self.owned_sessions if s in live)
        for s in ours:
            subprocess.run(["zellij", "kill-session", s], env=self.env, capture_output=True)
            subprocess.run(["zellij", "delete-session", "--force", s], env=self.env, capture_output=True)
        # An agent's hook wait outlives its pane briefly and restarts a
        # missing monitor, so stop until the record stays empty.
        quiet = 0
        deadline = time.monotonic() + budget
        while quiet < quiet_rounds and time.monotonic() < deadline:
            monitors = self.recorded_monitors()
            if not verified_monitors(monitors, process_table()):
                quiet += 1
                time.sleep(5)
                continue
            quiet = 0
            for sig in (signal.SIGTERM, signal.SIGKILL):
                for pid in verified_monitors(monitors, process_table()):
                    try:
                        os.kill(pid, sig)
                    except ProcessLookupError:
                        pass
                end = time.monotonic() + 8
                while time.monotonic() < end and verified_monitors(monitors, process_table()):
                    time.sleep(0.5)
        final = self._list_sessions(self.env)
        lost = self.sessions_before - final
        left = sorted(s for s in self.owned_sessions if s in final)
        return ours, lost, left


def js_wrap(scene, cast_text):
    body = json.dumps(cast_text, ensure_ascii=False)
    body = body.replace("\u2028", "\\u2028").replace("\u2029", "\\u2029")
    return f'(window.CLANK_CASTS ||= {{}})["{scene}"] = {body};\n'


def js_unwrap(js_text):
    m = re.fullmatch(r'\(window\.CLANK_CASTS \|\|= \{\}\)\["([^"]+)"\] = (".*");\n?', js_text, re.S)
    if not m:
        raise ValueError("not a cast wrapper")
    return m.group(1), json.loads(m.group(2))


def check_files(paths):
    patterns = load_patterns()
    literals = sorted(set(runtime_literals()) | zellij_sessions(dict(os.environ)))
    bad = 0
    for p in paths:
        scene, cast = js_unwrap(Path(p).read_text(encoding="utf-8"))
        joined = "".join(json.loads(l)[2] for l in cast.splitlines()[1:] if l.strip())
        hits = scrub_hits(cast + "\n" + joined, patterns, literals)
        for label, pat, s in hits:
            print(f"{p}: {label} /{pat}/ {redact(s)}")
        bad += bool(hits)
    return bad


def load_scenario(name):
    path = SCENARIOS / f"{name}.py"
    if str(SCENARIOS) not in sys.path:
        sys.path.insert(0, str(SCENARIOS))
    spec = importlib.util.spec_from_file_location(f"scenario_{name.replace('-', '_')}", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("scenario", nargs="?")
    ap.add_argument("--demo-parent", default="/tmp", help="SHORT directory to make the fresh demo home in")
    ap.add_argument("--keep-demo", action="store_true")
    ap.add_argument("--check", nargs="+", metavar="CAST_JS")
    ap.add_argument("--refused-dir", help="keep a refused cast here for inspection")
    ap.add_argument("--out-dir", type=Path, default=CASTS, help="where casts are written")
    args = ap.parse_args()
    if args.check:
        sys.exit(1 if check_files(args.check) else 0)
    if not args.scenario:
        ap.error("name a scenario: " + ", ".join(p.stem for p in SCENARIOS.glob("[!_]*.py")))
    mod = load_scenario(args.scenario)
    # Short and neutral on purpose: narrow panes wrap long paths across
    # lines, where no rewrite can reach, so every fragment of the demo root
    # must be harmless to show. $TMPDIR names the user's hash.
    demo = Path(tempfile.mkdtemp(prefix="bd-", dir=args.demo_parent))
    ctx = Ctx(demo)
    print(f"demo home {demo}", file=sys.stderr)
    print(f"zellij sessions before: {sorted(ctx.sessions_before)}", file=sys.stderr)
    error = None
    try:
        mod.run(ctx)
    except BaseException as e:  # noqa: BLE001 - teardown must run on Ctrl-C too
        error = e
    finally:
        ours, lost, left = ctx.teardown()
        print(f"stopped demo sessions: {ours}", file=sys.stderr)
        print(f"zellij sessions after: {sorted(zellij_sessions(ctx.env))}", file=sys.stderr)
        if lost:
            print(f"WARNING pre-existing sessions now missing: {sorted(lost)}", file=sys.stderr)
        if left:
            print(f"WARNING owned sessions still running: {left}", file=sys.stderr)
    if error:
        raise error

    patterns = load_patterns()
    literals = sorted(set(runtime_literals()) | ctx.sessions_before)
    target = getattr(mod, "TARGET_SECONDS", 75)
    min_speed = getattr(mod, "MIN_SPEED", 0.01)
    failed = False
    for t in ctx.terms:
        if not t.scene:
            continue
        cast, length = t.cast(target, min_speed)
        joined = "".join(json.loads(l)[2] for l in cast.splitlines()[1:])
        hits = scrub_hits(cast + "\n" + joined, patterns, literals)
        if hits:
            failed = True
            for label, pat, s in hits[:20]:
                print(f"REFUSED {t.scene}: {label} /{pat}/ {redact(s)}", file=sys.stderr)
            if args.refused_dir:
                Path(args.refused_dir).mkdir(parents=True, exist_ok=True)
                (Path(args.refused_dir) / f"{t.scene}.cast").write_text(cast, encoding="utf-8")
            continue
        args.out_dir.mkdir(parents=True, exist_ok=True)
        out = args.out_dir / f"{t.scene}.js"
        out.write_text(js_wrap(t.scene, cast), encoding="utf-8")
        print(f"wrote {out}: {length:.0f}s, {out.stat().st_size // 1024} KiB", file=sys.stderr)
    if not args.keep_demo:
        shutil.rmtree(demo, ignore_errors=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
