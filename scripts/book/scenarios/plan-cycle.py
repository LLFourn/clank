"""A real plan cycle in the zellij client that `clank open` starts.

The claude master and codex commit reviewer take one tiny plan from
promotion through review to `clank finish`.
"""

import time

from _team import bound, idle, no_blocks, setup_team

SCENES = ["plan-cycle"]
TARGET_SECONDS = 80
REPO = "book-demo-shop"
PLAN = "greet-by-name"

DRAFT = """# Greet by name

## Why

`greet.sh` only ever says hello. Callers want it to greet someone.

## What ships

- `./greet.sh Ada` prints `hello, Ada`.
- `./greet.sh` with no argument prints `hello, world`.

That is the whole change: one small edit to `greet.sh`, nothing else.

## Acceptance

Both commands above print exactly those lines.
"""


def run(ctx):
    repo = setup_team(ctx, REPO)
    ctx.own_session(repo)
    drafts = repo / ".clank" / "drafts"
    drafts.mkdir(parents=True, exist_ok=True)
    (drafts / f"{PLAN}.md").write_text(DRAFT)

    t = ctx.term(scene="plan-cycle", cwd=repo)
    t.expect(r"\$ $", 10)
    t.pause(0.8)
    t.type(f"cat .clank/drafts/{PLAN}.md", settle=2.5)
    t.type(f"clank queue add {PLAN}", settle=2)
    t.type("clank open", settle=3)

    ctx.wait_for(lambda: bound(ctx, repo, ["alice", "bob"]), 240, "alice and bob to bind")
    finished = repo / ".clank" / "finished" / f"{PLAN}.md"
    ctx.wait_for(lambda: no_blocks(ctx, repo) or finished.exists(), 40 * 60, "the plan to finish", poll=5)
    ctx.wait_for(lambda: idle(ctx, repo), 300, "the master to end its turn", poll=3)
    time.sleep(3)
    t.pause(6)
