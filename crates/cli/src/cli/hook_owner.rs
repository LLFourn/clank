//! The interactive codex client a parked Stop hook belongs to, so the
//! hook, and the wait it owns, end when that client does.
//!
//! Only a standalone client is an owner: it spawned the hook, so it is
//! the hook's parent. Under the codex app-server daemon the parent
//! outlives every session, and nothing available to the hook ties a
//! client process to its thread, so that hook stays `Unknown` and
//! behaves as before rather than ending on a guess.

use crate::proc_identity::ProcToken;
use crate::proc_identity::exit_watch::{Armed, ExitWatch, Wake};
use std::ffi::OsString;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Owner {
    Process {
        pid: i32,
        birth: ProcToken,
    },
    /// The process that spawned the hook has already exited.
    Gone,
    Unknown,
}

/// The argv of a recorded process, or why there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Argv {
    Read(Vec<OsString>),
    /// The process born at the recorded birth has exited or been replaced.
    Ended,
    Unreadable,
}

/// What resolution reads about this process's parent.
pub(crate) trait Parentage {
    fn parent(&self) -> i32;
    fn token(&self, pid: i32) -> Option<ProcToken>;
    /// The argv of `pid`, only while it is still the process born at `birth`.
    fn argv(&self, pid: i32, birth: &ProcToken) -> Argv;
}

pub(crate) struct Native;

impl Parentage for Native {
    fn parent(&self) -> i32 {
        // SAFETY: getppid has no preconditions and cannot fail.
        unsafe { libc::getppid() }
    }
    fn token(&self, pid: i32) -> Option<ProcToken> {
        crate::proc_identity::token_for(pid)
    }
    fn argv(&self, pid: i32, birth: &ProcToken) -> Argv {
        use crate::proc_identity::inspect::{Inspection, ProcessState};
        match crate::proc_identity::inspect::inspect(pid, birth) {
            Inspection::Verified(details)
                if matches!(details.state, Ok(ProcessState::Zombie | ProcessState::Dead)) =>
            {
                Argv::Ended
            }
            Inspection::Verified(details) => details.argv.map_or(Argv::Unreadable, Argv::Read),
            Inspection::Mismatched | Inspection::Ended => Argv::Ended,
            Inspection::Unavailable(_) => Argv::Unreadable,
        }
    }
}

/// Resolved once, before the wait is spawned.
///
/// The parent's birth is read between two `getppid` calls. A parent
/// that died during the read would have left us reparented, so an
/// unchanged parent means the token is our parent's and not that of a
/// stranger now holding its pid.
pub(crate) fn resolve(src: &impl Parentage) -> Owner {
    let parent = src.parent();
    if parent == 1 {
        return Owner::Gone;
    }
    let birth = src.token(parent);
    if src.parent() != parent {
        return Owner::Gone;
    }
    let Some(birth) = birth else {
        return Owner::Unknown;
    };
    match src.argv(parent, &birth) {
        Argv::Read(argv) if is_standalone_codex(&argv) => Owner::Process { pid: parent, birth },
        // The birth was taken while it was our parent, so its end is
        // the owner's.
        Argv::Ended => Owner::Gone,
        Argv::Read(_) | Argv::Unreadable => Owner::Unknown,
    }
}

/// The hook's lifetime for `tool`. Codex only: a claude hook's parent
/// is the claude client, which ends its own hooks, and none leaked.
pub(crate) fn lifetime_for(tool: clank_core::Tool, src: &impl Parentage) -> Lifetime {
    match tool {
        clank_core::Tool::Codex => Lifetime::of(resolve(src)),
        _ => Lifetime::Unbound,
    }
}

fn is_standalone_codex(argv: &[OsString]) -> bool {
    let program = argv
        .first()
        .and_then(|arg| std::path::Path::new(arg).file_name());
    program == Some(std::ffi::OsStr::new("codex")) && !argv.iter().any(|arg| arg == "app-server")
}

/// How long the hook may live: until its owner exits, or unbounded.
pub(crate) enum Lifetime {
    Gone,
    Bound {
        _watch: ExitWatch<()>,
        exited: tokio::sync::oneshot::Receiver<()>,
    },
    Unbound,
}

impl Lifetime {
    /// Watches `owner`. An owner that ends before the watch is armed is
    /// `Gone`; one that cannot be watched is `Unbound`.
    pub(crate) fn of(owner: Owner) -> Self {
        let (pid, birth) = match owner {
            Owner::Process { pid, birth } => (pid, birth),
            Owner::Gone => return Lifetime::Gone,
            Owner::Unknown => return Lifetime::Unbound,
        };
        let (tx, exited) = tokio::sync::oneshot::channel();
        let mut tx = Some(tx);
        let Ok(watch) = ExitWatch::new(None, move |wake| {
            if let (Wake::Exited(()), Some(tx)) = (wake, tx.take()) {
                let _ = tx.send(());
            }
        }) else {
            return Lifetime::Unbound;
        };
        match watch.arm((), pid, &birth) {
            Armed::Watching => Lifetime::Bound {
                _watch: watch,
                exited,
            },
            Armed::Ended => Lifetime::Gone,
            Armed::Unknown => Lifetime::Unbound,
        }
    }

    /// A watched lifetime whose owner's exit the test signals.
    #[cfg(test)]
    pub(crate) fn signalled() -> (Self, tokio::sync::oneshot::Sender<()>) {
        let (tx, exited) = tokio::sync::oneshot::channel();
        let watch = ExitWatch::new(None, |_| {}).unwrap();
        (
            Lifetime::Bound {
                _watch: watch,
                exited,
            },
            tx,
        )
    }

