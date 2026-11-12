//! Git fixtures, built in-process. Use Git itself only when its command,
//! hooks, or interoperability is the behavior under test.

#![allow(dead_code)]

use std::path::Path;

/// An unborn `main` repository with a local test identity, independent of
/// the developer's init templates, signing preferences, and default branch.
pub fn init(path: &Path) {
    let repo = gix::init(path).expect("initialize fixture repository");
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n").unwrap();
    use std::io::Write;
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(repo.git_dir().join("config"))
        .unwrap();
    config
        .write_all(b"\n[user]\nname = test\nemail = test@test\n[commit]\ngpgsign = false\n")
        .unwrap();
}

/// Commit exactly these worktree paths on top of HEAD, preserving the other
/// tracked paths. Also install its index so the fixture starts clean.
/// This deliberately does not imitate `git add`'s ignore/filter semantics.
pub fn commit_files(path: &Path, paths: &[&str], message: &str) -> String {
    let repo = gix::open(path).unwrap();
    let parent = repo.head_id().ok().map(|id| id.detach());
    let tree = parent
        .map(|id| repo.find_commit(id).unwrap().tree_id().unwrap().detach())
        .unwrap_or_else(|| gix::hash::ObjectId::empty_tree(repo.object_hash()));
    let mut editor = repo.edit_tree(tree).unwrap();
    for name in paths {
        let blob = repo
            .write_blob(std::fs::read(path.join(name)).unwrap())
            .unwrap();
        editor
            .upsert(*name, gix::objs::tree::EntryKind::Blob, blob.detach())
            .unwrap();
    }
    let tree = editor.write().unwrap().detach();
    let signature = gix::actor::Signature {
        name: "test".into(),
        email: "test@test".into(),
        time: gix::date::Time::new(1_700_000_000, 0),
    };
    let mut time = gix::date::parse::TimeBuf::default();
    let signature = signature.to_ref(&mut time);
    let id = repo
        .commit_as(signature, signature, "HEAD", message, tree, parent)
        .unwrap();
    repo.index_from_tree(&tree)
        .unwrap()
        .write(Default::default())
        .unwrap();
    id.to_string()
}
