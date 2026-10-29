//! `clank open`: the team workspace in zellij.

use super::{OpenArgs, OpenCmd, OpenZellijArgs};

pub async fn run(args: OpenArgs) -> anyhow::Result<()> {
    match args.command {
        Some(OpenCmd::Zellij(zellij)) => {
            super::open_zellij::run(zellij_args(args.zellij, zellij)?).await
        }
        // Bare `clank open` and `clank open zellij` are the same
        // thing. The subcommand survives as an alias so nothing that
        // invokes it breaks (zellij-is-the-workspace).
        None => super::open_zellij::run(args.zellij).await,
    }
}

/// The opener's options from both sides of `zellij`: `clank open --repo R
/// zellij` and `clank open zellij --repo R` are the same request. An
/// option given on both sides must agree, and the one-of `--fork` /
/// `--pr` / `--all` still holds across them.
pub(crate) fn zellij_args(
    before: OpenZellijArgs,
    after: OpenZellijArgs,
) -> anyhow::Result<OpenZellijArgs> {
    fn one<T: PartialEq + std::fmt::Debug>(
        name: &str,
        before: Option<T>,
        after: Option<T>,
    ) -> anyhow::Result<Option<T>> {
        match (before, after) {
            (Some(b), Some(a)) if b != a => {
                anyhow::bail!("`--{name}` given twice with different values: {b:?} and {a:?}")
            }
            (b, a) => Ok(a.or(b)),
        }
    }
    let merged = OpenZellijArgs {
        repo: one("repo", before.repo, after.repo)?,
        fork: one("fork", before.fork, after.fork)?,
        pr: one("pr", before.pr, after.pr)?,
        all: before.all || after.all,
        print: before.print || after.print,
    };
    let targets = [merged.fork.is_some(), merged.pr.is_some(), merged.all];
    if targets.iter().filter(|t| **t).count() > 1 {
        anyhow::bail!("`--fork`, `--pr` and `--all` choose one target; give one");
    }
    Ok(merged)
}

#[cfg(test)]
mod open_args_tests {
    use super::super::{OpenArgs, OpenCmd};
    use super::zellij_args;
    use clap::Parser;

    /// Minimal root so the OpenArgs clap shape is testable
    /// in-process (the real root lives in main.rs).
    #[derive(Parser, Debug)]
    struct T {
        #[command(subcommand)]
        cmd: C,
    }
    #[derive(clap::Subcommand, Debug)]
    enum C {
        Open(OpenArgs),
    }

    fn parse(argv: &[&str]) -> OpenArgs {
        match T::parse_from(argv).cmd {
            C::Open(o) => o,
        }
    }

    #[test]
    fn bare_open_accepts_the_flattened_zellij_flags() {
        // The zellij flags work on the bare `clank open` form via
        // the flattened (shared, drift-proof) OpenZellijArgs.
        let o = parse(&["t", "open"]);
        assert!(o.command.is_none());
        assert!(!o.zellij.print);

        let o = parse(&["t", "open", "--print", "--repo", "/r"]);
        assert!(o.command.is_none());
        assert!(o.zellij.print);
        assert_eq!(o.zellij.repo.as_deref(), Some(std::path::Path::new("/r")));
    }

    /// codex 458bb8e: `clank open --print zellij` once parsed with the
    /// parent `--print` silently dropped, because dispatch read only the
    /// subcommand's args. The options on either side of `zellij` are now
    /// one request, and nothing given is dropped.
    #[test]
    fn options_before_zellij_are_kept_not_dropped() {
        let merged = |argv: &[&str]| {
            let o = parse(argv);
            let Some(OpenCmd::Zellij(after)) = o.command else {
                panic!("expected zellij")
            };
            zellij_args(o.zellij, after)
        };
        let r = Some(std::path::Path::new("/r"));
        let m = merged(&["t", "open", "--repo", "/r", "zellij"]).unwrap();
        assert_eq!(m.repo.as_deref(), r);
        let m = merged(&["t", "open", "zellij", "--repo", "/r"]).unwrap();
        assert_eq!(m.repo.as_deref(), r);
        let m = merged(&["t", "open", "--print", "zellij", "--repo", "/r"]).unwrap();
        assert!(m.print && m.repo.as_deref() == r, "both sides count");
        assert!(
            merged(&["t", "open", "--repo", "/a", "zellij", "--repo", "/b"]).is_err(),
            "two different repos are refused, not one silently kept"
        );
        assert!(merged(&["t", "open", "--repo", "/r", "zellij", "--repo", "/r"]).is_ok());
        assert!(
            merged(&["t", "open", "--fork", "a", "zellij", "--pr", "3"]).is_err(),
            "one target across both sides"
        );
    }

    /// The pre-rename form `clank open <path>` errors rather than
    /// mis-parsing (ruthless a25f71d minor).
    #[test]
    fn a_bare_path_is_refused() {
        assert!(T::try_parse_from(["t", "open", "/some/path"]).is_err());
    }

    #[test]
    fn explicit_subcommands_unchanged() {
        let o = parse(&["t", "open", "zellij", "--print"]);
        match o.command {
            Some(OpenCmd::Zellij(z)) => assert!(z.print),
            other => panic!("expected zellij subcommand, got {other:?}"),
        }
    }
}
