# post-rewrite-hook-chaining
# Clank's post-rewrite hook coexists with others and honours core.hooksPath

## Why

RELEASE-CHECKLIST.md §1 `post-rewrite-hook-chaining`. The post-rewrite
hook moves review feedback to the new shas when history is rewritten.
Two cases break it today:
- If `.git/hooks/post-rewrite` already has someone else's content,
  `clank init` warns and skips it, and `--force-hooks` overwrites it.
- The hook is always written to `<common-dir>/hooks` and
  `core.hooksPath` is never read. A repo using a hook manager (husky,
  lefthook, pre-commit) therefore never runs it, and feedback silently
  fails to follow a rebase.

## What changes

### One resolver for the hook git will actually run

`git_io::post_rewrite_slot(worktree) -> HookSlot` resolves the hook the
way git does, and installation and `doctor` both use it.

**Where it looks:**
- `core.hooksPath` when it is set. An absolute path is used as is. A
  relative one is resolved against the worktree's top level, which is
  where git runs a hook in a non-bare repo.
- `core.hooksPath = /dev/null` means hooks are disabled.
- Otherwise `<common-dir>/hooks`, which a linked worktree shares.

**What it reports**, as the slot's state:
- **Missing:** there is no file.
- **Disabled:** hooksPath is `/dev/null`, or the file exists but is not
  executable. Git ignores a non-executable hook, so this counts as
  deliberately off.
- **Linked:** the slot is a symlink, typically placed by a hook manager
  or a dotfiles setup. Its target is recorded.
- **Managed:** a regular executable whose dispatch clank must not
  disturb. It is recognised by any of these:
  - the hooks directory is a known manager's (husky's `.husky/_`);
  - the file carries a manager's generated marker (lefthook, pre-commit,
    husky);
  - it dispatches on its own name (`$0`, or `basename "$0"`). Renaming it
    would change what it runs.
- **Clank:** clank's own hook.
- **ClankChaining:** clank's wrapper, together with the chained file it
  runs.
- **Foreign:** any other plain executable hook.

### What `clank init` does for each state

- **Missing:** install clank's hook.
- **Clank / ClankChaining:** leave it alone. Installing twice is a no-op.
- **Foreign:** chain it (next section).
- **Disabled, Linked, Managed:** write nothing. clank does not own these
  files.

### Ownership and wiring are two separate facts

The slot's state above says who owns the hook and what `init` may do.
Whether rewire is actually wired is a second fact. `doctor` reports it as
one of Wired, Delegated, Unwired or Off.

- **Wired:** the slot is Clank or ClankChaining, and git would execute
  it.
