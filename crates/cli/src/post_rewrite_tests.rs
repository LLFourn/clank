use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::*;
use crate::cli::doctor::{CheckResult, CheckStatus, check_post_rewrite_hook};

/// A repo with one commit, plus a recorder directory and a `bin/` that is
/// the only clank-bearing entry of the PATH its git runs with. The real
/// clank is never reachable from that PATH: `clank` there is the shim or
/// nothing.
struct Fixture {
    tmp: tempfile::TempDir,
    top: PathBuf,
    rec: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let (top, rec, bin) = (root.join("repo"), root.join("rec"), root.join("bin"));
        for dir in [&top, &rec, &bin] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let f = Self { tmp, top, rec, bin };
        f.git(&["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(f.top.join("base.txt"), "base\n").unwrap();
        f.git(&["add", "base.txt"]);
        f.git(&["commit", "--quiet", "-m", "base"]);
        f
    }

    fn root(&self) -> PathBuf {
        dunce::canonicalize(self.tmp.path()).unwrap()
    }

    fn path_env(&self) -> String {
        let git = which::which("git").unwrap();
        let mut dirs = vec![self.bin.clone()];
        for dir in [
            git.parent().unwrap(),
            Path::new("/usr/bin"),
            Path::new("/bin"),
        ] {
            if !dirs.iter().any(|d| d == dir) {
                dirs.push(dir.to_path_buf());
            }
        }
        let path = std::env::join_paths(dirs).unwrap().into_string().unwrap();
        let clank = which::which_in("clank", Some(&path), &self.top).ok();
        assert!(
            clank.is_none() || clank == Some(self.bin.join("clank")),
            "the test PATH must reach only the shim, found {clank:?}"
        );
        path
    }

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .env("PATH", self.path_env())
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.top, args)
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// `clank` on the test PATH: records its argv and the stdin it read.
    fn shim(&self, exit: i32) {
        let script = format!(
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{rec}/clank.argv'\ncat > '{rec}/clank.stdin'\nexit {exit}\n",
            rec = self.rec.display()
        );
        write_exec(&self.bin.join("clank"), &script);
    }

    /// A consumer that records what it was given under `who`, then exits
    /// with `exit`.
    fn recorder(&self, who: &str, exit: i32) -> String {
        format!(
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{rec}/{who}.argv'\ncat > '{rec}/{who}.stdin'\nexit {exit}\n",
            rec = self.rec.display()
        )
    }

    fn stdin_of(&self, who: &str) -> String {
        std::fs::read_to_string(self.rec.join(format!("{who}.stdin")))
            .unwrap_or_else(|e| panic!("{who} recorded no stdin: {e}"))
    }

    fn argv_of(&self, who: &str) -> Vec<String> {
        let raw = std::fs::read_to_string(self.rec.join(format!("{who}.argv"))).unwrap();
        raw.split_terminator('\0').map(str::to_string).collect()
    }

    fn ran(&self, who: &str) -> bool {
        self.rec.join(format!("{who}.stdin")).exists()
    }

    fn hooks(&self) -> PathBuf {
        self.top.join(".git/hooks")
    }

    fn slot(&self) -> HookSlot {
        crate::git_io::post_rewrite_slot(&self.top).unwrap()
    }

    fn doctor(&self) -> CheckResult {
        check_post_rewrite_hook(&self.top).unwrap()
    }

    /// `git commit --amend`, returning the stream git hands post-rewrite.
    fn amend(&self) -> String {
        let old = self.head();
        self.git(&["commit", "--quiet", "--amend", "-m", "amended"]);
        format!("{old} {}\n", self.head())
    }

    /// Rebase two topic commits onto an advanced main, returning the
    /// stream git hands post-rewrite.
    fn rebase(&self) -> String {
        self.git(&["checkout", "--quiet", "-b", "topic"]);
        for name in ["t1", "t2"] {
            std::fs::write(self.top.join(name), name).unwrap();
            self.git(&["add", name]);
            self.git(&["commit", "--quiet", "-m", name]);
        }
        let old: Vec<String> = self
            .git(&["rev-list", "--reverse", "main..topic"])
            .lines()
            .map(str::to_string)
            .collect();
        self.git(&["checkout", "--quiet", "main"]);
        std::fs::write(self.top.join("m1"), "m1").unwrap();
        self.git(&["add", "m1"]);
        self.git(&["commit", "--quiet", "-m", "m1"]);
        self.git(&["checkout", "--quiet", "topic"]);
        self.git(&["rebase", "--quiet", "main"]);
        let new: Vec<String> = self
            .git(&["rev-list", "--reverse", "main..topic"])
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(old.len(), 2);
        assert_eq!(new.len(), 2);
        old.iter()
            .zip(&new)
            .map(|(o, n)| format!("{o} {n}\n"))
            .collect()
    }
}

