"""`clank tui` while the team works, zoomed to fill the screen.

Only one `clank tui` may drive a repo's panes, so this is the TUI pane of
the team `clank open` starts, made fullscreen, over a second tiny plan.
"""

import time

from _team import bound, idle, no_blocks, setup_team

SCENES = ["tui-watch"]
TARGET_SECONDS = 75
REPO = "book-demo-shop"
PLAN = "usage-line"

DRAFT = """# A usage line

## Why

The README never says how to run `greet.sh`.

## What ships

- `README.md` gains one line under the title: ``Usage: `./greet.sh [name]` ``.

Nothing else changes.

## Acceptance

`README.md` contains that usage line.
"""


def run(ctx):
    repo = setup_team(ctx, REPO)
    ctx.own_session(repo)
    drafts = repo / ".clank" / "drafts"
    drafts.mkdir(parents=True, exist_ok=True)
    (drafts / f"{PLAN}.md").write_text(DRAFT)

    t = ctx.term(scene="tui-watch", cwd=repo)
    t.expect(r"\$ $", 10)
    t.pause(0.8)
    t.type(f"clank queue add {PLAN}", settle=2)
    t.type("clank open", settle=3)
    t.expect(r"add agent", 60)
    t.pause(1.5)
    # The TUI sits below the reviewer stack; clank's layout binds
    # Alt+h/j/k/l to focus moves.
    t.keys("\x1bl", after=0.6)
    t.keys("\x1bj", after=0.6)
    t.keys("\x10", after=0.4)
    t.keys("f", after=2)

    ctx.wait_for(lambda: bound(ctx, repo, ["alice", "bob"]), 240, "alice and bob to bind")
    finished = repo / ".clank" / "finished" / f"{PLAN}.md"
    ctx.wait_for(lambda: no_blocks(ctx, repo) or finished.exists(), 40 * 60, "the plan to finish", poll=5)
    ctx.wait_for(lambda: idle(ctx, repo), 300, "the master to end its turn", poll=3)
    time.sleep(3)
    t.pause(6)
