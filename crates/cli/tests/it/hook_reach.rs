//! Enforcement gate (hooks-read-only-what-they-are-given): a hook acts only
//! on what it is given (I4), held over everything a hook process REACHES.
//!
//! `hook_is_a_messenger` reads the hook modules line by line, so a helper
//! they call could read policy unseen: the `outage_notified` read came back
//! through `load_all_agent_configs` and `wait::blocked` with all its tests
//! passing. This gate walks the production call graph
//! ([`crate::common::call_graph`]) from each hook process's entry, and
//! holds every function it reaches to three rules: the per-line rules of
//! `hook_is_a_messenger`, no call to a policy or decision-state reader
//! ([`DENIED`]), and no file opened in a function that names a
//! decision-state file. What the graph cannot follow fails the gate unless
//! [`UNFOLLOWABLE`] lists it with a reason. Every failure names the path
//! from its root.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::common::call_graph::{Graph, Shape, To, dependencies};
use crate::common::crates_dir;
use crate::hook_is_a_messenger::line_rule;

/// The hook processes' entries: every way the harness, or a hook, starts a
/// clank process that acts for a hook.
const ROOTS: &[&str] = &[
    // Stop, PostCompact, SessionStart and subagent events, and the dispatch
    // to Grok's and recovery's adapters.
    "crate::cli::stop_hook::run",
    // The `clank wait --turn-end` child a Stop carries its report in.
    "crate::cli::turn_child::run",
    "crate::cli::grok_hook::hook",
    "crate::cli::question_hook::run",
    "crate::cli::recovery_hook::run",
    // A parked Stop becoming the installed build: written by the old
    // image, taken by the new one.
    "crate::cli::hook_handoff::Handoff::hand_to",
    "crate::cli::hook_handoff::Handoff::take",
];

/// Readers of policy and of state the monitor owns, and what each reads.
const DENIED: &[(&str, &str)] = &[
    ("crate::agent_store::load_agent_config", "an agent's config"),
    ("crate::agent_store::resolve_role", "a role decision"),
    ("crate::agent_store::repo_config::read", "the repo config"),
    ("crate::cli::watch_store::read", "the repo config"),
    (
        "crate::cli::team::resolve_effective_auto_mode",
        "an auto-mode decision",
    ),
    (
        "crate::cli::team::resolve_effective_auto_mode_with_source",
        "an auto-mode decision",
    ),
    ("crate::cli::wait::blocked", "the repo's blocks"),
    ("crate::cli::block::Blocks::read", "the repo's blocks"),
    ("crate::cli::block::scan_blocks", "the repo's blocks"),
    ("crate::cli::block::ended", "the repo's blocks"),
    ("crate::cli::block::master", "the roster's master"),
    ("crate::cli::question_state::load", "the question state"),
    ("crate::cli::question_state::asking", "the question state"),
    ("crate::cli::recovery::load", "the recovery episode"),
    ("crate::cli::ledger::load", "the delivery ledger"),
    (
        "crate::cli::turn_custody::load_stored",
        "the monitor's turn custody",
    ),
    (
        "crate::cli::agent_state::load",
        "the monitor's published state",
    ),
    (
        "crate::cli::agent_state::read",
        "the monitor's published state",
    ),
    (
        "crate::cli::agent_state::attended_now",
        "the attendance projection",
    ),
    ("crate::cli::nudge_log::read", "the nudge history"),
    ("clank_user_config::read", "the user config"),
    ("clank_user_config::versioned::read", "a config document"),
    (
        "clank_user_config::versioned::read_stored",
        "a config document",
    ),
];

/// What one function may itself do that the rules otherwise refuse. The
/// permission is the function's own: nothing it calls inherits it, and the
/// walk still holds everything it reaches to every rule.
#[derive(Clone, Copy)]
struct Exception {
    at: &'static str,
    /// Denied readers it may call directly, and per-line needles its own
    /// lines may contain.
    may: &'static [&'static str],
    /// For a document reader: the only keys it may consume, by literal or
    /// by a deserialized struct's fields.
    keys: &'static [&'static str],
    /// For a document reader: the only types it may deserialize into.
    targets: &'static [&'static str],
    why: &'static str,
}

