# clank-stash-always-stashes
# clank stash always stashes the plan it is asked to

## Why

Lloyd, 2026-10-17: "clank stash MUST always DWIM. It must never say
'no' — even when there are untracked files it should merely warn about
them. Clank stash must always succeed in stashing the targeted plan
(with the one exception is if it creates commit conflicts)." And: "clank
stash should just roll unstaged changes into a commit with an
indication this was an automatic commit."

What happened: an agent was asked to pause `rust-k1-generator` and start
another plan. `clank stash push` refused because an untagged local
commit (`ignore experiments/, add k1_generator.py`) came after the
plan's intro. The agent was then left choosing between a `git reset
--hard` that auto mode blocked, and leaving the plan active. The
operator asked for a stash, and clank's job was to do it.

Push today is built on the shared snapshot rewrite (`cli/rewrite.rs`),
and every refusal follows from that engine's model:

- **Foreign commits.** `rewrite::apply_plan` keeps a commit by
  recreating it with its ORIGINAL full tree on a new parent. A foreign
  commit after the intro has a tree that contains the plan's files and
  changes, so keeping it that way would bring the stashed work straight
  back. The unconditional `safety_check` refusal guards exactly that. It
  cannot just be lifted: a foreign commit has to be replayed as its
  CHANGE onto the branch without the plan.
- **A dirty working tree** refuses whenever the rewrite changes the
  tree, because publishing is a `reset --hard`. That includes untracked
  files outside `.clank/`.
- **Uncommitted plan-body edits** refuse (`plan_file_dirty`).
- **"Already stashed"** is an error, not an answer.
- **The engine's own blockers:**
  - a merge in the range (`collect_blockers`);
  - an intro that is a root commit when every commit would be dropped
    (`apply_plan`);
  - a detached HEAD (`current_branch`).

  Each is a normal state for a plan, not a conflict.

Pop refuses a dirty tree outright. On a conflict it leaves a cherry-pick
in progress.

So the fix is not removing guards around the snapshot rewrite. Stash
gets its own transaction:

1. build the whole result in the object database;
2. check it against the working tree;
3. protect what it sets aside;
4. publish with compare-and-swap.

Purge, finish and every other rewrite caller keep the snapshot engine
and its safety unchanged.

## The contract

`clank stash push [<plan>]` sets aside exactly the plan's own commits,
plus the operator's uncommitted tracked work, and always succeeds
unless replaying what remains genuinely conflicts. Either it publishes
the whole result, or it moves nothing.

### One transaction, in this order

1. **Answers before anything is built.** Each of these exits 0 with no
   commit, ref, index or working-tree change, however dirty the tree and
   whatever plan is active:
   - the plan is already stashed: the reply names the pop command;
   - the plan has no commits and nothing uncommitted.
2. **Snapshot: the automatic commit, off the branch.** If the working
   tree's tracked state differs from HEAD, the snapshot is a commit
   object parented on HEAD and referenced by nothing.
   - The working tree's tracked state is the index plus every staged
     and unstaged change: additions, deletions, edits, and a path with
     both kinds of edit, which takes its working-tree content.
   - The commit is built through a temporary index file, so the real
     index and working tree are not touched.
   - Its subject is `[<plan>] clank stash: uncommitted work at stash
     time`. It belongs to the plan and is set aside with it.
   - Untracked and ignored files are never in it.
   - Call the result T0: the snapshot, or HEAD when there was nothing
     to snapshot.
3. **Build the result, in memory.**
   - Walk the plan's range, from the intro to T0, along first parents.
   - The plan's commits and the snapshot are set aside.
   - Every other commit is replayed in order, as its change against its
     first parent (`git_plumbing::replay_change`), onto a running tree.
     The running tree starts at the intro's parent's tree, or the empty
     tree when the intro is a root commit.
   - Each replayed commit keeps its message, author and author date.
   - A foreign merge is replayed as its first-parent change, as one
     ordinary commit, and push names it as linearized. A merge the plan
     owns is set aside like any of its commits.
   - Call the new tip N:
     - the last replayed commit;
     - otherwise the intro's parent;
     - otherwise, when the intro was a root and nothing remains, a new
       root commit with the empty tree, subject `clank stash: <plan> set
       aside; it was this branch's whole history`.
   - **The one refusal: a replay conflict.** Push names the foreign
     commit and the conflicting paths, and writes nothing a ref,
     index or working tree can see.
