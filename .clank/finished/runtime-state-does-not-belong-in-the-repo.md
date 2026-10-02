# runtime-state-does-not-belong-in-the-repo
# Runtime state does not belong in the repo

## Why

A fresh team forked from `~/src/frostsnap` starts life like this:

```
Untracked files:
        .clank/monitor.json
        .clank/monitor.lock
```

Nothing was wrong with the team. The monitor wrote its descriptor and its lease into
`.clank/`, and whether git saw them depended entirely on that repo's
`.clank/.gitignore` — a file that propagates by being TRACKED. `frostsnap` never had one,
so every worktree and every fork of it starts dirty, and nothing in the new team's creation
fixes that. Measured across this machine: six repos have the allow-list and ignore these
correctly; the two that do not are the two where the files show up.

The deeper point is that the `.gitignore` should not be load-bearing here at all. The
monitor's SOCKET already lives outside the repo — `socket_dirs` puts it in a short flat
directory, deliberately, "never under the repo". The descriptor and the lease were the same
kind of thing and they went inside. That was the inconsistency; the dirty status was a
symptom of it.

`.clank/` held three different kinds of thing. `clank_dir_boundary`'s `DECLARED` is the
enforced version of all three — every entry in it is in one of these bullets, and every
path in these bullets that production constructs is in it:

- **Tracked and meaningful**: `plans/`, `finished/`, `.gitignore`.
- **Local but durable**: `config.json`, `agents/`, `queue/`, `drafts/`, `blocks/`,
  `stash/`, `shelved/`, `pr-reviews/`, `fork.json`, `attachments/`, `worktrees/`,
  `clones/`. These belong to the repo even though they are not committed, so they do need
  an ignore rule.
- **Ephemeral process state**: `monitor.json`, `monitor.lock`, `status-tui.lock`, and
  arguably `cache/`, `html/`, `zellij/`. These belong to a RUNNING PROCESS and mean nothing
  once it is gone.

Only the third kind was this plan's subject — and of those three files, all three are now
outside the repo. `cache/`, `html/` and `zellij/` stayed, deliberately (see Out of scope).

## What to do

Move ephemeral process state out of the repo, beside the socket, keyed by the canonical
repo path. Then no runtime file can appear in anyone's `git status` whatever their
`.gitignore` says — including repos that never run `clank init` again, which is the only
version of this fix that does not depend on every repo being up to date.

Then keep it true with a gate, in the manner of `git_boundary` and `signal_boundary`:
production code may not create a path directly under `.clank/` unless it is in a declared
set. A new runtime file becomes a build failure rather than somebody's dirty status six
months later.

## A CUTOVER, not a bridge — and why

The lease is the authority for "one monitor owns this repo", and that only works while every
contender derives the SAME path. Moving it therefore has a compatibility question, and the
first draft of this plan answered it with a dual-lock bridge "for one release". That answer
was wrong twice over (codex on c0ebe23):

- **It contradicted the plan's own goal.** `acquire_lease` opens the legacy path with
  `create(true)`, so taking it in a fresh repo CREATES `.clank/monitor.lock` — the exact
  untracked file this plan exists to remove. A bridge would have had to keep an ignore rule
  alive and admit it, rather than claim none is involved. `status_tui/lease.rs` does the
  same.
- **"One release" is not a synchronisation event.** A legacy-only binary can start at any
  time, including after the bridging release is gone or after a rollback. Installing two
  releases in order prevents nothing; it just makes the window feel smaller.

So this plan takes the other option and says so plainly: **a cutover**. After it, a binary
predating it is NOT SUPPORTED running concurrently against the same repo. Two monitors
could own one repo in that configuration, and nothing here prevents it — the plan does not
get to claim otherwise.

That is defensible because it is exactly how the previous plan was already deployed. Its
install retired every running monitor by hand, deliberately, as its documented one-time
bootstrap. This cutover is paid the same way and at the same moment: install, then retire
anything still running. The alternative is a bridge whose temporary exception would
outlive the plan and whose removal needs a support-floor policy nobody wants to write.