fn write_exec(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every entry under `root`: bytes or symlink target, and mode.
fn tree(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    let mut out = BTreeMap::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = path.symlink_metadata().unwrap();
            let bytes = if meta.file_type().is_symlink() {
                std::fs::read_link(&path)
                    .unwrap()
                    .into_os_string()
                    .into_encoded_bytes()
            } else if meta.is_dir() {
                dirs.push(path.clone());
                Vec::new()
            } else {
                std::fs::read(&path).unwrap()
            };
            out.insert(path, (bytes, meta.permissions().mode()));
        }
    }
    out
}

/// The recorded argv is what the production CLI parses as rewire.
fn assert_parses_as_rewire(argv: &[String]) {
    use crate::cli::command::{Cli, Command};
    use clap::Parser;
    let parsed = Cli::try_parse_from(std::iter::once("clank".to_string()).chain(argv.to_vec()))
        .unwrap_or_else(|e| panic!("{argv:?} does not parse: {e}"));
    let Command::Rewire(args) = parsed.command else {
        panic!("{argv:?} is not a rewire command");
    };
    assert!(args.from_stdin);
    assert!(args.repo.is_none());
}

fn chain_foreign(f: &Fixture, exit: i32) {
    write_exec(&f.hooks().join(HOOK), &f.recorder("foreign", exit));
    assert_eq!(install(&f.top).unwrap(), HookChange::Chained);
    assert_eq!(f.slot().state, SlotState::ClankChaining { current: true });
}

#[test]
fn an_amend_hands_rewire_and_the_chained_hook_gits_stream() {
    let f = Fixture::new();
    f.shim(0);
    chain_foreign(&f, 0);
    let stream = f.amend();
    assert_eq!(f.stdin_of("clank"), stream);
    assert_eq!(f.stdin_of("foreign"), stream);
    assert_eq!(f.argv_of("foreign"), ["amend"]);
    assert_eq!(f.argv_of("clank"), ["rewire", "--from-stdin"]);
    assert_parses_as_rewire(&f.argv_of("clank"));
}

#[test]
fn a_rebase_hands_rewire_and_the_chained_hook_gits_stream() {
    let f = Fixture::new();
    f.shim(0);
    chain_foreign(&f, 0);
    let stream = f.rebase();
    assert_eq!(stream.lines().count(), 2);
    assert_eq!(f.stdin_of("clank"), stream);
    assert_eq!(f.stdin_of("foreign"), stream);
    assert_eq!(f.argv_of("foreign"), ["rebase"]);
    assert_parses_as_rewire(&f.argv_of("clank"));
}

#[test]
fn without_clank_on_path_the_chained_hook_still_gets_the_stream() {
    let f = Fixture::new();
    chain_foreign(&f, 0);
    let stream = f.amend();
    assert!(!f.ran("clank"));
    assert_eq!(f.stdin_of("foreign"), stream);
    assert_eq!(f.argv_of("foreign"), ["amend"]);
}

#[test]
fn a_failing_rewire_still_runs_the_chained_hook_with_the_stream() {
    let f = Fixture::new();
    f.shim(1);
    chain_foreign(&f, 0);
    let stream = f.amend();
    assert_eq!(f.stdin_of("clank"), stream);
    assert_eq!(f.stdin_of("foreign"), stream);
}

#[test]
fn the_wrapper_exits_with_the_chained_hooks_status_and_leaves_no_temp_file() {
    let f = Fixture::new();
    f.shim(0);
    chain_foreign(&f, 3);
    let tmpdir = f.root().join("tmp");
    std::fs::create_dir_all(&tmpdir).unwrap();
    let mut child = Command::new(f.slot().path())
        .arg("amend")
        .current_dir(&f.top)
        .env("PATH", f.path_env())
        .env("TMPDIR", &tmpdir)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"a b\n").unwrap();
    drop(child.stdin.take());
    assert_eq!(child.wait().unwrap().code(), Some(3));
    assert_eq!(f.stdin_of("foreign"), "a b\n");
    assert_eq!(std::fs::read_dir(&tmpdir).unwrap().count(), 0);
}

#[test]
fn a_relative_hooks_path_is_resolved_against_the_top_level() {
    let f = Fixture::new();
    f.git(&["config", "core.hooksPath", ".githooks"]);
    assert_eq!(install(&f.top).unwrap(), HookChange::Wrote);
    let hook = f.top.join(".githooks").join(HOOK);
    assert_eq!(f.slot().path(), hook);
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), POST_REWRITE_BODY);
    assert!(!f.hooks().join(HOOK).exists());
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Ok, "{}", doctor.message);
    assert!(doctor.message.contains(&hook.display().to_string()));
    f.shim(0);
    let stream = f.amend();
    assert_eq!(
        f.stdin_of("clank"),
        stream,
        "git runs the hook clank put there"
    );
}