- **Delegated:** the slot is Linked or Managed, and the owner's hook runs
  clank's documented stanza. clank recognises the stanza by its marker
  line, `# clank rewire (delegated)`, together with the command
  `clank rewire --from-stdin`. It looks only in bounded places:
  - the file git executes, after resolving a symlink;
  - for husky, `.husky/post-rewrite`, which husky's dispatcher runs;
  - for lefthook, a `post-rewrite` command in `lefthook.yml` or
    `.lefthook.yml` that runs `clank rewire --from-stdin` with
    `use_stdin: true`. This counts only when that command is the sole
    `use_stdin` consumer in `post-rewrite`. Lefthook gives the stream to
    one `use_stdin` command only
    (https://lefthook.dev/configuration/use_stdin/), so with another
    consumer present the slot stays Unwired.

  Recognising the stanza gives clank no ownership and no permission to
  rewrite any of these files.
- **Unwired:** a Linked or Managed hook without the stanza. `doctor` warns
  and prints the recipe for that owner:
  - a plain script, a symlink target, or a husky hook gets the shell
    stanza for the top of the hook. It replays stdin, so the hook's own
    consumers still see git's stream:

    ```sh
    # clank rewire (delegated)
    clank_in=$(mktemp); cat > "$clank_in"
    clank rewire --from-stdin < "$clank_in" || echo "clank rewire failed" >&2
    exec < "$clank_in"; rm -f "$clank_in"
    ```
  - lefthook gets the YAML for a `post-rewrite` command with
    `use_stdin: true`, but only when no other `post-rewrite` command uses
    stdin. When one does, `doctor` says lefthook cannot share the stream
    and that its consumer must take clank's shell stanza instead. That
    consumer is the user's own script, so the stanza can replay the
    stream into it;
  - pre-commit has no supported recipe that keeps stdin, so `doctor` says
    so, and the slot stays Unwired.
- **Off:** the slot is Disabled. `doctor` says that rewrite feedback will
  not follow rebases while hooks are off.

`--force-hooks` is removed. A sweep of callers, the README, the book,
skills and tests leaves no stray reference; finished plans are history.

### Chaining a plain foreign hook

**Installing, under `<hooks>/.clank-hook.lock`:**
1. If `post-rewrite.clank-chained` already exists, it must be
   byte-identical to the current foreign hook (an interrupted earlier
   install), in which case it is reused. Otherwise init refuses and names
   both files. Nothing is overwritten.
2. Copy the foreign hook to `post-rewrite.clank-chained`, created with
   O_EXCL, keeping its mode.
3. Write the wrapper to a temp file in the same directory and rename it
   over `post-rewrite`. A failure before the rename leaves the original
   hook where it was. A failure after it leaves a working wrapper.

**The wrapper** is POSIX sh and carries clank's marker:
- It reads git's stdin once, to EOF, into a temp file.
- It runs `clank rewire --from-stdin < tmp` when `clank` is on PATH.
  That is the CLI's real contract: `RewireArgs` takes `--from-stdin` and
  no positional argument. A missing
  clank or a failing rewire prints one stderr line and does not stop the
  chain.
- It then runs `post-rewrite.clank-chained "$@" < tmp`, in the same cwd
  and environment, so both consumers get the identical byte stream and
  arguments that git sent.
- Its exit status is the chained hook's.
- It removes the temp file on every exit path, through a trap.

## Acceptance

**Real git, real wrapper.** Fixture repos are built with git in tests.
The tests install the generated wrapper through the production installer
and then run a real `git commit --amend` and a real `git rebase`.
- `clank` on PATH is a shim that records its argv and the stdin bytes it
  consumed. The recorded argv must parse through the production `Cli`
  (`Cli::try_parse_from`) as a `rewire` command, so a permissive
  recorder cannot hide a broken invocation.
- The foreign hook independently records its own arguments and stdin.
- Both recordings equal git's stream (old and new shas). The foreign hook
  receives git's argument (`amend`, then `rebase`), and clank receives
  `rewire --from-stdin`.
- Removing the shim from PATH, and making the shim exit 1, each still
  run the foreign hook with the full stream.

**The resolver**, one test per case, with installation and doctor
agreeing:
- relative and absolute `core.hooksPath`;
- a linked worktree, which shares the common dir's hooks;
- `/dev/null`;
- a non-executable hook;
- a symlinked hook;
- a husky, lefthook or `$0`-dispatching hook, which is Managed;
- an unrelated existing `post-rewrite.clank-chained`, which is refused
  with nothing changed;
- a second install, which is a no-op.

**Delegated integration.** A husky-style hook and a symlinked hook each
start Unwired, with a second consumer in the owner's script that records
stdin.
- Applying the documented stanza and running a real rewrite makes the
  shim record the stream, and the owner's consumer still records the
  identical stream.
- `doctor` then reports Delegated.
- `clank init` leaves the owner's files byte-identical before and after.

Lefthook, by text, without running lefthook:
- a config whose only `post-rewrite` stdin consumer is the documented
  command is Delegated;
- a config with that command plus another `use_stdin: true` consumer is
  Unwired, and doctor names the competing command.

**Doctor** calls the slot Wired only when git would execute it and it is
clank's hook or wrapper. A clank marker in a non-executable file, or in
a hook outside the effective directory, is not wired.

**No clank process is spawned.** The shim stands in for clank, and the
wrapper's call to `clank rewire` is exercised through it.

## As landed

- **25194862:** the plan.
  - `git_io::post_rewrite_slot` is the resolver.
  - The new `post_rewrite.rs` holds the slot classification, the
    separate wiring state (Wired/Delegated/Unwired/Off) and its
    owner-specific recipes, and `install`: under the lock, it classifies
    again, then installs, refreshes or chains through an O_EXCL copy and
    a temp-then-rename wrapper.
  - The wrapper replays git's stdin to `clank rewire --from-stdin`, then
    to the chained hook with git's arguments.
  - `--force-hooks` is removed.
  - It went beyond the plan in four places:
    - an older clank hook body is refreshed;
    - a plain hook that already carries the stanza is not chained;
    - a wrapper whose chained file is missing is an error;
    - a symlink whose target git won't run is Off.
- **98ffa755:**
  - Any `$0` use other than `${0%/*}` or `dirname "$0"` is name
    dispatch, so the hook is Managed.
  - `chained_defect` refuses a chained file that is missing, a symlink,
    not a regular file, the same inode as the hook, or not executable.
  - The lefthook reader returns a Result. Composed configs, unread
    configs, and syntax outside the supported block form are all
    undetermined.
- **dc2e6060:**
  - A lefthook jobs entry's opening line is a field like any other.
  - One `runnable()` predicate, `access(X_OK)` for the caller, decides
    executability at all three sites. It is one small `unsafe` libc call
    with a SAFETY comment; the queued `unsafe-lives-in-named-modules`
    should move it to rustix.
- **Tests:** 42 in `post_rewrite::tests`, including real amend and
  rebase runs through the real wrapper with a recorder shim whose argv is
  parsed by the production `Cli`. Every named mutation is caught.