/// The evidence the hook audit allows. Today only the binding a hook routes
/// by needs one: the transcript, process identity and upgrade state are
/// read by nothing the rules refuse. A function excepted for a document
/// read is audited against its projection: it may index the document only
/// with its listed keys, deserialize it only into its listed types, whose
/// fields must be listed keys, and may not walk its entries, hand it to
/// another function or return it.
const EXCEPTIONS: &[Exception] = &[
    Exception {
        at: "crate::agent_store::identity::tool",
        may: &["clank_user_config::versioned::read"],
        keys: &["team", "promoted", "agents", "role", "tool"],
        targets: &["Tool"],
        why: "bootstrap reads only the roster's declared tool, never role or launch policy",
    },
    Exception {
        at: "crate::agent_store::binding_in",
        may: &["clank_user_config::versioned::read_stored"],
        keys: &["binding"],
        targets: &["Binding"],
        why: "the session an agent's state binds, parsed as identity alone",
    },
    Exception {
        at: "crate::agent_store::identity::mutate",
        may: &["clank_user_config::versioned::read_stored"],
        keys: &["binding", "resume_pending", "bound"],
        targets: &["Identity"],
        why: "rewrites only the binding's own fields, and on a rebind drops the old session's \
              `bound` record unparsed; every other part is preserved unparsed",
    },
    Exception {
        at: "crate::agent_store::state::path",
        may: &["state.json"],
        keys: &[],
        targets: &[],
        why: "the binding lives in the agent's state document",
    },
    Exception {
        at: "crate::agent_store::agents_root",
        may: &["agents_root("],
        keys: &[],
        targets: &[],
        why: "where every agent's binding lives",
    },
    Exception {
        at: "crate::agent_store::identity::binding",
        may: &["agents_root("],
        keys: &[],
        targets: &[],
        why: "the binding lives in the agent's directory",
    },
    Exception {
        at: "crate::agent_store::iter_agent_dirs",
        may: &["agents_root("],
        keys: &[],
        targets: &[],
        why: "a session clank did not launch is found by every agent's binding",
    },
    Exception {
        at: "crate::agent_store::StateGuard::acquire",
        may: &["agents_root("],
        keys: &[],
        targets: &[],
        why: "the binding's lock is the agent's directory",
    },
];

/// One site the graph cannot follow, reviewed: in function `at`, the
/// refusal of `form` whose description names `subject`. The graph still
/// follows every call it can see there.
struct Unfollowable {
    at: &'static str,
    form: &'static str,
    subject: &'static str,
    why: &'static str,
}

const UNFOLLOWABLE: &[Unfollowable] = &[
    Unfollowable {
        at: "crate::cli::question_hook::answer",
        form: "json!",
        subject: "json!",
        why: "the empty answer, when the broker fails",
    },
    Unfollowable {
        at: "crate::cli::recovery_hook::await_recovery",
        form: "json!",
        subject: "json!",
        why: "frames the dispatch the monitor sent",
    },
    Unfollowable {
        at: "crate::cli::recovery_hook::run",
        form: "json!",
        subject: "json!",
        why: "frames what the monitor heard",
    },
    Unfollowable {
        at: "crate::cli::monitor::connect_within",
        form: "offset_of!",
        subject: "offset_of!",
        why: "a field offset of `sockaddr_un`, a constant",
    },
    Unfollowable {
        at: "crate::cli::stop_hook::OwnedWait::verdict_until",
        form: "pin!",
        subject: "pin!",
        why: "pins the abandon future in place",
    },
    Unfollowable {
        at: "crate::cli::stop_hook::await_verdict_under_deadline",
        form: "pin!",
        subject: "pin!",
        why: "pins the verdict future in place",
    },
    Unfollowable {
        at: "crate::cli::turn_child::carry",
        form: "debug!",
        subject: "debug!",
        why: "logs why no monitor holds the report",
    },
    Unfollowable {
        at: "crate::cli::turn_child::emit_final",
        form: "debug!",
        subject: "debug!",
        why: "logs that the hook was gone",
    },
    Unfollowable {
        at: "crate::cli::turn_child::forward_result",
        form: "debug!",
        subject: "debug!",
        why: "logs that the result could not be forwarded",
    },
    Unfollowable {
        at: "crate::upgrade::Replacing::pass",
        form: "debug!",
        subject: "debug!",
        why: "logs why the replacement failed",
    },
    Unfollowable {
        at: "crate::build_stamp::framed",
        form: "unresolved",
        subject: "`STAMP`",
        why: "a byte constant the build script generates, included from OUT_DIR",
    },
    Unfollowable {
        at: "crate::build_stamp::framed",
        form: "unresolved",
        subject: "`VERSION`",
        why: "a string constant the build script generates, included from OUT_DIR",
    },
    Unfollowable {
        at: "crate::cli::source_group::retire_natively",
        form: "use as",
        subject: "line 22",
        why: "`cs` is `command_source`, which no per-line rule names",
    },
    Unfollowable {
        at: "crate::cli::recovery_hook::observation",
        form: "json!",
        subject: "json!",
        why: "builds the observed line from the hook's own arguments and the clock",
    },
    Unfollowable {
        at: "crate::cli::stop_hook::OwnedWait::verdict_until",
        form: "select!",
        subject: "select!",
        why: "races the child's next frame against the park being abandoned",
    },
    Unfollowable {
        at: "crate::cli::stop_hook::await_verdict_under_deadline",
        form: "select!",
        subject: "select!",
        why: "races the verdict against becoming the installed build",
    },
    Unfollowable {
        at: "crate::cli::turn_child::carry",
        form: "select!",
        subject: "select!",
        why: "races attaching, and the pause between attempts, against the hook going",
    },
    Unfollowable {
        at: "crate::cli::turn_child::shared",
        form: "select!",
        subject: "select!",
        why: "races the hook's pipe against the monitor's notes",
    },
    Unfollowable {
        at: "crate::upgrade::Replacing::pass_until",
        form: "select!",
        subject: "select!",
        why: "races the retry pause against a wake",
    },
    Unfollowable {
        at: "crate::upgrade::watch_install",
        form: "warn!",
        subject: "warn!",
        why: "logs why the install could not be watched",
    },
];

