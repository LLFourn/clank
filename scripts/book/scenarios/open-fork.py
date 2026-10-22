"""`clank open` bringing up a team, then `clank fork create` and its tab.

One team serves both scenes: the second attaches a fresh client to the
session the first opened, so it starts from a full redraw.
"""

from _team import bound, settle, setup_team

SCENES = ["open-team", "fork-create"]
TARGET_SECONDS = 70
REPO = "book-demo-shop"


def run(ctx):
    repo = setup_team(ctx, REPO)
    ctx.own_session(repo)
    t = ctx.term(scene="open-team", cwd=repo)
    t.expect(r"\$ $", 10)
    t.pause(0.8)
    t.type("clank agent list")
    t.pause(1.5)
    t.type("clank open", settle=3)
    ctx.wait_for(lambda: bound(ctx, repo, ["alice", "bob"]), 240, "alice and bob to bind")
    settle(ctx, repo)
    t.pause(4)
    t.close()

    f = ctx.term(scene="fork-create", cwd=repo)
    f.expect(r"\$ $", 10)
    f.pause(0.8)
    f.type("clank open", settle=2.5)
    f.type(f"zellij attach clank-{repo.name}", settle=4)
    f.keys("\x1bn", after=2)
    f.type("clank fork create greeting", settle=6)
    ctx.wait_for(lambda: (repo / ".clank/worktrees/greeting").exists(), 120, "the fork worktree")
    fork = repo / ".clank/worktrees/greeting"
    ctx.wait_for(lambda: bound(ctx, fork, ["alice", "bob"]), 240, "the forked agents to bind")
    settle(ctx, fork)
    f.pause(4)