What that costs, stated rather than discovered: a rollback to the previous binary gives it
a free legacy lock and a free external lock, so a monitor from each can run until one is
retired. Say it in the plan's own notes and in the release's.

### The install must also DELETE what the old location holds

Moving the writes does not clean up the writes already made. Every repo that has run a
pre-cutover binary has a `.clank/monitor.json` and a `.clank/monitor.lock` on disk right
now, and after this lands nothing will ever rewrite them — or remove them. In the two
repos without an allow-list `.gitignore` they would go on showing up in `git status`
exactly as before, which is the complaint this plan exists to answer. A fix that leaves
them there has not fixed anything a user can see.

So the install is three steps, not two, and the third is ORDERED AFTER the second for a
reason worth stating, because the first draft of this paragraph got it wrong:

1. Install the binary.
2. Retire every pre-cutover holder — monitors AND status TUIs. A legacy panel is in this
   list for the same reason a legacy monitor is: it leases `.clank/status-tui.lock`, which
   a post-cutover TUI never looks at, so the two would drive the same repo's panes at once
   and the old one's untracked file would stay visible.
3. Then delete the legacy files: `.clank/monitor.json`, `.clank/monitor.lock` and
   `.clank/status-tui.lock`.

Unlinking a coordination path while a holder is live is NOT safe, and the earlier claim
that it was — "the held fd IS the state, so unlinking does not release it" — is only the
half that favours the conclusion. The other half is that the name is now free: the next
legacy-only process to start creates a FRESH inode there and locks it, so two of them hold
"the lease" and neither can see the other. That is the identical failure the whole scheme
exists to prevent, and it does not care that one of the two holders was ours to retire.

So the deletion is valid only once the old holders are stopped AND nothing can start
another one — which, for a hand-paid cutover on one machine, means the pre-cutover binary
is no longer the installed one. A rollback re-opens the window, and that is already stated
above as the rollback's cost.

`PROTO` does NOT bump for this, and the reason is worth stating because a reader will
reach for it: the wire did not change, and the two versions cannot meet on a socket anyway.
A post-cutover client discovers monitors only through the descriptor at the new path, where
no pre-cutover monitor ever writes, so it finds nothing and starts its own. Bumping would
advertise a wire change that did not happen, and the decline it would produce is for a
conversation that cannot occur.

Done by hand, deliberately, like the retirement it accompanies. The affected set is
exactly "repos that ran a pre-cutover binary": finite, enumerable on this machine, and
never added to again. Cleanup code for it would be a permanent reconciliation pass for a
one-time event — and it would have to live in the startup path of every future release to
catch a repo that had been sitting idle.


## There is now a precedent — reuse it

`a-proto-bump-cannot-retire-a-running-monitor` landed a repair gate that is exactly this
pattern, and it should be the model rather than a second scheme invented beside it:

- A FIXED per-uid directory, `/tmp/clank-repair-<uid>/`, derived from nothing else. The
  reasoning is in its doc and is worth reading before choosing a path here: the socket may
  try CANDIDATES because the published descriptor then tells everyone which was chosen,
  while a lock has no such discovery, so every caller must derive the same path or it is
  not a lock. Two clients differing only in `XDG_RUNTIME_DIR` each took a different one.
- A stable digest of the canonical path's RAW BYTES (`repo_tag`, blake3). `DefaultHasher`
  was rejected because Rust leaves its algorithm unspecified between releases — which bites
  precisely when two installed binaries contend during an upgrade — and `to_string_lossy`
  is not the path.
- The directory vetted through `ensure_dir`, because a lock file replaced under a holder
  lets the next caller lock a different inode at the same name.

The descriptor and the monitor lease have the same requirements. Whether they share that
directory or get their own is a decision for this plan; inventing a different naming scheme
is not.

**Decided: they share it, and it is renamed.** `/tmp/clank-repair-<uid>/` becomes
`/tmp/clank-run-<uid>/`, holding `<repo_tag>.repair`, `.lock`, `.json` and `.status`. One
directory, one vet, one per-uid mode-0700 namespace — not four. The rename is part of the
same cutover for the same reason the lease move is: a binary predating it derives a
different repair path, and that is one more thing the install must retire rather than a
compatibility surface to carry.