/// Calls that open a file.
const OPENS: &[&str] = &[
    "fs::read",
    "fs::read_to_string",
    "fs::write",
    "fs::File::open",
    "fs::File::create",
    "fs::File::create_new",
    "fs::OpenOptions::new",
    "File::open",
    "File::create",
    "File::create_new",
    "OpenOptions::new",
];

/// Method calls that open or replace a file.
const OPENING_METHODS: &[&str] = &["persist", "persist_noclobber", "open"];

/// Whether a string literal names a decision-state file.
fn decision_state_file(lit: &str) -> bool {
    let name = lit.rsplit('/').next().unwrap_or(lit);
    (name.starts_with("recovery") && name.ends_with(".json"))
        || matches!(
            name,
            "question.json"
                | "question.lock"
                | "turn.json"
                | "state.json"
                | "ledger.json"
                | "blocks.lock"
                | "nudges.jsonl"
                | "nudge-pending.json"
                | "turns.jsonl"
                | "repeat.json"
                | "repeat.lock"
        )
        || lit.contains("blocks/")
}

/// Why `n`, excepted for a document read by `e`, consumes more of the
/// document than its audited projection.
fn beyond_projection(g: &Graph, n: usize, e: &Exception) -> Vec<(usize, String)> {
    let p = g.projection(n);
    let mut out = Vec::new();
    for (line, key) in &p.keys {
        if !e.keys.contains(&key.as_str()) {
            out.push((
                *line,
                format!("consumes `{key}`, which its projection does not list"),
            ));
        }
    }
    for (line, target) in &p.targets {
        match target {
            None => out.push((
                *line,
                "deserializes into a type its source does not name".into(),
            )),
            Some((name, _)) if !e.targets.contains(&name.as_str()) => out.push((
                *line,
                format!("deserializes into `{name}`, which its projection does not list"),
            )),
            Some((name, Some(Shape::Opaque(why)))) => out.push((
                *line,
                format!("deserializes into `{name}`, whose fields are not its names: {why}"),
            )),
            Some((name, Some(Shape::Fields(fields)))) => {
                for f in fields {
                    if !e.keys.contains(&f.as_str()) {
                        out.push((
                            *line,
                            format!(
                                "deserializes `{name}.{f}`, which its projection does not list"
                            ),
                        ));
                    }
                }
            }
            Some((_, None)) => {}
        }
    }
    for line in &p.iterations {
        out.push((*line, "walks entries it does not name".into()));
    }
    for (line, callee) in &p.hands_on {
        out.push((*line, format!("hands a whole document to {callee}")));
    }
    if p.returns_document {
        out.push((g.nodes[n].lines.0, "returns a whole document".into()));
    }
    out
}

