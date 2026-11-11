//! Enforcement gate (work-only-when-something-happens): PRODUCTION code
//! constructs a platform event stream in one place, the status and TUI
//! worktree watch in `cli/status.rs`, whose roots are trees of any size.
//!
//! On macOS that stream is FSEvents: each one costs seconds to set up, and
//! every stream on the machine loads one shared daemon, which slowed the
//! file operations of every process while the suite ran. A watch of a few
//! known directories and files goes through `proc_identity::tree_watch`,
//! the kernel's own registrations, instead.

use crate::common::{Production, crates_dir, rs_files};

const LAYER: &str = "cli/src/cli/status.rs";

/// What makes a backend that is not clank's kernel watch.
const STREAMS: &[&str] = &[
    "recommended_watcher",
    "RecommendedWatcher",
    "FsEventWatcher",
    "INotifyWatcher",
    "KqueueWatcher",
    "PollWatcher",
];

#[test]
fn only_the_worktree_watch_opens_a_platform_event_stream() {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates_dir())
        .expect("read crates/")
        .flatten()
    {
        let src = entry.path().join("src");
        if src.is_dir() {
            rs_files(&src, &mut files);
        }
    }
    assert!(!files.is_empty(), "scanned no source files");
    let production = Production::of(&files);

    let mut violations = Vec::new();
    let mut in_layer = 0;
    for f in &files {
        let layer = f.to_string_lossy().replace('\\', "/").ends_with(LAYER);
        for (i, line) in production.text(f).lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for stream in STREAMS {
                if code.contains(stream) {
                    if layer {
                        in_layer += 1;
                    } else {
                        violations.push(format!("{}:{} — `{stream}`", f.display(), i + 1));
                    }
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "a platform event stream outside the worktree watch ({} site(s)); \
         watch known directories and files with \
         `proc_identity::tree_watch::KernelWatcher`:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
    assert!(
        in_layer > 0,
        "the worktree watch no longer names its stream"
    );
}