#[test]
fn an_absolute_hooks_path_is_used_as_is() {
    let f = Fixture::new();
    let dir = f.root().join("elsewhere/hooks");
    f.git(&["config", "core.hooksPath", dir.to_str().unwrap()]);
    assert_eq!(install(&f.top).unwrap(), HookChange::Wrote);
    assert_eq!(f.slot().path(), dir.join(HOOK));
    assert_eq!(
        std::fs::read_to_string(dir.join(HOOK)).unwrap(),
        POST_REWRITE_BODY
    );
    assert!(!f.hooks().join(HOOK).exists());
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Ok, "{}", doctor.message);
    assert!(
        doctor
            .message
            .contains(&dir.join(HOOK).display().to_string())
    );
}

#[test]
fn a_linked_worktree_installs_into_the_shared_hooks_dir() {
    let f = Fixture::new();
    let wt = f.root().join("wt");
    f.git(&["worktree", "add", "--quiet", wt.to_str().unwrap()]);
    assert_eq!(install(&wt).unwrap(), HookChange::Wrote);
    let shared = f.hooks().join(HOOK);
    assert_eq!(std::fs::read_to_string(&shared).unwrap(), POST_REWRITE_BODY);
    for worktree in [&wt, &f.top] {
        let slot = crate::git_io::post_rewrite_slot(worktree).unwrap();
        assert_eq!(dunce::canonicalize(slot.path()).unwrap(), shared);
        let doctor = check_post_rewrite_hook(worktree).unwrap();
        assert_eq!(doctor.status, CheckStatus::Ok, "{}", doctor.message);
    }
    assert_eq!(install(&f.top).unwrap(), HookChange::UpToDate);
}

#[test]
fn hooks_path_dev_null_is_off_and_nothing_is_written() {
    let f = Fixture::new();
    f.git(&["config", "core.hooksPath", "/dev/null"]);
    let before = tree(&f.top.join(".git/hooks"));
    assert_eq!(
        install(&f.top).unwrap(),
        HookChange::Left(Left::Off(Disabled::HooksOff))
    );
    assert_eq!(tree(&f.top.join(".git/hooks")), before);
    assert_eq!(wiring(&f.slot()), Wiring::Off(Disabled::HooksOff));
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Warn);
    assert!(doctor.message.contains("/dev/null"), "{}", doctor.message);
}

#[test]
fn a_non_executable_hook_is_off_and_left_alone_even_when_it_is_clanks() {
    let f = Fixture::new();
    for body in ["#!/bin/sh\necho mine\n", POST_REWRITE_BODY] {
        let hook = f.hooks().join(HOOK);
        std::fs::write(&hook, body).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = tree(&f.hooks());
        assert_eq!(
            install(&f.top).unwrap(),
            HookChange::Left(Left::Off(Disabled::NotExecutable))
        );
        assert_eq!(tree(&f.hooks()), before);
        assert_eq!(wiring(&f.slot()), Wiring::Off(Disabled::NotExecutable));
        let doctor = f.doctor();
        assert_eq!(doctor.status, CheckStatus::Warn);
        assert!(
            doctor.message.contains("not executable"),
            "{}",
            doctor.message
        );
    }
}

/// Root may execute a file any execute bit allows, so the
/// executable-only-by-others cases cannot be told apart there.
fn skip_as_root(test: &str) -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let root = unsafe { libc::geteuid() } == 0;
    if root {
        println!("skipping {test}: running as root, where X_OK ignores the owner's bits");
    }
    root
}

#[test]
fn a_hook_executable_only_by_others_is_off_and_unchanged() {
    if skip_as_root("a_hook_executable_only_by_others_is_off_and_unchanged") {
        return;
    }
    let f = Fixture::new();
    let hook = f.hooks().join(HOOK);
    std::fs::write(&hook, f.recorder("foreign", 0)).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o641)).unwrap();
    f.amend();
    assert!(!f.ran("foreign"), "git does not run it either");
    let before = tree(&f.hooks());
    assert_eq!(f.slot().state, SlotState::Disabled(Disabled::NotExecutable));
    assert_eq!(
        install(&f.top).unwrap(),
        HookChange::Left(Left::Off(Disabled::NotExecutable))
    );
    assert_eq!(tree(&f.hooks()), before);
    assert_eq!(f.doctor().status, CheckStatus::Warn);
}