/// The violations reached from `roots` in `g`, each naming its path.
fn violations(g: &Graph, roots: &[&str], exceptions: &[Exception]) -> Vec<String> {
    let denied: HashMap<&str, &str> = DENIED.iter().copied().collect();
    let mut via: HashMap<usize, Option<usize>> = HashMap::new();
    let mut queue = VecDeque::new();
    let mut out = BTreeMap::new();
    for r in roots {
        for &n in g.find(r).unwrap_or_default() {
            via.insert(n, None);
            queue.push_back(n);
        }
    }
    let chain = |via: &HashMap<usize, Option<usize>>, mut n: usize| {
        let mut out = vec![g.nodes[n].id.clone()];
        while let Some(Some(p)) = via.get(&n) {
            out.push(g.nodes[*p].id.clone());
            n = *p;
        }
        out.reverse();
        out.join("\n      -> ")
    };
    let names_state = |n: usize| {
        g.nodes[n]
            .literals
            .iter()
            .find(|(_, l)| decision_state_file(l))
            .cloned()
    };
    while let Some(n) = queue.pop_front() {
        let node = &g.nodes[n];
        let exception = exceptions.iter().find(|e| e.at == node.id);
        let may = |what: &str| exception.is_some_and(|e| e.may.contains(&what));
        let mut report = |what: String, line: usize, via: &HashMap<usize, Option<usize>>| {
            let f = &node.file;
            let at = format!(
                "{}:{line}",
                f.strip_prefix(crates_dir()).unwrap_or(f).display()
            );
            out.entry(format!("{} {what} ({at})", node.id))
                .or_insert_with(|| chain(via, n));
        };
        if let Some(e) = exception
            && e.may.iter().any(|m| denied.contains_key(m))
        {
            for (line, why) in beyond_projection(g, n, e) {
                report(why, line, &via);
            }
        }
        for (line, text) in g.lines(n) {
            if let Some((needle, what)) = line_rule(text)
                && !may(needle)
            {
                report(format!("touches {what}"), line, &via);
            }
        }
        let (edges, refusals) = g.edges(n);
        for r in refusals {
            let reviewed = UNFOLLOWABLE
                .iter()
                .any(|u| u.at == node.id && u.form == r.form && r.what.contains(u.subject));
            if !reviewed {
                report(format!("cannot be followed: {}", r.what), r.line, &via);
            }
        }
        let mut opens = None;
        let mut state_path = None;
        for e in &edges {
            let (callee, internal) = match &e.to {
                To::Node(t) => (g.nodes[*t].id.as_str(), Some(*t)),
                To::External(p) => (p.as_str(), None),
                To::Method(m) => {
                    if OPENING_METHODS.contains(&m.as_str()) {
                        opens.get_or_insert((e.line, format!(".{m}()")));
                    }
                    continue;
                }
            };
            if internal.is_none() && !e.value && OPENS.iter().any(|o| callee.ends_with(o)) {
                opens.get_or_insert((e.line, callee.to_string()));
            }
            let reader = denied
                .iter()
                .find(|(d, _)| **d == callee || (internal.is_none() && callee.ends_with(**d)));
            if let Some((d, what)) = reader
                && !may(d)
            {
                report(format!("calls {d}, which reads {what}"), e.line, &via);
                continue;
            }
            if let Some(t) = internal {
                if g.returns_path(t) && names_state(t).is_some() {
                    state_path.get_or_insert((e.line, g.nodes[t].id.clone()));
                }
                if let std::collections::hash_map::Entry::Vacant(v) = via.entry(t) {
                    v.insert(Some(n));
                    queue.push_back(t);
                }
            }
        }
        if let Some((line, how)) = opens {
            if let Some((_, lit)) = names_state(n) {
                report(format!("opens a file ({how}) and names {lit}"), line, &via);
            } else if let Some((_, path)) = state_path {
                report(
                    format!("opens a file ({how}) at a decision-state path from {path}"),
                    line,
                    &via,
                );
            }
        }
    }
    out.into_iter()
        .map(|(what, path)| format!("{what}\n    path: {path}"))
        .collect()
}

fn production() -> Graph {
    let cli = crates_dir().join("cli");
    Graph::load(&cli.join("src"), dependencies(&cli.join("Cargo.toml"))).expect("the crate parses")
}

