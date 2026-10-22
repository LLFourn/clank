"""Helpers the team scenarios share: a demo team and its observed state."""

import json
import time

# Reduced motion: a spinner redrawn ten times a second is most of a cast's bytes.
CLAUDE_ARGS = ["--model", "sonnet", "--permission-mode", "auto", "--settings", '{"prefersReducedMotion":true}']
CODEX_ARGS = ["-c", 'model_reasoning_effort="low"']

GREET = "#!/bin/sh\necho hello\n"


def setup_team(ctx, name, master="alice", reviewer="bob"):
    repo = ctx.make_repo(
        name,
        {"README.md": "# shop\n\nA tiny demo app.\n", "greet.sh": GREET},
    )
    ctx.sh("chmod +x greet.sh && git add greet.sh && git commit -qm 'Make greet.sh executable'", cwd=repo)
    ctx.sh("clank init", cwd=repo)
    ctx.add_agent(repo, master, "claude", tool_args=CLAUDE_ARGS)
    if reviewer:
        ctx.add_agent(repo, reviewer, "codex", tool_args=CODEX_ARGS)
    ctx.sh("git add .clank && git commit -qm 'Add clank'", cwd=repo)
    return repo


def status(ctx, repo):
    r = ctx.sh("clank status -j", cwd=repo, check=False)
    try:
        return json.loads(r.stdout)
    except ValueError:
        return {}


def agents(ctx, repo):
    return {a["label"]: a for a in status(ctx, repo).get("agents", [])}


def no_blocks(ctx, repo):
    """Raise if an agent asked the human something: the run cannot finish unattended."""
    blocks = [b for b in status(ctx, repo).get("blocks") or [] if b.get("pending")]
    if blocks:
        raise RuntimeError(f"{repo.name}: agent raised a block: {blocks[0].get('question')}")


def parked(agent):
    """Parked in a wait that can take its work: the turn is over."""
    state = (agent.get("state") or {}).get("state") or {}
    return "delivering" in (state.get("suppressors") or [])


def bound(ctx, repo, labels):
    a = agents(ctx, repo)
    return all(a.get(lab, {}).get("session") for lab in labels)


def idle(ctx, repo):
    a = agents(ctx, repo)
    return bool(a) and all(parked(x) for x in a.values())


def settle(ctx, repo, timeout=300):
    """Idle twice, a beat apart: a fresh session is briefly idle before its first turn."""
    for _ in range(2):
        ctx.wait_for(lambda: idle(ctx, repo), timeout, f"{repo.name} to settle", poll=3)
        time.sleep(6)