#[test]
fn a_symlink_to_a_hook_executable_only_by_others_is_off() {
    if skip_as_root("a_symlink_to_a_hook_executable_only_by_others_is_off") {
        return;
    }
    let f = Fixture::new();
    let target = f.root().join("dotfiles/post-rewrite");
    write_exec(&target, "#!/bin/sh\necho mine\n");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o641)).unwrap();
    std::os::unix::fs::symlink(&target, f.hooks().join(HOOK)).unwrap();
    assert_eq!(wiring(&f.slot()), Wiring::Off(Disabled::NotExecutable));
}

#[test]
fn a_clank_hook_outside_the_effective_dir_is_not_wired() {
    let f = Fixture::new();
    write_exec(&f.hooks().join(HOOK), POST_REWRITE_BODY);
    f.git(&["config", "core.hooksPath", ".githooks"]);
    assert_eq!(f.slot().state, SlotState::Missing);
    assert_eq!(wiring(&f.slot()), Wiring::Unwired(Recipe::RunInit));
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Warn);
    assert!(
        doctor
            .message
            .contains(&f.top.join(".githooks").join(HOOK).display().to_string()),
        "{}",
        doctor.message
    );
}

#[test]
fn a_symlinked_hook_is_left_alone_and_gets_the_stanza_recipe() {
    let f = Fixture::new();
    let target = f.root().join("dotfiles/post-rewrite");
    write_exec(&target, "#!/bin/sh\necho mine\n");
    std::os::unix::fs::symlink(&target, f.hooks().join(HOOK)).unwrap();
    let before = (tree(&f.hooks()), tree(target.parent().unwrap()));
    assert_eq!(install(&f.top).unwrap(), HookChange::Left(Left::Linked));
    assert_eq!((tree(&f.hooks()), tree(target.parent().unwrap())), before);
    assert_eq!(
        f.slot().state,
        SlotState::Linked {
            target: target.clone()
        }
    );
    assert_eq!(
        wiring(&f.slot()),
        Wiring::Unwired(Recipe::Shell(target.clone()))
    );
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Warn);
    assert!(doctor.message.contains(SHELL_STANZA), "{}", doctor.message);
    assert!(doctor.message.contains(&target.display().to_string()));
}

/// Install leaves a managed hook byte-identical and doctor gives `recipe`.
fn assert_managed(f: &Fixture, manager: Manager, recipe: Recipe) {
    let before = tree(&f.hooks());
    assert_eq!(f.slot().state, SlotState::Managed(manager));
    assert_eq!(
        install(&f.top).unwrap(),
        HookChange::Left(Left::Managed(manager))
    );
    assert_eq!(tree(&f.hooks()), before);
    assert_eq!(wiring(&f.slot()), Wiring::Unwired(recipe));
    assert_eq!(f.doctor().status, CheckStatus::Warn);
}

#[test]
fn a_husky_hook_is_managed() {
    let f = Fixture::new();
    write_exec(
        &f.hooks().join(HOOK),
        "#!/bin/sh\n. \"$(dirname \"$0\")/husky.sh\"\nnpx --no -- husky-run post-rewrite\n",
    );
    assert_managed(&f, Manager::Husky, Recipe::Shell(f.hooks().join(HOOK)));
}

#[test]
fn a_lefthook_hook_is_managed() {
    let f = Fixture::new();
    write_exec(
        &f.hooks().join(HOOK),
        "#!/bin/sh\nif [ \"$LEFTHOOK\" = \"0\" ]; then exit 0; fi\ncall_lefthook run post-rewrite \"$@\"\n",
    );
    assert_managed(&f, Manager::Lefthook, Recipe::Lefthook(None));
    assert!(f.doctor().message.contains(LEFTHOOK_RECIPE));
}

#[test]
fn a_pre_commit_hook_is_managed_with_no_recipe() {
    let f = Fixture::new();
    write_exec(
        &f.hooks().join(HOOK),
        "#!/usr/bin/env bash\n# File generated by pre-commit: https://pre-commit.com\nexec pre-commit hook-impl --hook-type=post-rewrite -- \"$@\"\n",
    );
    assert_managed(&f, Manager::PreCommit, Recipe::PreCommit);
    assert!(f.doctor().message.contains("no supported way"));
}

#[test]
fn a_name_dispatching_hook_is_managed() {
    let f = Fixture::new();
    write_exec(
        &f.hooks().join(HOOK),
        "#!/bin/sh\nexec \"$HOME/hooks.d/$(basename \"$0\")\" \"$@\"\n",
    );
    assert_managed(
        &f,
        Manager::NameDispatch,
        Recipe::Shell(f.hooks().join(HOOK)),
    );
}