#[test]
fn every_root_reader_and_exception_names_a_function() {
    let g = production();
    let missing: Vec<&str> = ROOTS
        .iter()
        .copied()
        .chain(
            DENIED
                .iter()
                .map(|(d, _)| *d)
                .filter(|d| d.starts_with("crate::")),
        )
        .chain(EXCEPTIONS.iter().map(|e| e.at))
        .chain(UNFOLLOWABLE.iter().map(|u| u.at))
        .filter(|id| g.find(id).is_none_or(<[usize]>::is_empty))
        .collect();
    assert!(
        missing.is_empty(),
        "the gate names functions the crate does not have; a renamed root guards \
         nothing: {missing:?}"
    );
    for e in EXCEPTIONS {
        assert!(!e.why.is_empty() && !e.may.is_empty(), "{} says why", e.at);
    }
    for u in UNFOLLOWABLE {
        assert!(
            !u.why.is_empty() && !u.subject.is_empty(),
            "{} says why",
            u.at
        );
    }
}

#[test]
fn nothing_a_hook_reaches_reads_policy_or_decision_state() {
    let v = violations(&production(), ROOTS, EXCEPTIONS);
    assert!(
        v.is_empty(),
        "a hook process reaches policy or decision state ({} site(s)). Report the \
         evidence to the monitor and frame its verdict instead:\n  {}",
        v.len(),
        v.join("\n  ")
    );
}

#[test]
fn every_exception_and_reviewed_site_is_needed() {
    let g = production();
    for (i, e) in EXCEPTIONS.iter().enumerate() {
        let without: Vec<Exception> = EXCEPTIONS
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, e)| *e)
            .collect();
        let v = violations(&g, ROOTS, &without);
        assert!(
            v.iter().any(|v| v.contains(e.at)),
            "{} excepts nothing the walk reaches",
            e.at
        );
    }
    for u in UNFOLLOWABLE {
        let n = g.find(u.at).unwrap()[0];
        let (_, refusals) = g.edges(n);
        assert!(
            refusals
                .iter()
                .any(|r| r.form == u.form && r.what.contains(u.subject)),
            "{} has no {} site to review",
            u.at,
            u.form
        );
    }
}

/// A crate of `files` under a temporary `src/`, depending on
/// `clank_user_config`, `serde`, `serde_json` and `anyhow`.
fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, Graph) {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let externs = ["clank_user_config", "serde", "serde_json", "anyhow"]
        .map(String::from)
        .into();
    let g = Graph::load(dir.path(), externs).unwrap();
    (dir, g)
}

const STORE: &str = "pub mod agent_store { pub fn load_agent_config() {} }\n";

fn caught(lib: &str) -> Vec<String> {
    let (_dir, g) = fixture(&[("lib.rs", &format!("{STORE}{lib}"))]);
    violations(&g, &["crate::run"], &[])
}

#[test]
fn a_reader_is_caught_at_any_depth_and_named_by_its_path() {
    let v = caught(
        "pub fn run() { a() }\nfn a() { b() }\nfn b() { crate::agent_store::load_agent_config(); }\n",
    );
    assert!(
        v.iter().any(
            |v| v.contains("calls crate::agent_store::load_agent_config")
                && v.contains("path: crate::run\n      -> crate::a\n      -> crate::b")
        ),
        "{v:?}"
    );
}

#[test]
fn a_reader_named_as_a_callback_is_reached() {
    let v = caught(
        "pub fn run() { call(crate::agent_store::load_agent_config) }\nfn call(f: fn()) { f() }\n",
    );
    assert!(v.iter().any(|v| v.contains("load_agent_config")), "{v:?}");
}

#[test]
fn a_method_reaches_its_receivers_type_and_not_a_namesake() {
    let types = "pub struct A;\nimpl A { pub fn go(&self) { crate::agent_store::load_agent_config() } }\n\
                 pub struct B;\nimpl B { pub fn go(&self) {} }\n";
    assert!(caught(&format!("{types}pub fn run(b: &B) {{ b.go() }}\n")).is_empty());
    assert!(!caught(&format!("{types}pub fn run(a: &A) {{ a.go() }}\n")).is_empty());
    assert!(
        !caught(&format!(
            "{types}pub fn run(all: Vec<A>) {{ all.iter().for_each(|x| x.go()) }}\n"
        ))
        .is_empty(),
        "an untyped receiver reaches the methods of every type in scope"
    );
}