4. **Check the checkout, before it is done.** The working tree's
   tracked state equals T0's tree, so publishing changes exactly the
   paths that differ between T0's tree and N's tree. Each path N writes
   is checked against what is on disk, ignored files included. It is
   obstructed by:
   - an untracked or ignored entry at that path;
   - a file or symlink at any ancestor of it, where N needs a
     directory;
   - a directory at that path holding untracked or ignored content,
     where N needs a file.

   Any obstruction is refused before anything moves, naming every
   obstructing path. That is a conflict too: publishing would destroy
   it. Every other untracked or ignored file is left exactly as it is,
   and push prints one warning line naming the untracked ones it left.
6. **Fence.** Other agents and editors do not take clank's plan
   mutation lock, so the inputs step 2 captured may have changed by
   now. Before anything is written, all of them are read again:
   - the HEAD binding: the branch HEAD names, or that it is detached;
   - HEAD's commit;
   - the real index, by content;
   - the working tree's tracked state, as the snapshot tree it would
     make now.

   Handling changes:
   - A HEAD binding that changed is refused, because the operator
     switched what they are on. This includes a switch to another branch
     at the same commit, and the branch that was left is not rewritten.
   - A change to the index or the tracked working tree means newer work.
     The transaction is rebuilt from step 1 with it, at most three times,
     then refused with "the work kept changing while stashing".
   - Either way nothing has moved, and newer work is never overwritten.
7. **Protect, then publish.**
   - The protective ref `refs/clank/stash/<plan>` is set to T0, which
     reaches every set-aside commit, the snapshot included. The record's
     `shas` are the set-aside commits, oldest first. No foreign commit
     is in the record.
   - The captured branch, or HEAD itself when detached, moves from the
     captured commit to N by compare-and-swap.
   - The working tree is moved from T0's tree to N's, touching only the
     paths that differ, and each path is written only while its disk
     content still equals T0's. An edit that lands after the fence
     stops the move at that path. The transaction then rolls back:
     - every path already moved is restored from T0;
     - the original index file is put back;
     - the branch is moved back to the captured commit by
       compare-and-swap;
     - the protective ref and record are removed.

     The newer edit is untouched, and push reports it.
   - The window that remains is the single write of one path, the
     same as git's own checkout.
   - Last, the index becomes N's tree.
   - A failure that cannot roll back keeps the protective ref and
     record, and says how to recover, as push does today.

`--dry` runs steps 1 to 5 and prints what it would do:

- the automatic commit and the paths it would hold;
- the commits it would set aside and the foreign commits it would
  replay;
- the warnings, or the conflict or obstruction it would refuse with.

It writes no ref, index, working-tree file, record or stash
directory. Loose objects nothing references are allowed.

### Pop, the same way

`clank stash pop [<plan>]`:

- **One active plan.** Pop keeps today's admission rule
  (`plan_mutation::lock` then `slot.empty_state()`): with any plan
  active, pop is refused before any snapshot or replay, and nothing
  moves. That rule is what keeps a repo to one active plan, so the
  restored plan's intro is never one the fold ignores. It is not a
  dirty-tree refusal and it stays.
- **Snapshot.** In the empty slot, uncommitted tracked work becomes an
  automatic commit on the current branch. It is untagged, with subject
  `clank stash pop: uncommitted work at pop time`. It is never the
  restored plan's, because no plan owns it.
- **Replay.** The recorded commits are replayed, in order and in memory,
  onto the snapshot (or HEAD), each with its message and author.
- **Refusals.** A replay conflict is refused, naming the commit and
  paths. So is a checkout obstruction, checked as for push. Either way
  nothing moves: no snapshot lands, no cherry-pick is left in progress,
  and the protective ref and record are untouched.
- **Fence and publish** as for push: the same captured inputs are read
  again, with the same rebuild, refusal and guarded checkout. Only after
  the branch and working tree are moved are the protective ref and
  record removed.

## Scope

- `cli/stash.rs`:
  - push and pop rebuilt on the transaction;
  - `safety_check`, the dirty-tree refusal and `plan_file_dirty` go,
    because the transaction subsumes them;
  - "already stashed" becomes an answer.
- New stash transaction module (`cli/stash_txn.rs` or inside
  `stash.rs`): snapshot, replay, the obstruction check, and publish.