#[test]
fn locating_a_sibling_with_dirname_is_not_name_dispatch() {
    assert!(dispatches_on_name("exec /x/$(basename \"$0\")\n"));
    assert!(dispatches_on_name("case \"$0\" in *rewrite) ;; esac\n"));
    assert!(dispatches_on_name("exec /x/${0##*/}\n"));
    assert!(dispatches_on_name("exec \"${0%.sh}.helper\" \"$@\"\n"));
    assert!(dispatches_on_name("exec \"$0.helper\"\n"));
    assert!(dispatches_on_name("d=$(dirname \"$0x\")\n"));
    assert!(!dispatches_on_name("d=$(dirname -- \"$0\")\n"));
    assert!(!dispatches_on_name("d=`dirname $0`\n"));
    assert!(!dispatches_on_name(". \"$(dirname \"$0\")/lib.sh\"\n"));
    assert!(!dispatches_on_name("here=${0%/*}\n"));
    assert!(!dispatches_on_name("# uses $0\necho hi\n"));
}

#[test]
fn a_hook_deriving_a_helper_from_its_name_is_managed_and_keeps_running() {
    let f = Fixture::new();
    write_exec(
        &f.hooks().join(HOOK),
        "#!/bin/sh\nexec \"${0%.sh}.helper\" \"$@\"\n",
    );
    write_exec(
        &f.hooks().join(format!("{HOOK}.helper")),
        &f.recorder("helper", 0),
    );
    let stream = f.amend();
    assert_eq!(f.stdin_of("helper"), stream, "the helper runs before init");
    std::fs::remove_file(f.rec.join("helper.stdin")).unwrap();
    let before = tree(&f.hooks());
    assert_eq!(
        install(&f.top).unwrap(),
        HookChange::Left(Left::Managed(Manager::NameDispatch))
    );
    assert_eq!(tree(&f.hooks()), before);
    let stream = f.amend();
    assert_eq!(f.stdin_of("helper"), stream, "and still runs after it");
}

/// Install refuses, naming the chained file, and every entry in the
/// hooks dir keeps its bytes, mode and link target.
fn assert_refused_unchanged(f: &Fixture) {
    let lock = f.hooks().join(LOCK);
    let entries = || -> BTreeMap<_, _> {
        tree(&f.hooks())
            .into_iter()
            .filter(|(p, _)| *p != lock)
            .collect()
    };
    let before = entries();
    let err = format!("{:#}", install(&f.top).unwrap_err());
    assert!(
        err.contains(&f.hooks().join(CHAINED).display().to_string()),
        "{err}"
    );
    assert_eq!(entries(), before);
}

#[test]
fn a_chained_symlink_back_to_the_hook_is_refused_with_nothing_changed() {
    let f = Fixture::new();
    write_exec(&f.hooks().join(HOOK), "#!/bin/sh\necho mine\n");
    std::os::unix::fs::symlink(HOOK, f.hooks().join(CHAINED)).unwrap();
    assert_refused_unchanged(&f);
}

#[test]
fn a_non_executable_identical_chained_file_is_refused_with_nothing_changed() {
    let f = Fixture::new();
    let body = "#!/bin/sh\necho mine\n";
    write_exec(&f.hooks().join(HOOK), body);
    let chained = f.hooks().join(CHAINED);
    std::fs::write(&chained, body).unwrap();
    std::fs::set_permissions(&chained, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_refused_unchanged(&f);
}

#[test]
fn a_chained_backup_executable_only_by_others_is_refused_before_any_replacement() {
    if skip_as_root("a_chained_backup_executable_only_by_others_is_refused_before_any_replacement")
    {
        return;
    }
    let f = Fixture::new();
    let body = "#!/bin/sh\necho mine\n";
    write_exec(&f.hooks().join(HOOK), body);
    let chained = f.hooks().join(CHAINED);
    std::fs::write(&chained, body).unwrap();
    std::fs::set_permissions(&chained, std::fs::Permissions::from_mode(0o641)).unwrap();
    assert_refused_unchanged(&f);
}

#[test]
fn a_wrapper_whose_chained_file_is_unusable_is_refused_and_reported() {
    let f = Fixture::new();
    chain_foreign(&f, 0);
    let chained = f.hooks().join(CHAINED);
    std::fs::remove_file(&chained).unwrap();
    std::os::unix::fs::symlink(HOOK, &chained).unwrap();
    assert_refused_unchanged(&f);
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Warn);
    assert!(
        doctor.message.contains("is a symlink"),
        "{}",
        doctor.message
    );
}