#[test]
fn a_formatting_macro_is_parsed_and_any_other_is_refused() {
    let v = caught(
        "pub fn run() { let _ = format!(\"{:?}\", crate::agent_store::load_agent_config()); }\n",
    );
    assert!(
        v.iter()
            .any(|v| v.contains("calls crate::agent_store::load_agent_config")),
        "{v:?}"
    );
    assert!(!v.iter().any(|v| v.contains("cannot be followed")), "{v:?}");
    let v = caught("pub fn run() { other!(crate::agent_store::load_agent_config()); }\n");
    assert!(
        v.iter()
            .any(|v| v.contains("cannot be followed: `other!`, whose expansion")),
        "{v:?}"
    );
    assert!(
        v.iter()
            .any(|v| v.contains("calls crate::agent_store::load_agent_config")),
        "a refused macro's calls are still followed: {v:?}"
    );
}

#[test]
fn a_macro_is_refused_whatever_its_arguments_say() {
    let v = caught(
        "macro_rules! policy { () => { crate::agent_store::load_agent_config() }; }
         pub fn run() { policy!(); }
",
    );
    assert!(
        v.iter()
            .any(|v| v.contains("`policy!`, a macro this crate defines")),
        "a reader hidden in a macro's definition passed: {v:?}"
    );
    let v = caught(
        "pub fn run() { opaque!(); }
",
    );
    assert!(
        v.iter().any(|v| v.contains("`opaque!`, whose expansion")),
        "an external macro with no call in its arguments passed: {v:?}"
    );
    let v = caught(
        "macro_rules! format { () => { crate::agent_store::load_agent_config() }; }
         pub fn run() { let _ = format!(); }
",
    );
    assert!(
        v.iter()
            .any(|v| v.contains("`format!`, a macro this crate defines")),
        "a crate macro shadowing a parsed one passed: {v:?}"
    );
}

#[test]
fn a_renamed_path_is_refused() {
    let v = caught("use crate::agent_store as s;\npub fn run() { s::load_agent_config() }\n");
    assert!(v.iter().any(|v| v.contains("renaming `use`")), "{v:?}");
}

#[test]
fn an_exception_is_its_functions_own() {
    let lib = "pub fn run() { binding() }\n\
               fn binding() { clank_user_config::versioned::read(); below() }\n\
               fn below() { clank_user_config::versioned::read() }\n";
    let (_dir, g) = fixture(&[("lib.rs", lib)]);
    let excepted = [Exception {
        at: "crate::binding",
        may: &["clank_user_config::versioned::read"],
        keys: &[],
        targets: &[],
        why: "fixture",
    }];
    let v = violations(&g, &["crate::run"], &excepted);
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(
        v[0].starts_with("crate::below calls clank_user_config::versioned::read")
            && v[0].contains("path: crate::run\n      -> crate::binding\n      -> crate::below"),
        "a helper below an exception inherits nothing: {v:?}"
    );
}

/// A binding reader in the shape of `agent_store::identity::tool`, held to
/// that exception's projection.
fn tool_reader(body: &str, helpers: &str) -> Vec<String> {
    let lib = format!(
        "pub mod agent_store {{ pub mod identity {{\n\
         use clank_user_config::versioned::{{self, Document}};\n\
         pub fn tool(path: &std::path::Path, label: &str) -> anyhow::Result<Tool> {{\n{body}\n}}\n\
         {helpers}\n}} }}\n\
         pub fn run() {{ let _ = agent_store::identity::tool(std::path::Path::new(\"\"), \"\"); }}\n"
    );
    let (_dir, g) = fixture(&[("lib.rs", &lib)]);
    violations(&g, &["crate::run"], EXCEPTIONS)
}

const TOOL_READ: &str = "let doc = versioned::read(path, 1)?.context(\"no roster configured\")?;\n\
    anyhow::ensure!(!doc.contains_key(\"team\"), \"legacy roster configuration\");\n\
    let entry = doc.get(\"agents\").and_then(|a| a.get(label)).context(\"not in the roster\")?;\n\
    serde_json::from_value(entry.get(\"tool\").context(\"no tool\")?.clone()).map_err(Into::into)";

