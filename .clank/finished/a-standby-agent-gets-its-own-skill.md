# a-standby-agent-gets-its-own-skill
# A standby agent gets its own skill

## Problem

Standby agents are installed one skill shared with reviewers. `clank-reviewer`'s description says "REVIEWER and STANDBY roles". Its role guard grants one exception: "A standby agent writes no verdicts". The rest of `setup_assets/skill_reviewer.md` is the reviewer's job:
- "You are a REVIEWER … That is all";
- the invariants;
- the review loop;
- the verdicts.

So a standby agent such as safepay story-short's `painter` loads a reviewer's instructions and has to remember a one-line exception to all of them. The master skill's description also routes standby agents to `clank-reviewer`.

A standby agent is a third role. It reviews nothing and is woken for everything else, as `clank agent add --review standby` says. It needs its own instructions, not a reviewer's with a carve-out.

## Design

- **A new skill, `clank-standby`, kept minimal.** LLFourn: "they need a basic skill to understand events and messages, that's about it." Its body is `skill_shared_core.md`, which already covers how work arrives, never running `clank wait`, and reading and acking messages. On top of that, a short `setup_assets/skill_standby.md` says three things:
  - every event it is woken with, not only messages, is read with `clank events read` and acked with `clank events ack` once handled;
  - it writes no verdicts and runs no plan or roster commands;
  - otherwise it does what the event or message asks.

  Nothing else goes in: no loop, no verdicts, no plan workflow. The shared parts are never copied into the standby body.
- **`clank-reviewer` becomes reviewer-only.**
  - The description, the role guard and the "Always (both roles)" / "Messages (both roles)" headings name reviewers only.
  - The standby exception is deleted.
  - The guard names `clank-standby` and `clank-master` as the others.
- **Every description and role guard names all three.** That covers the master skill and both of the others, so an agent never loads another role's skill.
- **`setup` installs and upgrades `clank-standby`, and `doctor` verifies it,** for every tool it handles the others for: Claude, Codex, Grok and OpenCode. clank has no uninstall command. The only removal path is retired-asset cleanup, and this skill isn't retired. Anything that enumerates the role skills learns the new one, including `Role` and the role→skill table at `setup.rs:327`.
- **The role→skill mapping is one table.** If anything elsewhere (status output, wake items, doctor) names which skill a role should use, it reads that table.

## Acceptance

- Unit tests in `cli::setup`:
  - each skill's frontmatter names its own role and the other two skills;
  - the standby body never contains the reviewer loop or verdict text;
  - the reviewer body no longer mentions standby;
  - setup writes `clank-standby` for each tool root through the shared asset inventory, which install, upgrade and doctor all read.
- Sweep the whole repo for "REVIEWER and STANDBY", "standby" next to "clank-reviewer", and "both roles". That covers README, the book, docs, the setup assets and tests. Report which hits were kept.
- The README's skill lists and the book's (`for-agents.html`, `getting-started.html`) gain `clank-standby`.

## Out of scope

Changing what a standby agent is woken for.
