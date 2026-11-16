//! Gate for `docs/invariants.md` (I13): every test an entry names exists,
//! every plan it names is still to land, and each status carries what it
//! owes. It proves references, not that a test protects its invariant.

use std::path::{Path, PathBuf};

use crate::common::crates_dir;

#[derive(Debug, PartialEq)]
enum Status {
    Holds,
    HoldsUntested,
    Target,
}

#[derive(Debug, Default)]
struct Entry {
    id: String,
    status: Option<Status>,
    tests: Vec<String>,
    plans: Vec<String>,
}

fn repo_root() -> PathBuf {
    crates_dir()
        .parent()
        .expect("crates/ has a parent")
        .to_path_buf()
}

/// Each `## I<n>.` section, with its `- Field:` lines joined to their
/// indented continuations.
fn entries(doc: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = vec![];
    let mut field: Option<(String, String)> = None;
    let close = |field: &mut Option<(String, String)>, out: &mut Vec<Entry>| {
        let (Some((name, value)), Some(entry)) = (field.take(), out.last_mut()) else {
            return;
        };
        match name.as_str() {
            "Status" => {
                entry.status = match value.trim() {
                    "holds" => Some(Status::Holds),
                    "holds untested" => Some(Status::HoldsUntested),
                    "target" => Some(Status::Target),
                    _ => None,
                }
            }
            "Tests" => {
                entry.tests = value
                    .split('`')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_string)
                    .collect()
            }
            "Plans" => {
                entry.plans = value
                    .split([',', ' '])
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            _ => {}
        }
    };
    for line in doc.lines() {
        if let Some(rest) = line.strip_prefix("## I") {
            close(&mut field, &mut out);
            let id = rest.split('.').next().unwrap_or_default();
            out.push(Entry {
                id: format!("I{id}"),
                ..Entry::default()
            });
        } else if let Some(rest) = line.strip_prefix("- ") {
            close(&mut field, &mut out);
            if let Some((name, value)) = rest.split_once(':') {
                field = Some((name.to_string(), value.to_string()));
            }
        } else if line.starts_with(' ') {
            if let Some((_, value)) = field.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else {
            close(&mut field, &mut out);
        }
    }
    close(&mut field, &mut out);
    out
}

fn test_problem(root: &Path, reference: &str) -> Option<String> {
    let Some((path, name)) = reference.rsplit_once("::") else {
        return Some(format!("`{reference}` is not `path::name`"));
    };
    match std::fs::read_to_string(root.join(path)) {
        Ok(source) if source.contains(&format!("fn {name}(")) => None,
        Ok(_) => Some(format!("`{path}` has no test `{name}`")),
        Err(e) => Some(format!("`{path}`: {e}")),
    }
}

/// The queue is the master's private state, so a checkout without one (CI)
/// cannot see queued plans and accepts a plan that is neither active nor
/// finished. Mirroring the queue's names into a versioned file would be a
/// second copy of the queue (I6); the master's own run of this gate, where
/// the registry is edited, is what catches a mistyped plan.
fn plan_problem(root: &Path, plan: &str) -> Option<String> {
    let clank = root.join(".clank");
    if clank.join("finished").join(format!("{plan}.md")).exists() {
        return Some(format!(
            "plan `{plan}` has finished; its entry's status should have changed"
        ));
    }
    if clank.join("plans").join(format!("{plan}.md")).exists() {
        return None;
    }
    let queue = match std::fs::read_dir(clank.join("queue")) {
        Ok(queue) => queue,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("the queue cannot be read: {e}")),
    };
    let queued = queue.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name == format!("{plan}.md") || name.ends_with(&format!("-{plan}.md"))
    });
    (!queued).then(|| format!("plan `{plan}` is neither active nor queued"))
}

fn problems(doc: &str, root: &Path) -> Vec<String> {
    let entries = entries(doc);
    let mut out = vec![];
    if entries.is_empty() {
        out.push("no `## I<n>.` entries".to_string());
    }
    for (n, entry) in entries.iter().enumerate() {
        let id = &entry.id;
        if *id != format!("I{}", n + 1) {
            out.push(format!("{id} is out of sequence; expected I{}", n + 1));
        }
        match entry.status {
            None => out.push(format!(
                "{id}: Status is not holds, holds untested or target"
            )),
            Some(Status::Holds) if entry.tests.is_empty() => {
                out.push(format!("{id}: holds but names no test"))
            }
            Some(Status::HoldsUntested | Status::Target) if entry.plans.is_empty() => {
                out.push(format!("{id}: names no plan to make it hold"))
            }
            _ => {}
        }
        let tests = entry.tests.iter().filter_map(|t| test_problem(root, t));
        let plans = entry.plans.iter().filter_map(|p| plan_problem(root, p));
        out.extend(tests.chain(plans).map(|p| format!("{id}: {p}")));
    }
    out
}