#[test]
fn an_unrelated_chained_file_is_refused_with_nothing_changed() {
    let f = Fixture::new();
    write_exec(&f.hooks().join(HOOK), "#!/bin/sh\necho mine\n");
    write_exec(&f.hooks().join(CHAINED), "#!/bin/sh\necho something else\n");
    let before = tree(&f.hooks());
    let err = format!("{:#}", install(&f.top).unwrap_err());
    assert!(
        err.contains(&f.hooks().join(HOOK).display().to_string()),
        "{err}"
    );
    assert!(
        err.contains(&f.hooks().join(CHAINED).display().to_string()),
        "{err}"
    );
    let after = tree(&f.hooks());
    let lock = f.hooks().join(LOCK);
    assert_eq!(
        after
            .into_iter()
            .filter(|(p, _)| *p != lock)
            .collect::<BTreeMap<_, _>>(),
        before
    );
}

#[test]
fn an_interrupted_chain_reuses_an_identical_chained_file() {
    let f = Fixture::new();
    let body = f.recorder("foreign", 0);
    write_exec(&f.hooks().join(HOOK), &body);
    write_exec(&f.hooks().join(CHAINED), &body);
    assert_eq!(install(&f.top).unwrap(), HookChange::Chained);
    assert_eq!(
        std::fs::read_to_string(f.hooks().join(HOOK)).unwrap(),
        WRAPPER_BODY
    );
    assert_eq!(
        std::fs::read_to_string(f.hooks().join(CHAINED)).unwrap(),
        body
    );
}

#[test]
fn chaining_keeps_the_foreign_hooks_mode() {
    let f = Fixture::new();
    let hook = f.hooks().join(HOOK);
    std::fs::write(&hook, "#!/bin/sh\necho mine\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(install(&f.top).unwrap(), HookChange::Chained);
    let mode = std::fs::metadata(f.hooks().join(CHAINED))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o700);
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Ok, "{}", doctor.message);
}

#[test]
fn a_second_install_is_a_no_op() {
    let fresh = Fixture::new();
    assert_eq!(install(&fresh.top).unwrap(), HookChange::Wrote);
    let chained = Fixture::new();
    chain_foreign(&chained, 0);
    for f in [&fresh, &chained] {
        let before = tree(&f.hooks());
        assert_eq!(install(&f.top).unwrap(), HookChange::UpToDate);
        assert_eq!(tree(&f.hooks()), before);
        assert_eq!(wiring(&f.slot()), Wiring::Wired);
        assert_eq!(f.doctor().status, CheckStatus::Ok);
    }
}

#[test]
fn an_older_clank_hook_is_refreshed() {
    let f = Fixture::new();
    write_exec(
        &f.hooks().join(HOOK),
        "#!/bin/sh\n# clank rewire hook\nclank rewire --from-stdin\n",
    );
    assert_eq!(f.slot().state, SlotState::Clank { current: false });
    assert_eq!(f.doctor().status, CheckStatus::Warn);
    assert_eq!(install(&f.top).unwrap(), HookChange::Refreshed);
    assert_eq!(
        std::fs::read_to_string(f.hooks().join(HOOK)).unwrap(),
        POST_REWRITE_BODY
    );
}

/// Insert the documented stanza at the top of `script`, below a shebang.
fn apply_stanza(script: &Path) {
    let body = std::fs::read_to_string(script).unwrap();
    let with = match body.split_once('\n') {
        Some((shebang, rest)) if shebang.starts_with("#!") => {
            format!("{shebang}\n{SHELL_STANZA}{rest}")
        }
        _ => format!("{SHELL_STANZA}{body}"),
    };
    std::fs::write(script, with).unwrap();
}

/// The owner's files start unwired; with the stanza applied a real amend
/// reaches rewire and the owner's own consumer with one stream, doctor
/// calls it delegated, and init leaves every owner file as it was.
fn assert_delegation(f: &Fixture, script: &Path, owner_dirs: &[PathBuf]) {
    assert_eq!(
        wiring(&f.slot()),
        Wiring::Unwired(Recipe::Shell(script.to_path_buf()))
    );
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Warn);
    assert!(doctor.message.contains(SHELL_STANZA), "{}", doctor.message);
    assert!(doctor.message.contains(&script.display().to_string()));

    apply_stanza(script);
    f.shim(0);
    let stream = f.amend();
    assert_eq!(f.stdin_of("clank"), stream);
    assert_eq!(f.stdin_of("owner"), stream);
    assert_parses_as_rewire(&f.argv_of("clank"));

    assert_eq!(wiring(&f.slot()), Wiring::Delegated(script.to_path_buf()));
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Ok, "{}", doctor.message);
    assert!(doctor.message.contains("delegated"), "{}", doctor.message);

    let snapshot = || owner_dirs.iter().map(|d| tree(d)).collect::<Vec<_>>();
    let before = snapshot();
    crate::cli::init::setup_repo_support(&f.top).unwrap();
    assert_eq!(snapshot(), before);
}