- Git access stays in the layer, each primitive with a "why not gix"
  line where it is a subprocess:
  - `git_plumbing`: the snapshot commit through a temporary index; a
    commit with given parents, message and author; a two-tree checkout
    of the changed paths; compare-and-swap on HEAD or its branch;
  - `git_io`: what is on disk at each path the checkout writes.
- `cli/rewrite.rs` is not changed. Purge, finish and the other callers
  keep the snapshot engine and its refusals.

## Acceptance

Through the in-process stash core on real temp repos, with no binary
spawned:

- **The reported case.** An untagged commit after the intro adds an
  independent file. After push:
  - none of the plan's files or implementation changes are in N's tree
    or the working tree;
  - the foreign file is present with its content, message and author.

  After pop, the plan's changes are back and the foreign commit exists
  once.
- **Another plan's commit interleaved** is kept as its change. This plan
  is stashed.
- **A dependent foreign commit,** editing a file only the plan created,
  is refused, naming the commit and path. HEAD, every ref, the stash
  directory, the index (staged and unstaged distinguished) and the
  working tree are byte-identical to before.
- **The automatic commit:**
  - The fixture has a staged addition, a staged deletion, an unstaged
    edit, and one path with both a staged and an unstaged edit. Push
    makes one automatic commit with the documented subject. It is set
    aside, and the working tree is clean of those changes after push.
    Pop restores exactly that content.
  - A clean tree makes no automatic commit.
- **Dirty plus a foreign conflict, and dirty plus an untracked
  obstruction:** each is refused with nothing moved. The index's staged
  versus unstaged state is unchanged, and no automatic commit is
  reachable from any ref.
- **Untracked files:**
  - An ordinary untracked file and an ignored file outside the changed
    paths survive untouched, and push warns about the untracked one.
  - An untracked directory with a note inside, at a path where N puts a
    tracked file, is refused, naming the note. So is an ignored file at
    a path N writes.
- **Dry runs:**
  - A dirty `--dry` describes the automatic commit and changes no
    ref, index, working tree or stash directory.
  - A dirty already-stashed push exits 0 and makes no commit, both
    with this plan active and with another one active.
- **The engine's blockers:**
  - A detached HEAD: push moves HEAD and the plan is stashed.
  - A foreign merge in the range is linearized, its merged content
    kept.
  - A root intro with nothing else: the empty root commit, and pop
    restores the plan onto it.
- **Pop with dirty work:**
  - Independent dirty work in an empty slot: an untagged automatic
    commit, and the plan's commits land after it.
  - Another plan active, with dirty work: refused before anything is
    built. No snapshot is made, there is no second intro, and HEAD,
    refs, index and working tree are unchanged.
  - Dirty work that conflicts with a recorded commit: refused, naming
    both. Nothing moves, there is no cherry-pick in progress, and the
    ref and record are intact.
- **The fence, by controlled interleaving.** A test seam runs between
  the checks and the fence:
  - An uncommitted tracked edit lands there. The rebuild takes it into
    the automatic commit, and nothing newer is overwritten.
  - An index-only change, staging an edit already on disk: rebuilt, and
    the result holds it.
  - A switch to another branch at the same commit: refused. Neither
    branch is rewritten, and no protective ref or record is left.
  - HEAD's commit moved: refused, and nothing is left behind.
  - An edit landing during the guarded checkout, through a seam before
    a path's write: the move stops, and everything rolls back. The edit
    is intact, the branch and index are as captured, and no protective
    ref or record is left.
  - Pop gets the same edit and branch-switch cases.
- **Other callers.** Purge and finish keep their existing refusal
  tests, unchanged.
- **Mutations.** Each new path is mutation-checked:
  - replaying a foreign commit with its full tree instead of its
    change;
  - dropping the obstruction check;
  - committing the snapshot on the live branch before the checks;
  - fencing HEAD's commit only, not its binding, the index or the tree;
  - a checkout that writes a path without re-checking it;
  - pop without the empty-slot check;
  - pop tagging its snapshot with the restored plan;
  - publishing without compare-and-swap.

## As built