#[test]
fn every_entry_resolves() {
    let root = repo_root();
    let doc = std::fs::read_to_string(root.join("docs/invariants.md")).expect("registry");
    let problems = problems(&doc, &root);
    assert!(problems.is_empty(), "{problems:#?}");
}

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["plans", "queue", "finished"] {
            std::fs::create_dir_all(dir.path().join(".clank").join(sub)).unwrap();
        }
        std::fs::write(dir.path().join("t.rs"), "#[test]\nfn real_test() {}\n").unwrap();
        std::fs::write(dir.path().join(".clank/plans/active.md"), "").unwrap();
        std::fs::write(dir.path().join(".clank/queue/150-queued.md"), "").unwrap();
        std::fs::write(dir.path().join(".clank/finished/landed.md"), "").unwrap();
        Self(dir)
    }

    fn problems(&self, doc: &str) -> Vec<String> {
        problems(doc, self.0.path())
    }
}

#[test]
fn a_named_test_must_exist() {
    let f = Fixture::new();
    let holds = |test: &str| format!("## I1. x\n\n- Status: holds\n- Tests: `{test}`\n");
    assert_eq!(f.problems(&holds("t.rs::real_test")), Vec::<String>::new());
    assert_eq!(
        f.problems(&holds("t.rs::renamed_test")),
        ["I1: `t.rs` has no test `renamed_test`"]
    );
    assert_eq!(f.problems(&holds("gone.rs::real_test")).len(), 1);
}

#[test]
fn a_named_plan_must_be_active_or_queued() {
    let f = Fixture::new();
    let target = |plan: &str| format!("## I1. x\n\n- Status: target\n- Plans: {plan}\n");
    assert_eq!(f.problems(&target("active, queued")), Vec::<String>::new());
    assert_eq!(
        f.problems(&target("invented")),
        ["I1: plan `invented` is neither active nor queued"]
    );
    assert_eq!(
        f.problems(&target("landed")),
        ["I1: plan `landed` has finished; its entry's status should have changed"]
    );
}

#[test]
fn without_a_local_queue_only_finished_plans_are_refused() {
    let f = Fixture::new();
    std::fs::remove_dir_all(f.0.path().join(".clank/queue")).unwrap();
    let target = |plan: &str| format!("## I1. x\n\n- Status: target\n- Plans: {plan}\n");
    assert_eq!(f.problems(&target("invented")), Vec::<String>::new());
    assert_eq!(f.problems(&target("landed")).len(), 1);
}

#[test]
fn an_unreadable_queue_is_a_problem_not_an_absent_one() {
    let f = Fixture::new();
    let queue = f.0.path().join(".clank/queue");
    std::fs::remove_dir_all(&queue).unwrap();
    std::fs::write(&queue, "").unwrap();
    let problems = f.problems("## I1. x\n\n- Status: target\n- Plans: invented\n");
    assert!(
        problems[0].starts_with("I1: the queue cannot be read"),
        "{problems:?}"
    );
}

#[test]
fn each_status_carries_what_it_owes() {
    let f = Fixture::new();
    assert_eq!(
        f.problems("## I1. x\n\n- Status: holds\n"),
        ["I1: holds but names no test"]
    );
    assert_eq!(
        f.problems("## I1. x\n\n- Status: holds untested\n"),
        ["I1: names no plan to make it hold"]
    );
    assert_eq!(
        f.problems("## I1. x\n\n- Status: maybe\n"),
        ["I1: Status is not holds, holds untested or target"]
    );
    assert_eq!(
        f.problems("## I2. x\n\n- Status: target\n- Plans: queued\n"),
        ["I2 is out of sequence; expected I1"]
    );
}

#[test]
fn a_wrapped_field_keeps_its_continuation() {
    let f = Fixture::new();
    let doc = "## I1. x\n\n- Status: holds\n- Tests: `t.rs::real_test`,\n  `t.rs::other_test`\n";
    assert_eq!(f.problems(doc), ["I1: `t.rs` has no test `other_test`"]);
}