    /// Whether the owner is known to have exited, without waiting.
    pub(crate) fn gone_now(&mut self) -> bool {
        let gone = match self {
            Lifetime::Gone => true,
            Lifetime::Bound { exited, .. } => exited.try_recv().is_ok(),
            Lifetime::Unbound => false,
        };
        if gone {
            *self = Lifetime::Gone;
        }
        gone
    }

    /// Resolves when the owner exits; never, while it lives or when
    /// there is none to watch.
    pub(crate) async fn ended(&mut self) {
        let delivered = match self {
            Lifetime::Gone => true,
            Lifetime::Bound { exited, .. } => exited.await.is_ok(),
            Lifetime::Unbound => false,
        };
        if delivered {
            *self = Lifetime::Gone;
        } else {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Script {
        parents: RefCell<Vec<i32>>,
        token: Option<ProcToken>,
        argv: Option<Vec<&'static str>>,
    }

    impl Parentage for Script {
        fn parent(&self) -> i32 {
            self.parents.borrow_mut().remove(0)
        }
        fn token(&self, _pid: i32) -> Option<ProcToken> {
            self.token.clone()
        }
        fn argv(&self, _pid: i32, _birth: &ProcToken) -> Argv {
            match &self.argv {
                Some(a) => Argv::Read(a.iter().map(OsString::from).collect()),
                None => Argv::Unreadable,
            }
        }
    }

    fn birth() -> ProcToken {
        ProcToken::MacStart { sec: 1, usec: 2 }
    }

    fn script(parents: &[i32], argv: Option<Vec<&'static str>>) -> Script {
        Script {
            parents: RefCell::new(parents.to_vec()),
            token: Some(birth()),
            argv,
        }
    }

    #[test]
    fn a_hook_already_reparented_at_start_has_lost_its_owner() {
        assert_eq!(resolve(&script(&[1], None)), Owner::Gone);
    }

    #[test]
    fn a_parent_that_changes_during_the_identity_read_is_gone() {
        let s = script(&[40, 1], Some(vec!["/usr/local/bin/codex"]));
        assert_eq!(resolve(&s), Owner::Gone);
    }

    #[test]
    fn a_verified_standalone_codex_parent_is_the_owner() {
        let s = script(
            &[40, 40],
            Some(vec!["/Users/u/.local/bin/codex", "resume", "t"]),
        );
        assert_eq!(
            resolve(&s),
            Owner::Process {
                pid: 40,
                birth: birth()
            }
        );
    }

    #[test]
    fn anything_but_a_standalone_codex_parent_is_unknown() {
        let daemon = vec![
            "/Users/u/.codex/bin/codex",
            "app-server",
            "--listen",
            "unix://",
        ];
        for argv in [Some(daemon), Some(vec!["/bin/zsh"]), None] {
            assert_eq!(
                resolve(&script(&[40, 40], argv.clone())),
                Owner::Unknown,
                "{argv:?}"
            );
        }
        let mut unreadable = script(&[40, 40], Some(vec!["codex"]));
        unreadable.token = None;
        assert_eq!(resolve(&unreadable), Owner::Unknown);
    }

    #[test]
    fn only_a_codex_hook_takes_its_owner() {
        assert!(matches!(
            lifetime_for(clank_core::Tool::Codex, &script(&[1], None)),
            Lifetime::Gone
        ));
        for tool in [clank_core::Tool::Claude, clank_core::Tool::Grok] {
            assert!(matches!(
                lifetime_for(tool, &script(&[1], None)),
                Lifetime::Unbound
            ));
        }
    }

    fn child() -> (std::process::Child, ProcToken) {
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let birth = crate::proc_identity::token_for(child.id() as i32).unwrap();
        (child, birth)
    }

    /// The parent is captured, then dies before its argv is read.
    struct DiesBeforeArgv {
        pid: i32,
        birth: ProcToken,
        child: RefCell<Option<std::process::Child>>,
    }

    impl Parentage for DiesBeforeArgv {
        fn parent(&self) -> i32 {
            self.pid
        }
        fn token(&self, _pid: i32) -> Option<ProcToken> {
            Some(self.birth.clone())
        }
        fn argv(&self, pid: i32, birth: &ProcToken) -> Argv {
            if let Some(mut child) = self.child.borrow_mut().take() {
                child.kill().unwrap();
                child.wait().unwrap();
            }
            Native.argv(pid, birth)
        }
    }

    #[test]
    fn an_owner_dying_during_its_argv_read_is_gone() {
        let (child, birth) = child();
        let parent = DiesBeforeArgv {
            pid: child.id() as i32,
            birth,
            child: RefCell::new(Some(child)),
        };
        assert_eq!(resolve(&parent), Owner::Gone);
    }

    #[tokio::test]
    async fn a_watched_owner_ends_the_lifetime_when_it_exits() {
        let (mut child, birth) = child();
        let mut life = Lifetime::of(Owner::Process {
            pid: child.id() as i32,
            birth,
        });
        assert!(matches!(life, Lifetime::Bound { .. }));
        assert!(!life.gone_now());
        child.kill().unwrap();
        child.wait().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), life.ended())
            .await
            .expect("the owner's exit ends the lifetime");
        assert!(life.gone_now());
    }

    #[tokio::test]
    async fn an_owner_that_ended_before_the_watch_is_gone_and_unknown_never_ends() {
        let (mut child, birth) = child();
        let pid = child.id() as i32;
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(matches!(
            Lifetime::of(Owner::Process { pid, birth }),
            Lifetime::Gone
        ));
        let mut unbound = Lifetime::of(Owner::Unknown);
        assert!(!unbound.gone_now());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), unbound.ended())
                .await
                .is_err()
        );
    }
}