- **`cli/stash_txn.rs` (new): the transaction.**
  - `Capture`: the HEAD binding (`git_io::head_binding_at`; `None` when detached), HEAD's commit, an index digest, and the tracked tree.
    - The index digest is over its entries' path, mode, object and stage, not its bytes, because any `git status` rewrites stat data.
    - The tracked tree is `git_plumbing::tracked_tree`: a scratch copy of the index with `git add -u` over it, then `write-tree`.
  - `fence` reads all four again. A changed binding or commit is `Moved`; a changed index or tracked tree is `Newer`.
  - `snapshot`: the automatic commit (`git_plumbing::commit_new`) of the tracked tree on HEAD, on no ref, or `None` when it is HEAD's tree.
  - `replay_onto`: each commit, oldest first, as `git_plumbing::replay_change` (its change against its first parent, merged in memory) onto a running tree, written by `replay_commit` (author and message kept). The first conflict is `Built::Conflict` with its paths. A merge is marked `linearized`.
  - `preflight`, which two checks back:
    - **`in_the_way`, the stash's own.** For every path the new tree adds, it flags an entry on disk at that path, untracked or ignored content in a directory there, or a file at a parent directory. Git's checkout guard leaves ignored files out and would overwrite them.
    - **`git_plumbing::move_worktree` dry, git's own.** A scratch index of the old tree refreshed against the disk, then `read-tree -n -m -u old new`. Git verifies every path before writing any: a tracked path that is not the old tree's, and an untracked file it would overwrite or remove.
  - `publish`:
    1. `in_the_way` again.
    2. Git's own `HEAD.lock` is taken (`git_plumbing::HeadLock`) and held to the end, so no git command can switch, attach or detach HEAD meanwhile. Under it, HEAD must still name the captured branch, or be detached at the captured commit; otherwise the result is `RolledBack`, and neither branch nor working tree is touched.
    3. A compare-and-swap of the captured branch from the captured commit to the tip. A lost race is `RolledBack`, nothing moved. A detached HEAD is moved by the lock itself, only when it commits.
    4. The real `move_worktree`. It runs git's check alone first, and only that check's refusal is `Moved::Refused`: proven to have written nothing, so the branch is swapped back (`RolledBack`). A write that fails after the check passed (a required filter, the disk, a permission) is `Moved::Partial`. The branch then stays at the tip, the HEAD lock commits, and the result is `Published::Partial`.
    5. `install_index` of the scratch index under git's `index.lock`, only while the index's digest is still the captured one (`IndexKept` otherwise).

    `publish` returns an error only while nothing has moved. Once the real write has begun, every failure is `Published::Partial { lines, at_tip }`, so a caller can release protection only on `Done`, `IndexKept`, or `RolledBack`, which is a certified no-write. That covers the write itself, a HEAD lock that cannot commit (a detached HEAD then stays at the captured commit, `at_tip: false`), a branch that cannot be put back, and an index that cannot be written.
