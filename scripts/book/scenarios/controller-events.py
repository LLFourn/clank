"""A controller repo woken by a timer and by a GitHub-shaped event.

`book-demo-ops` has one claude agent whose wait_events hold a one-shot
timer and a command source. The source stands in for GitHub without an
account: a local producer that prints one PR-comment-shaped JSONL line
when the scenario touches its trigger file. The controller forwards the
request to the `book-demo-shop` team with `clank send --repo`, and the
shop's master answers.
"""

import datetime
import json
import time

from _team import CLAUDE_ARGS, bound, idle, settle

SCENES = ["controller-events"]
TARGET_SECONDS = 95
TIMER_LEAD = 150

FEED = r'''#!/usr/bin/env python3
import json, os, sys, time
trigger = os.path.join(os.path.dirname(os.path.abspath(__file__)), "github-feed.trigger")
while not os.path.exists(trigger):
    time.sleep(1)
payload = {
    "action": "created",
    "repository": {"full_name": "acme/shop"},
    "issue": {"number": 12, "title": "greet.sh: greet people by name", "pull_request": {}},
    "comment": {
        "user": {"login": "octo-reviewer"},
        "body": "Can greet.sh greet people by name? `./greet.sh Ada` should print `hello, Ada`.",
    },
}
print(json.dumps({"id": "acme/shop#12/comment/4417", "kind": "pr_comment",
                  "content": json.dumps(payload, indent=2)}), flush=True)
while True:
    time.sleep(3600)
'''

OPS_GUIDE = """# ops: the controller

This repo holds no product code. Its agent, `ops`, watches the shop's
events and hands the shop team work.

- The shop team's repo is `{shop}`.
- A `pr_comment` event is a GitHub comment on the shop's repo. When it asks
  for a change, send it to the shop's master in one message:
  `clank send --repo {shop} --master --reply-to ops -m "<the request>"`,
  then ack the event.
- A timer event: do what it says, then ack it.
- A message from the shop: read it, ack it, and say one line about it.

Keep every answer to a line or two. Never edit files in this repo.
"""


def rfc3339(ts):
    return datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S+00:00")


def events(ctx, repo, author):
    r = ctx.sh(f"clank events list --author {author} --all -j", cwd=repo, check=False)
    try:
        data = json.loads(r.stdout)
    except ValueError:
        return []
    return data if isinstance(data, list) else data.get("events", [])


def matches(e, needle):
    if needle.startswith("from:"):
        return (e.get("item") or {}).get("from") == needle[5:]
    return needle in json.dumps(e)


def handled(ctx, repo, author, needle):
    return any(matches(e, needle) and e.get("acked") for e in events(ctx, repo, author))


def arrived(ctx, repo, author, needle):
    return any(matches(e, needle) for e in events(ctx, repo, author))


def run(ctx):
    shop = ctx.make_repo("book-demo-shop", {"README.md": "# shop\n\nA tiny demo app.\n", "greet.sh": "#!/bin/sh\necho hello\n"})
    ctx.sh("chmod +x greet.sh && git commit -qam 'Make greet.sh executable' && clank init", cwd=shop)
    ctx.add_agent(shop, "alice", "claude", tool_args=CLAUDE_ARGS)
    ctx.sh("git add .clank && git commit -qm 'Add clank'", cwd=shop)

    ops = ctx.make_repo("book-demo-ops", {"CLAUDE.md": OPS_GUIDE.format(shop=shop)})
    ctx.own_session(ops)
    ctx.sh("clank init", cwd=ops)
    ctx.add_agent(ops, "ops", "claude", tool_args=CLAUDE_ARGS)
    ctx.sh("git add .clank && git commit -qm 'Add clank'", cwd=ops)

    bindir = ctx.demo / "bin"
    bindir.mkdir()
    feed = bindir / "github-feed"
    feed.write_text(FEED)
    feed.chmod(0o755)
    trigger = bindir / "github-feed.trigger"

    cfg_path = ops / ".clank" / "agents" / "ops" / "config.json"
    cfg = json.loads(cfg_path.read_text()) if cfg_path.exists() else {}
    cfg["wait_events"] = [
        {
            "kind": "timer",
            "name": "shop-check",
            "due_at": rfc3339(int(time.time()) + TIMER_LEAD),
            "content": f"Check on the shop team: run `clank status --repo {shop}` and report it in one line.",
        },
        {"kind": "command", "name": "github", "command": [str(feed)]},
    ]
    cfg_path.parent.mkdir(parents=True, exist_ok=True)
    cfg_path.write_text(json.dumps(cfg, indent=2) + "\n")

    t = ctx.term(scene="controller-events", cwd=ops)
    t.expect(r"\$ $", 10)
    t.pause(0.8)
    t.type("cat .clank/agents/ops/config.json", settle=4)
    t.type("clank open", settle=3)
    ctx.wait_for(lambda: bound(ctx, ops, ["ops"]), 240, "ops to bind")
    settle(ctx, ops)

    t.keys("\x1bn", after=2)
    t.type(f"clank open --repo ~/{shop.name}", settle=4)
    ctx.wait_for(lambda: bound(ctx, shop, ["alice"]), 240, "the shop master to bind")
    settle(ctx, shop)
    t.keys("\x14", after=0.4)
    t.keys("h", after=0.4)
    t.keys("\r", after=1.5)
    t.type("exit", settle=2)

    ctx.wait_for(lambda: handled(ctx, ops, "ops", "shop-check"), 600, "ops to handle the timer", poll=4)
    ctx.wait_for(lambda: idle(ctx, ops), 300, "ops to end its turn", poll=3)
    t.pause(3)

    trigger.touch()
    ctx.wait_for(lambda: arrived(ctx, shop, "alice", "from:ops"), 600, "the message to reach the shop", poll=3)
    ctx.wait_for(lambda: idle(ctx, ops), 300, "ops to end its turn", poll=3)
    t.pause(3)

    t.keys("\x14", after=0.4)
    t.keys("l", after=0.4)
    t.keys("\r", after=1.5)
    ctx.wait_for(lambda: arrived(ctx, ops, "ops", "from:alice"), 900, "the shop to reply", poll=4)
    time.sleep(10)
    t.pause(3)

    t.keys("\x14", after=0.4)
    t.keys("h", after=0.4)
    t.keys("\r", after=1.5)
    ctx.wait_for(lambda: handled(ctx, ops, "ops", "from:alice"), 600, "ops to take the reply", poll=4)
    ctx.wait_for(lambda: idle(ctx, ops), 300, "ops to end its turn", poll=3)
    t.pause(5)