#[test]
fn the_binding_projection_itself_is_allowed() {
    let v = tool_reader(TOOL_READ, "");
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn a_policy_key_read_through_an_allowed_reader_is_caught() {
    let v = tool_reader(
        &format!("let _ = versioned::read(path, 1)?.unwrap().get(\"event_watches\");\n{TOOL_READ}"),
        "",
    );
    assert!(
        v.iter().any(|v| v.contains("consumes `event_watches`")),
        "{v:?}"
    );
    let v = tool_reader(
        &format!(
            "let _: Everything = serde_json::from_value(serde_json::Value::Null)?;\n{TOOL_READ}"
        ),
        "",
    );
    assert!(
        v.iter()
            .any(|v| v.contains("deserializes into `Everything`")),
        "{v:?}"
    );
}

#[test]
fn a_policy_read_below_an_allowed_reader_is_caught() {
    let v = tool_reader(
        &format!("watches(path);\n{TOOL_READ}"),
        "fn watches(path: &std::path::Path) { let _ = versioned::read(path, 1); }",
    );
    assert!(
        v.iter().any(|v| v.starts_with(
            "crate::agent_store::identity::watches calls clank_user_config::versioned::read"
        )),
        "{v:?}"
    );
    let v = tool_reader(
        &format!("let all = versioned::read(path, 1)?.unwrap();\nwatches(&all);\n{TOOL_READ}"),
        "fn watches(doc: &Document) { let _ = doc.get(\"event_watches\"); }",
    );
    assert!(
        v.iter()
            .any(|v| v.contains("hands a whole document to crate::agent_store::identity::watches")),
        "{v:?}"
    );
}

#[test]
fn a_binding_struct_that_grows_a_policy_field_is_caught() {
    let lib = |fields: &str| {
        format!(
            "pub mod agent_store {{\n\
             pub fn binding_in(path: &std::path::Path) -> anyhow::Result<Option<u8>> {{\n\
             #[derive(serde::Deserialize)] struct Binding {{ {fields} }}\n\
             let Some(stored) = clank_user_config::versioned::read_stored(path, 1)? else {{ return Ok(None); }};\n\
             let binding: Binding = serde_json::from_value(serde_json::Value::Object(stored.doc))?;\n\
             Ok(binding.binding)\n}} }}\n\
             pub fn run() {{ let _ = agent_store::binding_in(std::path::Path::new(\"\")); }}\n"
        )
    };
    let (_dir, g) = fixture(&[("lib.rs", &lib("binding: Option<u8>"))]);
    let v = violations(&g, &["crate::run"], EXCEPTIONS);
    assert!(v.is_empty(), "{v:?}");
    let (_dir, g) = fixture(&[("lib.rs", &lib("binding: Option<u8>, bound: Option<u8>"))]);
    let v = violations(&g, &["crate::run"], EXCEPTIONS);
    assert!(
        v.iter().any(|v| v.contains("deserializes `Binding.bound`")),
        "{v:?}"
    );
}

#[test]
fn opening_a_decision_state_file_is_caught_directly_or_through_its_path() {
    let direct = caught(
        "pub fn run(d: &std::path::Path) { let _ = std::fs::read(d.join(\"recovery.json\")); }\n",
    );
    assert!(
        direct.iter().any(|v| v.contains("opens a file")),
        "{direct:?}"
    );
    let through = caught(
        "fn path(d: &std::path::Path) -> std::path::PathBuf { d.join(\"question.json\") }\n\
         pub fn run(d: &std::path::Path) { let _ = std::fs::write(path(d), \"\"); }\n",
    );
    assert!(
        through
            .iter()
            .any(|v| v.contains("at a decision-state path from crate::path")),
        "{through:?}"
    );
}

#[test]
fn a_whole_config_read_through_the_binding_exception_is_caught() {
    let src = "pub mod agent_store { pub mod identity {\n\
               pub fn tool() {\n\
               let doc = clank_user_config::versioned::read(std::path::Path::new(\"/repo/.clank/config.json\"), 1);\n\
               let _ = doc.unwrap().unwrap().get(\"event_watches\");\n\
               }\n}}\n\
               pub fn run() { agent_store::identity::tool(); }\n";
    let (_dir, g) = fixture(&[("lib.rs", src)]);
    let v = violations(&g, &["crate::run"], EXCEPTIONS);
    assert!(
        v.iter().any(|v| v.contains("consumes `event_watches`")),
        "{v:?}"
    );
}