- **`cli/stash.rs`:**
  - push: a named plan already stashed is answered before the fold, and an active one after it. Exit 0 either way, nothing touched.
  - push then runs `ATTEMPTS = 3` rounds of capture, snapshot, replay of the foreign commits onto the intro's parent, preflight, `Stage::Checked`, fence and `Stage::Fenced`. `Newer` goes round again; `Moved`, a conflict or an obstruction refuses.
  - Then protect (ref at T0, record of the plan's commits plus the snapshot) and publish. `RolledBack`, or an error (nothing moved), removes the ref and record. `Partial` keeps both and says where HEAD is: the snapshot stays reachable from the protective ref, and the message gives the recovery for HEAD at the tip or still at the captured commit.
  - An intro that was a root with nothing remaining gets the empty root commit. `--to-queue` reads the body from T0. `--dry` prints the automatic commit's paths, the kept commits and any refusal, and writes no ref, record or file.
  - pop keeps `slot.empty_state()` first. It then runs the same rounds with an untagged snapshot and replays the recorded commits onto it. On success, a compare-and-delete of the protective ref and the record. Before publishing, pop points its own ref, `refs/clank/stash-pop/<plan>`, at the result, so the pre-pop snapshot is reachable whatever happens to HEAD. The ref is removed on any outcome but `Partial`, which keeps it with the protective ref and record and names it.
    - Its lifetime: it is taken create-only just before the write, and only by the attempt that created it.
    - A partial attempt leaves it. While it exists, every later pop of the plan refuses before anything is built, naming it and its commit, and saying how to recover (`git reset --hard <it>`) and then delete it.
    - So a retry never overwrites the last copy of an earlier attempt's work.
  - `--dry` takes no stash lock, so it creates neither `.clank/stash/` nor its lock file; the plan mutation lock still serializes it.
  - `safety_check`, `plan_file_dirty` and the dirty-tree refusal are gone; `cli/rewrite.rs` is unchanged.
  - `push_with` and `pop_with` take the `Stage` hook, as a test seam.
- **Git layer:**
  - `git_io`: `head_binding_at`, `first_parent_at`, `index_digest_at`, `tree_files_at`.
  - `git_plumbing`: `commit_new`, `ScratchIndex`, `tracked_tree`, `move_worktree`/`Moved`, `install_index`.
  - Each subprocess has its why-not-gix line.
- **The guarded checkout is git's.** `read-tree -m -u` checks every path before it writes any, so a refusal by that check writes nothing and needs no per-path rollback.
  - Writing can still fail after the check passes. That is not a refusal: it is `Partial`, and everything protected stays protected, with an honest message. There is no attempt to restore paths by hand.
  - The window that remains for a concurrent edit is between git's check and its write. The in-checkout interleaving is tested as an edit landing after the fence, before the checkout (`Stage::Fenced`). A real write failing part way is tested with a required smudge filter that always fails.
- **Tests (`tests/it/stash_dwim.rs`, new; `stash_integration.rs` updated):**
  - the reported untagged commit kept, with its author and content; the plan's files and its removal undone; pop restores it, once;
  - another plan's commit kept;
  - a dependent foreign commit refused naming the commit and path, with HEAD, binding, refs, index entries, staged and unstaged diffs, and status unchanged;
  - every kind of uncommitted work (staged addition, staged deletion, staged-then-unstaged edit, plan-body edit) as one automatic commit, set aside, and restored byte-for-byte by pop; a clean tree makes none;
  - an untracked directory where a file returns, and an ignored file there, each refused with nothing moved; other untracked and ignored files left alone;
  - a dirty dry run that changes nothing;
  - already stashed: an answer when dirty, and when another plan is active;
  - a detached HEAD stashed in place, `main` untouched;
  - a foreign merge kept as one single-parent commit with its content;
  - the whole-history plan leaving the empty root, and popping onto it;
  - pop's untagged automatic commit before the plan's commits; pop refused with another plan active and dirty work, nothing moved; pop refused for conflicting dirty work, nothing moved, with no cherry-pick in progress;
  - interleavings, each moving nothing it should not:
    - an edit after the checks is rebuilt into the automatic commit;
    - staging after the checks is rebuilt;
    - a switch to a branch at the same commit is refused;
    - a commit after the checks is refused;
    - a commit after the fence loses the compare-and-swap, and the late commit stays;
    - an ignored file appearing after the fence is refused by publish's own check, not overwritten;
    - a branch switch after the fence is refused for push and pop, rewriting neither branch; so is attaching a detached HEAD;
  - a write failing part way (a required filter that fails): push keeps the protective ref, the record and the only copy of the dirty edit reachable, and says the move is partial; pop keeps the pre-pop snapshot reachable from the branch, with the ref and record;
  - a dirty dry run creates no `.clank/stash/` at all;
  - a detached HEAD that cannot be moved after the checkout (the HEAD lock made unwritable by a smudge): push keeps the record and the only copy of the dirty edit on the protective ref, says HEAD is still at the captured commit, and leaves it there. Pop keeps its pre-pop snapshot reachable from `refs/clank/stash-pop/<plan>`. A successful pop leaves no ref of its own. A pop after a partial one refuses, leaving that recovery ref at the same commit and the stash intact; once the ref is deleted by hand, pop works again;
    - an edit after the fence makes the checkout refuse, and everything rolls back;
    - pop's edit and branch-switch cases.
- **Mutations, each caught:**
  - replaying a foreign commit with its full tree instead of its change;
  - `in_the_way` reporting nothing;
  - publishing without its own obstruction check;
  - committing the snapshot on the live branch;
  - fencing without the binding, or without the index and tracked tree;
  - a checkout that writes without re-checking (`read-tree --reset -u`);
  - publishing without compare-and-swap;
  - pop without the empty-slot check;
  - pop tagging its snapshot with the restored plan;
  - a partial write reported as a refusal;
  - publishing without the HEAD binding check;
  - `--dry` taking the stash lock;
  - a HEAD lock that fails to commit escaping as an error;
  - pop without its own ref;
  - pop keeping its own ref on success;
  - pop not refusing while an earlier attempt's recovery ref exists, alone and together with taking that ref without create-only.

  Without `in_the_way` at the preflight alone, the publish-time check still refuses the ignored file, so the preflight site is only caught together with the publish site.
