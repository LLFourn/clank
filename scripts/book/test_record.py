"""Recorder ownership: teardown can only ever reach what its run created.

    python3 -m unittest scripts/book/test_record.py

Nothing here signals a process; selection is checked on fixture tables.
"""

import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import record  # noqa: E402
from record import Proc  # noqa: E402

NO_SESSIONS = lambda env: set()  # noqa: E731


class Run:
    """A Ctx over a fresh demo home, without touching zellij."""

    def __init__(self, parent):
        self.demo = Path(tempfile.mkdtemp(prefix="bd-", dir=parent))
        self.ctx = record.Ctx(self.demo, list_sessions=NO_SESSIONS)


class OwnershipTests(unittest.TestCase):
    def setUp(self):
        self.parent = tempfile.TemporaryDirectory()
        self.addCleanup(self.parent.cleanup)

    def own_repo(self, run, base="book-demo-shop"):
        repo = run.demo / run.ctx.name(base)
        run.ctx.repos.append(repo)
        return repo, run.ctx.own_session(repo, planned="clank-" + repo.name)

    def test_a_process_that_mentions_or_sits_in_the_demo_dir_is_never_selected(self):
        run = Run(self.parent.name)
        repo, _ = self.own_repo(run)
        ours_tty = "ttys901"
        editor, shell, leader, child = 4101, 4102, 4103, 4104
        table = {
            # An editor opened on a demo file, and a shell cd'd into the demo
            # repo: both name the demo dir, neither is on the run's pty.
            editor: Proc("ttys123", 1000.0),
            shell: Proc("ttys124", 1000.0),
            leader: Proc(ours_tty, 1000.0),
            child: Proc(ours_tty, 1001.0),
        }
        self.assertEqual(sorted(record.tty_members(table, ours_tty)), [leader, child])
        self.assertNotIn(editor, record.tty_members(table, ours_tty))
        self.assertNotIn(shell, record.tty_members(table, ours_tty))
        self.assertEqual(record.tty_members(table, "??"), [], "a tty-less process belongs to no pty")
        self.assertEqual(record.verified_monitors({}, table), [], "no record, no monitor")

    def test_a_process_seen_on_the_pty_stays_ours_only_while_its_pid_keeps_its_start(self):
        hung_up, reused, reaped = 6101, 6102, 6103
        seen = {hung_up: 3000.0, reused: 3000.0, reaped: 3000.0}
        table = {
            # A hangup took the terminal away; it is the same process.
            hung_up: Proc("??", 3000.2),
            reused: Proc("??", 3900.0),
            reaped: Proc("??", 3000.0, True),
        }
        self.assertEqual(record.still_running(seen, table), [hung_up])

    def test_a_closed_term_never_reaches_a_reused_tty_pid_or_fd(self):
        ours, foreign = 11111, 99999
        tables = iter([
            {ours: Proc("ttys901", 1000.0)},
            {},
            {},
        ])
        later = {foreign: Proc("ttys901", 5000.0)}
        signalled, closed, reaped = [], [], []
        term = record.Term.__new__(record.Term)
        term.tty, term.pid, term.fd, term.alive, term.closed = "ttys901", ours, 42, True, False
        term.reader = unittest.mock.Mock()
        patches = [
            unittest.mock.patch.object(record, "process_table", lambda: next(tables, later)),
            unittest.mock.patch.object(record.os, "kill", lambda pid, sig: signalled.append(pid)),
            unittest.mock.patch.object(record.os, "waitpid", lambda pid, _: reaped.append(pid)),
            unittest.mock.patch.object(record.os, "close", closed.append),
            unittest.mock.patch.object(record.time, "sleep", lambda _: None),
        ]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)
        term.close()
        term.close()
        self.assertEqual(signalled, [ours])
        self.assertNotIn(foreign, signalled)
        self.assertEqual(closed, [42])
        self.assertEqual(reaped, [ours])

    def test_a_reused_monitor_pid_is_not_ours(self):
        monitor, reused = 5101, 5102
        recorded = {monitor: 2000.0, reused: 2000.0}
        table = {monitor: Proc("??", 2000.4), reused: Proc("??", 2600.0)}
        self.assertEqual(record.verified_monitors(recorded, table), [monitor])

    def test_two_runs_never_share_a_name_or_a_cleanup_set(self):
        a, b = Run(self.parent.name), Run(self.parent.name)
        self.assertNotEqual(a.ctx.run_id, b.ctx.run_id)
        repo_a, session_a = self.own_repo(a)
        repo_b, session_b = self.own_repo(b)
        self.assertNotEqual(repo_a.name, repo_b.name)
        self.assertNotEqual(session_a, session_b)
        self.assertTrue(a.ctx.owned_sessions.isdisjoint(b.ctx.owned_sessions))
        self.assertTrue(set(a.ctx.repos).isdisjoint(b.ctx.repos))

    def test_only_a_repo_the_run_made_can_own_a_session(self):
        a, b = Run(self.parent.name), Run(self.parent.name)
        repo_b, _ = self.own_repo(b)
        with self.assertRaises(AssertionError):
            a.ctx.own_session(repo_b, planned="clank-" + repo_b.name)
        with self.assertRaises(AssertionError):
            a.ctx.own_session(a.demo / "book-demo-shop", planned="clank-book-demo-shop")
        stray = a.demo / a.ctx.name("book-demo-ops")
        with self.assertRaises(AssertionError, msg="named for this run but never made by it"):
            a.ctx.own_session(stray, planned="clank-" + stray.name)

    def test_a_session_clank_would_name_differently_is_refused(self):
        run = Run(self.parent.name)
        repo = run.demo / run.ctx.name("book-demo-shop")
        run.ctx.repos.append(repo)
        with self.assertRaises(AssertionError):
            run.ctx.own_session(repo, planned="clank-book-demo-sho-4c2f")

    def test_every_demo_name_fits_a_session_name_unhashed(self):
        run = Run(self.parent.name)
        for base in ("book-demo-shop", "book-demo-ops"):
            self.assertLessEqual(len("clank-" + run.ctx.name(base)), record.SESSION_NAME_MAX)

    def test_a_session_that_already_exists_is_refused(self):
        run = Run(self.parent.name)
        repo = run.demo / run.ctx.name("book-demo-shop")
        run.ctx.repos.append(repo)
        run.ctx._list_sessions = lambda env: {"clank-" + repo.name}
        with self.assertRaises(RuntimeError):
            run.ctx.own_session(repo, planned="clank-" + repo.name)

    def test_casts_show_the_base_name(self):
        run = Run(self.parent.name)
        unique = run.ctx.name("book-demo-shop")
        path = f"{run.demo}/{unique}/greet.sh"
        out = record.rewrite_events([(0.0, f"tab {unique} at {path}")], run.ctx.rewrite)
        self.assertEqual(out[0][1], "tab book-demo-shop at ~/book-demo-shop/greet.sh")


if __name__ == "__main__":
    unittest.main()