#[test]
fn a_husky_hook_with_the_stanza_is_delegated_and_its_consumer_keeps_the_stream() {
    let f = Fixture::new();
    let husky = f.top.join(".husky");
    write_exec(
        &husky.join("_/h"),
        "#!/bin/sh\nn=$(basename \"$0\")\ns=$(dirname \"$(dirname \"$0\")\")/$n\n[ ! -f \"$s\" ] && exit 0\nsh -e \"$s\" \"$@\"\n",
    );
    write_exec(
        &husky.join("_").join(HOOK),
        "#!/bin/sh\n. \"$(dirname \"$0\")/h\"\n",
    );
    let script = husky.join(HOOK);
    std::fs::write(
        &script,
        format!("cat > '{}/owner.stdin'\n", f.rec.display()),
    )
    .unwrap();
    f.git(&["config", "core.hooksPath", ".husky/_"]);
    assert_eq!(f.slot().state, SlotState::Managed(Manager::Husky));
    assert_delegation(&f, &script, &[husky]);
}

#[test]
fn a_symlinked_hook_with_the_stanza_is_delegated_and_its_consumer_keeps_the_stream() {
    let f = Fixture::new();
    let dotfiles = f.root().join("dotfiles");
    let script = dotfiles.join(HOOK);
    write_exec(&script, &f.recorder("owner", 0));
    std::os::unix::fs::symlink(&script, f.hooks().join(HOOK)).unwrap();
    assert_delegation(&f, &script, &[dotfiles, f.hooks()]);
}

fn lefthook_fixture(config: &str) -> Fixture {
    let f = Fixture::new();
    write_exec(
        &f.hooks().join(HOOK),
        "#!/bin/sh\ncall_lefthook run post-rewrite \"$@\"\n",
    );
    std::fs::write(f.top.join("lefthook.yml"), config).unwrap();
    f
}

#[test]
fn lefthook_with_rewire_as_the_sole_stdin_consumer_is_delegated() {
    let config = format!(
        "pre-commit:\n  commands:\n    lint:\n      run: make lint\n      use_stdin: true\n{LEFTHOOK_RECIPE}"
    );
    let f = lefthook_fixture(&config);
    let owned = || {
        (
            tree(&f.hooks()),
            std::fs::read(f.top.join("lefthook.yml")).unwrap(),
        )
    };
    let before = owned();
    assert_eq!(
        wiring(&f.slot()),
        Wiring::Delegated(f.top.join("lefthook.yml"))
    );
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Ok, "{}", doctor.message);
    crate::cli::init::setup_repo_support(&f.top).unwrap();
    assert_eq!(owned(), before);
}

#[test]
fn lefthook_with_a_second_stdin_consumer_is_unwired_and_names_it() {
    let config = format!(
        "{LEFTHOOK_RECIPE}    notify:\n      run: ./scripts/notify-rewrite\n      use_stdin: true\n"
    );
    let f = lefthook_fixture(&config);
    assert_eq!(
        wiring(&f.slot()),
        Wiring::Unwired(Recipe::LefthookContended {
            config: f.top.join("lefthook.yml"),
            competitors: vec!["notify".to_string()],
        })
    );
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Warn);
    assert!(doctor.message.contains("`notify`"), "{}", doctor.message);
    assert!(doctor.message.contains(SHELL_STANZA), "{}", doctor.message);
}

#[test]
fn lefthook_without_rewire_gets_the_yaml_recipe() {
    let f = lefthook_fixture("post-rewrite:\n  commands:\n    other:\n      run: echo hi\n");
    assert_eq!(
        wiring(&f.slot()),
        Wiring::Unwired(Recipe::Lefthook(Some(f.top.join("lefthook.yml"))))
    );
    assert!(f.doctor().message.contains(LEFTHOOK_RECIPE));
}

#[test]
fn lefthook_stdin_consumers_are_read_from_maps_and_lists_of_post_rewrite_only() {
    let text = "\
post-rewrite:
  jobs:
    - name: sync
      run: ./sync
      use_stdin: true
    - run: \"clank rewire --from-stdin\"
      use_stdin: yes # the stream
  commands:
    plain:
      run: echo
pre-push:
  commands:
    elsewhere:
      run: x
      use_stdin: true
";
    assert_eq!(
        lefthook_stdin_consumers(text),
        Ok(vec![
            StdinConsumer {
                name: "sync".into(),
                runs_rewire: false
            },
            StdinConsumer {
                name: "an unnamed job".into(),
                runs_rewire: true
            },
        ])
    );
}