## Acceptance

- A newly forked repo with no `.clank/.gitignore` has a clean `git status` while a monitor
  runs AND while a TUI holds its lease. Driven against that shape specifically, since it is
  the one that produced this.
- DISCOVERY trusts the namespace before it reads one, and creates nothing doing it. Moving
  the descriptor into a predictable path under world-writable `/tmp` means another uid can
  get there first and leave one naming the right repo and proto with `socket` pointing at
  theirs; matching fields are not evidence of who wrote them. Refusing to PUBLISH into a
  planted namespace does nothing about consuming what is already in it. Driven through the
  client with a connect SPY that must stay untouched — a vet returning the right answer
  proves nothing about whether discovery asks it — across a writable directory, a symlink,
  a trusted namespace and a missing one.
- Every MUTATION boundary in the new namespace goes through the same `ensure_dir` vet, and
  publishing is one of them. The descriptor is the DISCOVERY record, so a directory another
  uid planted in world-writable `/tmp` would otherwise take a descriptor naming their
  socket and every client would connect to it — a hazard the old location did not have,
  because the parent was inside the user's own repo.
- No production code creates a path directly under `.clank/` outside a declared set, and a
  gate fails the build if it does. It catches the ORDINARY ways of writing one, not the one
  spelling it was first written against: one literal (`join(".clank/monitor.json")`), the
  two-call chain in whatever formatting rustfmt left it, the `clank_dir()` accessor, and a
  local bound to the root. A gate a reformat can switch off is not a gate, and the negative
  corpus is driven through the same decision function the repo scan uses. The `~/.clank`
  exemption is a list of declared SITES, never a receiver's name.
- A repo with no ignore rule is clean while BOTH real holders are live — the monitor's
  lease and publication and the panel's `StatusLease::acquire`, not a stand-in writing to
  the path the test expects them to use.
- **One monitor per repo still, among SUPPORTED binaries.** Two of this version cannot both
  own a repo. The claim stops there: a binary predating the cutover is out of scope and the
  acceptance says so rather than implying it is prevented.
- The supported upgrade is driven: a current monitor running, the new binary installed, the
  old one retired, a client then starting one that takes the new lease. And the rollback is
  driven for what it HONESTLY does — each binary taking its own lock — so the limitation is
  recorded by a test rather than by a sentence somebody may not read.
- The runtime namespace is the one the repair gate already established, not a second
  scheme, and a repo that cannot get one degrades the way the socket already does rather
  than failing.
- Nothing durable moves. `agents/`, `queue/`, `config.json` and the rest stay where they
  are.


## Out of scope

- Migrating existing repos' `.clank/.gitignore` from the legacy deny-list to the allow-list.
  Worth doing, smaller, and independent — this plan's whole point is to stop the answer
  depending on it.
- `cache/`, `html/` and `zellij/`. Arguably the same category, but they are directories with
  their own lifecycles and lumping them in triples the surface. Decide deliberately and
  split if needed.
- Changing what the monitor DOES. This moves where it writes, nothing else.

## Watch out

- The socket directory already solves "a short flat path, never under the repo, chosen from
  a candidate list with ownership and mode vetting". Reuse that reasoning rather than
  inventing a second scheme — and note it vets the directory it binds in, which a
  descriptor written there needs too.
- A descriptor outside the repo must still be found from the repo, so the key is the
  CANONICAL repo path. A repo that moves is then a repo whose descriptor cannot be found,
  which is the same answer as no descriptor, which is safe — but say so rather than discover
  it.
- `status-tui.lock` has a second user with its own assumptions; check it before moving it.
  It has the same `create(true)` behaviour as the monitor lease, so it is in the cutover for
  the same reason and not a separate decision.
- Fixed acquisition order, nonblocking acquisition, and release of a partial acquisition on
  every error path, if more than one lock is ever held at once. And no lock file is unlinked
  or replaced while a contender may still open it — the held fd IS the state, as the repair
  gate's doc already says.