/// The lefthook fixture with `config`, whose consumer set doctor must
/// call unknown rather than delegated.
fn assert_lefthook_undetermined(f: &Fixture, config: &Path) {
    let Wiring::Unwired(Recipe::LefthookUndetermined { config: named, why }) = wiring(&f.slot())
    else {
        panic!("not undetermined: {:?}", wiring(&f.slot()));
    };
    assert_eq!(named, config);
    let doctor = f.doctor();
    assert_eq!(doctor.status, CheckStatus::Warn);
    assert!(
        doctor.message.contains("could not establish") && doctor.message.contains(&why),
        "{}",
        doctor.message
    );
}

#[test]
fn a_jobs_first_key_use_stdin_on_a_competing_job_stays_contended() {
    let f = lefthook_fixture(
        "post-rewrite:
  jobs:
    - use_stdin: true
      name: notify
      run: ./notify
    - name: clank-rewire
      run: clank rewire --from-stdin
      use_stdin: true
",
    );
    assert_eq!(
        wiring(&f.slot()),
        Wiring::Unwired(Recipe::LefthookContended {
            config: f.top.join("lefthook.yml"),
            competitors: vec!["notify".to_string()],
        })
    );
    assert!(f.doctor().message.contains("`notify`"));
}

#[test]
fn a_jobs_first_key_use_stdin_on_the_rewire_job_is_delegated() {
    let f = lefthook_fixture(
        "post-rewrite:
  jobs:
    - use_stdin: true
      name: clank-rewire
      run: clank rewire --from-stdin
    - name: lint
      run: ./lint
",
    );
    assert_eq!(
        wiring(&f.slot()),
        Wiring::Delegated(f.top.join("lefthook.yml"))
    );
}

#[test]
fn lefthook_with_a_flow_style_sibling_is_undetermined() {
    let f = lefthook_fixture(&format!(
        "{LEFTHOOK_RECIPE}    notify: {{run: ./notify, use_stdin: true}}\n"
    ));
    assert_lefthook_undetermined(&f, &f.top.join("lefthook.yml"));
}

#[test]
fn lefthook_that_extends_another_config_is_undetermined() {
    let f = lefthook_fixture(&format!(
        "extends:\n  - shared/lefthook.yml\n{LEFTHOOK_RECIPE}"
    ));
    assert_lefthook_undetermined(&f, &f.top.join("lefthook.yml"));
}

#[test]
fn lefthook_with_a_local_override_is_undetermined() {
    let f = lefthook_fixture(LEFTHOOK_RECIPE);
    let local = f.top.join("lefthook-local.yml");
    std::fs::write(
        &local,
        "post-rewrite:\n  commands:\n    notify:\n      run: ./notify\n      use_stdin: true\n",
    )
    .unwrap();
    assert_lefthook_undetermined(&f, &local);
}

#[test]
fn lefthook_syntax_the_reader_does_not_follow_is_an_error_not_an_empty_set() {
    for text in [
        "post-rewrite:\n  commands:\n    a: &shared\n      run: x\n",
        "post-rewrite:\n  commands:\n    b: *shared\n",
        "post-rewrite:\n  commands:\n    n:\n      run: ./notify\n      use_stdin: *on\n",
        "post-rewrite:\n  commands:\n    n:\n      run: [./notify]\n",
        "post-rewrite:\n  commands:\n    c:\n      <<: *shared\n",
        "post-rewrite:\n  commands:\n    d:\n      run: |\n        ./notify\n",
        "post-rewrite:\n  commands:\n    e:\n      group:\n        x: y\n",
        "post-rewrite: {commands: {}}\n",
        "post-rewrite:\n  unknown: 1\n",
        "post-rewrite:\n  jobs:\n    - use_stdin: false\n      use_stdin: true\n",
        "post-rewrite:\n\tcommands:\n",
        "post-rewrite:\n  commands: {}\npost-rewrite:\n  commands: {}\n",
    ] {
        assert!(lefthook_stdin_consumers(text).is_err(), "{text}");
    }
    assert_eq!(
        lefthook_stdin_consumers("pre-commit:\n  parallel: true\n"),
        Ok(vec![])
    );
}

#[test]
fn a_foreign_hook_that_already_delegates_is_not_chained() {
    let f = Fixture::new();
    let hook = f.hooks().join(HOOK);
    write_exec(&hook, &format!("#!/bin/sh\n{SHELL_STANZA}cat >/dev/null\n"));
    let before = tree(&f.hooks());
    assert_eq!(install(&f.top).unwrap(), HookChange::Left(Left::Delegated));
    assert_eq!(tree(&f.hooks()), before);
    assert_eq!(wiring(&f.slot()), Wiring::Delegated(hook));
}
