//! The README's harness table claims only what `docs/harness-support.toml`
//! backs (first-run-walk-per-harness). A version is tested when its latest
//! walk passed and the latest entry for every one of its harness's required
//! native acceptance tests passed; a harness whose required list is
//! `unavailable` has no tested version. The record's floors are read back
//! from the source, its test identities resolve to real `#[ignore]`d tests,
//! and `scripts/first-run/env.sh` really leaves no way back to a production
//! profile or session.

use crate::common::{Production, crates_dir, is_cfg_test, rs_files};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::visit::Visit;

const UNAVAILABLE: &str = "unavailable";
const NOT_YET_TESTED: &str = "not yet tested";
const NO_FLOOR: &str = "none";
const TABLE_HEADER: &str = "| harness | floor in source | tested |";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    harness: Vec<Harness>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Harness {
    name: String,
    source_floor: SourceFloor,
    #[serde(default)]
    walks: Vec<Walk>,
    required_acceptance: Required,
    #[serde(default)]
    acceptance: Vec<Acceptance>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Required {
    Tests(Vec<String>),
    Marker(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFloor {
    version: String,
    readme: String,
    file: Option<String>,
    line: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Walk {
    version: String,
    date: String,
    outcome: Outcome,
    rough_edges: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acceptance {
    test: String,
    version: String,
    date: String,
    outcome: Outcome,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
enum Outcome {
    Passed,
    Failed,
    NotRun,
}

impl Harness {
    /// The required test identities, or `None` when the list is
    /// `unavailable`.
    fn required(&self) -> Option<&[String]> {
        match &self.required_acceptance {
            Required::Tests(tests) => Some(tests),
            Required::Marker(_) => None,
        }
    }

    /// The `env.sh` argument and the word its source names the tool by.
    fn tool(&self) -> String {
        self.name
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_lowercase()
    }

    fn recorded_versions(&self) -> BTreeSet<&str> {
        self.walks
            .iter()
            .map(|w| w.version.as_str())
            .chain(self.acceptance.iter().map(|a| a.version.as_str()))
            .collect()
    }

    /// Why `version` is not tested, or `None` when it is.
    fn untested_because(&self, version: &str) -> Option<String> {
        let Some(required) = self.required() else {
            return Some(format!("its required acceptance is `{UNAVAILABLE}`"));
        };
        match latest(
            self.walks
                .iter()
                .filter(|w| w.version == version)
                .map(|w| (w.date.as_str(), w.outcome)),
        ) {
            None => return Some("it has no walk".into()),
            Some(Outcome::Passed) => {}
            Some(other) => return Some(format!("its latest walk is {other:?}")),
        }
        for test in required {
            match latest(
                self.acceptance
                    .iter()
                    .filter(|a| a.version == version && &a.test == test)
                    .map(|a| (a.date.as_str(), a.outcome)),
            ) {
                None => return Some(format!("it has no acceptance entry for {test}")),
                Some(Outcome::Passed) => {}
                Some(other) => return Some(format!("its latest {test} is {other:?}")),
            }
        }
        None
    }
}

/// The outcome on the latest date. Two entries on that date settle on the
/// worse, so a same-day failure cannot hide behind a pass.
fn latest<'a>(entries: impl Iterator<Item = (&'a str, Outcome)>) -> Option<Outcome> {
    let entries: Vec<_> = entries.collect();
    let last = entries.iter().map(|(date, _)| *date).max()?;
    let on_last = entries.iter().filter(|(date, _)| *date == last);
    Some(
        on_last
            .map(|(_, outcome)| *outcome)
            .find(|o| *o != Outcome::Passed)
            .unwrap_or(Outcome::Passed),
    )
}

struct Row {
    harness: String,
    floor: String,
    tested: Vec<String>,
}

fn readme_table(readme: &str) -> Result<Vec<Row>, String> {
    let mut lines = readme
        .lines()
        .skip_while(|l| l.trim() != TABLE_HEADER)
        .skip(1);
    if lines.next().map(str::trim) != Some("|---|---|---|") {
        return Err(format!("the README has no `{TABLE_HEADER}` table"));
    }
    let mut rows = Vec::new();
    for line in lines.take_while(|l| l.starts_with('|')) {
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        let [harness, floor, tested] = cells[..] else {
            return Err(format!("not a three-cell row: {line}"));
        };
        let tested = if tested == NOT_YET_TESTED {
            Vec::new()
        } else {
            tested.split(", ").map(str::to_string).collect()
        };
        rows.push(Row {
            harness: harness.into(),
            floor: floor.into(),
            tested,
        });
    }
    Ok(rows)
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

fn is_plan_stem(s: &str) -> bool {
    !s.is_empty()
        && s.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// Every way the record and the README table break the rules, without
/// reading the source.
fn violations(record_toml: &str, readme: &str) -> Vec<String> {
    let record: Record = match toml::from_str(record_toml) {
        Ok(r) => r,
        Err(e) => return vec![format!("the record does not parse: {e}")],
    };
    let rows = match readme_table(readme) {
        Ok(rows) => rows,
        Err(e) => return vec![e],
    };
    let mut out = Vec::new();
    let mut by_name = BTreeMap::new();
    for h in &record.harness {
        if by_name.insert(h.name.as_str(), h).is_some() {
            out.push(format!("{}: recorded twice", h.name));
        }
        out.extend(
            record_violations(h)
                .into_iter()
                .map(|v| format!("{}: {v}", h.name)),
        );
    }
    let mut seen = BTreeSet::new();
    for row in &rows {
        if !seen.insert(row.harness.as_str()) {
            out.push(format!("{}: two README rows", row.harness));
        }
        let Some(h) = by_name.get(row.harness.as_str()) else {
            out.push(format!(
                "{}: a README row missing from the record",
                row.harness
            ));
            continue;
        };
        if row.floor != h.source_floor.readme {
            out.push(format!(
                "{}: the README floor `{}` is not the source floor `{}`",
                h.name, row.floor, h.source_floor.readme
            ));
        }
        let recorded = h.recorded_versions();
        for version in &row.tested {
            if !recorded.contains(version.as_str()) {
                out.push(format!(
                    "{}: README version `{version}` matches no recorded version",
                    h.name
                ));
            } else if let Some(why) = h.untested_because(version) {
                out.push(format!(
                    "{}: {version} is claimed tested, but {why}",
                    h.name
                ));
            }
        }
    }
    for name in by_name.keys() {
        if !seen.contains(name) {
            out.push(format!(
                "{name}: recorded but missing from the README table"
            ));
        }
    }
    out
}

fn record_violations(h: &Harness) -> Vec<String> {
    let mut out = Vec::new();
    match &h.required_acceptance {
        Required::Marker(m) if m != UNAVAILABLE => out.push(format!(
            "required_acceptance is `{m}`: a list of tests or `{UNAVAILABLE}`"
        )),
        Required::Tests(tests) if tests.is_empty() => out.push(format!(
            "required_acceptance is empty, which would pass with no evidence; use `{UNAVAILABLE}`"
        )),
        _ => {}
    }
    let floor = &h.source_floor;
    let located = floor.file.is_some() && floor.line.is_some();
    if (floor.version == NO_FLOOR) == located || floor.file.is_some() != floor.line.is_some() {
        out.push(format!(
            "source_floor needs a file and line exactly when its version is not `{NO_FLOOR}`"
        ));
    }
    if floor.version != NO_FLOOR && !floor.readme.contains(&floor.version) {
        out.push(format!(
            "source_floor.readme `{}` does not state its version {}",
            floor.readme, floor.version
        ));
    }
    for w in &h.walks {
        if w.version.is_empty() || !is_date(&w.date) {
            out.push(format!(
                "walk `{}` on `{}` needs a version and a YYYY-MM-DD date",
                w.version, w.date
            ));
        }
        for edge in &w.rough_edges {
            if !is_plan_stem(edge) {
                out.push(format!("rough edge `{edge}` is not a plan stem"));
            }
        }
    }
    for a in &h.acceptance {
        if a.version.is_empty() || !is_date(&a.date) {
            out.push(format!(
                "acceptance `{}` on `{}` needs a version and a YYYY-MM-DD date",
                a.test, a.date
            ));
        }
        if !h.required().is_some_and(|r| r.contains(&a.test)) {
            out.push(format!(
                "acceptance entry `{}` is not a required test",
                a.test
            ));
        }
    }
    out
}

fn workspace() -> PathBuf {
    crates_dir().join("..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn record() -> Record {
    toml::from_str(&read(&workspace().join("docs/harness-support.toml"))).unwrap()
}

#[test]
fn the_readme_claims_only_what_the_record_backs() {
    let errors = violations(
        &read(&workspace().join("docs/harness-support.toml")),
        &read(&workspace().join("README.md")),
    );
    assert!(errors.is_empty(), "{errors:#?}");
}

/// The floor a source line enforces: an `(x, y, z)` version tuple, or a
/// `const …: u64 = N;` major.
fn floor_on(line: &str) -> Option<String> {
    let tuple = line.split('(').skip(1).find_map(|rest| {
        let inner = rest.split(')').next()?;
        let parts: Vec<&str> = inner.split(", ").collect();
        (parts.len() == 3 && parts.iter().all(|p| p.parse::<u64>().is_ok()))
            .then(|| parts.join("."))
    });
    tuple.or_else(|| {
        let (lhs, rhs) = line.trim().strip_suffix(';')?.split_once(" = ")?;
        (lhs.contains("const ") && lhs.ends_with(": u64") && rhs.parse::<u64>().is_ok())
            .then(|| rhs.to_string())
    })
}

/// Whether some production fn that runs `--version` names `tool`.
fn probes_version_of(tool: &str) -> Vec<String> {
    struct Probes<'a> {
        tool: &'a str,
        found: Vec<String>,
    }
    impl Probes<'_> {
        fn check(&mut self, name: &syn::Ident, body: &syn::Block) {
            use quote::ToTokens;
            let text = body.to_token_stream().to_string();
            if text.contains("\"--version\"") && text.to_lowercase().contains(self.tool) {
                self.found.push(name.to_string());
            }
        }
    }
    impl<'ast> Visit<'ast> for Probes<'_> {
        fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
            if !m.attrs.iter().any(is_cfg_test) {
                syn::visit::visit_item_mod(self, m);
            }
        }
        fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
            if f.attrs.iter().any(is_cfg_test) {
                return;
            }
            self.check(&f.sig.ident, &f.block);
            syn::visit::visit_item_fn(self, f);
        }
        fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
            self.check(&f.sig.ident, &f.block);
            syn::visit::visit_impl_item_fn(self, f);
        }
    }
    let mut files = Vec::new();
    rs_files(&crates_dir().join("cli/src"), &mut files);
    let production = Production::of(&files);
    let mut probes = Probes {
        tool,
        found: Vec::new(),
    };
    for f in files.iter().filter(|f| !production.is_test_only(f)) {
        let parsed = syn::parse_file(&read(f)).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        probes.visit_file(&parsed);
    }
    probes.found
}

#[test]
fn each_recorded_floor_is_the_one_its_source_line_enforces() {
    for h in record().harness {
        let floor = &h.source_floor;
        match (&floor.file, floor.line) {
            (Some(file), Some(line)) => {
                let text = read(&workspace().join(file));
                let at = text.lines().nth(line - 1).unwrap_or_default();
                assert_eq!(
                    floor_on(at).as_deref(),
                    Some(floor.version.as_str()),
                    "{}: {file}:{line} is `{at}`, not a floor of {}; the lines that are: {:?}",
                    h.name,
                    floor.version,
                    text.lines()
                        .enumerate()
                        .filter(|(_, l)| floor_on(l).as_deref() == Some(&floor.version))
                        .map(|(i, _)| i + 1)
                        .collect::<Vec<_>>()
                );
            }
            _ => {
                let probes = probes_version_of(&h.tool());
                assert!(
                    probes.is_empty(),
                    "{} is recorded with no version check, but these probe its version: {probes:?}",
                    h.name
                );
            }
        }
    }
}

#[test]
fn the_floor_reader_finds_both_shapes_the_source_uses() {
    assert_eq!(floor_on("    v >= (2, 1, 223)").as_deref(), Some("2.1.223"));
    assert_eq!(
        floor_on("            .is_some_and(|v| v.0 == 1 && v >= (1, 0, 46));").as_deref(),
        Some("1.0.46")
    );
    assert_eq!(
        floor_on("const OPENCODE_PLUGIN_MAJOR: u64 = 2;").as_deref(),
        Some("2")
    );
    assert_eq!(floor_on("    let v = (a, b, c);"), None);
    assert!(
        !probes_version_of("claude").is_empty(),
        "the probe finder misses Claude's `--version` probe"
    );
    assert_eq!(floor_on("const BUDGET: Duration = 2;"), None);
}

/// The items a test identity's module path reaches: `it::` from the
/// integration harness, anything else from the lib.
fn resolve_test(id: &str) -> Result<(), String> {
    let cli = crates_dir().join("cli");
    let mut segments: Vec<&str> = id.split("::").collect();
    let name = segments.pop().ok_or("an empty identity")?;
    let mut file = if segments.first() == Some(&"it") {
        segments.remove(0);
        cli.join("tests/it/main.rs")
    } else {
        cli.join("src/lib.rs")
    };
    let mut items = syn::parse_file(&read(&file))
        .map_err(|e| e.to_string())?
        .items;
    let mut dir = file.parent().unwrap().to_path_buf();
    let mut top_level = true;
    for segment in segments {
        let module = items
            .iter()
            .find_map(|item| match item {
                syn::Item::Mod(m) if m.ident == segment => Some(m.clone()),
                _ => None,
            })
            .ok_or_else(|| format!("{id}: no module `{segment}` in {}", file.display()))?;
        if let Some((_, inner)) = module.content {
            items = inner;
            dir = dir.join(segment);
            top_level = false;
            continue;
        }
        let path_attr = module.attrs.iter().find_map(|a| match &a.meta {
            syn::Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(s.value()),
                _ => None,
            },
            _ => None,
        });
        let file_dir = file.parent().unwrap().to_path_buf();
        file = match path_attr {
            Some(p) if top_level => file_dir.join(p),
            Some(_) => return Err(format!("{id}: a #[path] inside an inline module")),
            None => [
                dir.join(format!("{segment}.rs")),
                dir.join(segment).join("mod.rs"),
            ]
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| format!("{id}: no file for module `{segment}`"))?,
        };
        items = syn::parse_file(&read(&file))
            .map_err(|e| e.to_string())?
            .items;
        dir = if file.file_name().is_some_and(|f| f == "mod.rs") {
            file.parent().unwrap().to_path_buf()
        } else {
            file.with_extension("")
        };
        top_level = true;
    }
    let ignored = items.iter().any(|item| {
        matches!(item, syn::Item::Fn(f) if f.sig.ident == name
            && f.attrs.iter().any(|a| a.path().is_ident("ignore")))
    });
    ignored
        .then_some(())
        .ok_or_else(|| format!("{id}: no #[ignore]d fn `{name}` in {}", file.display()))
}

/// The name `--exact` matches: the integration harness's tests are named
/// without the `it::` that tells them apart from the lib's.
fn exact_name(id: &str) -> &str {
    id.strip_prefix("it::").unwrap_or(id)
}

#[test]
fn every_required_test_is_a_real_ignored_test_with_its_command_documented() {
    let doc = read(&workspace().join("docs/first-run-walk.md"));
    for h in record().harness {
        for id in h.required().unwrap_or_default() {
            resolve_test(id).unwrap();
            let command = format!("--ignored --exact {}\n", exact_name(id));
            assert!(
                doc.contains(&command),
                "{}: docs/first-run-walk.md lacks `{}`",
                h.name,
                command.trim_end()
            );
        }
    }
    let unset = doc
        .lines()
        .find(|l| l.starts_with("unset "))
        .expect("the doc's identity unset line");
    let named: BTreeSet<&str> = unset.split_whitespace().skip(1).collect();
    for var in clank::agent_env::SESSION_IDENTITY_VARS {
        assert!(named.contains(var), "the doc's unset line misses {var}");
    }
}

#[test]
fn the_test_resolver_rejects_what_is_not_an_ignored_test() {
    resolve_test("it::claude_question_protocol::interactive_permission_question_hooks_arbitrate_with_the_local_terminal").unwrap();
    resolve_test("cli::grok_setup::tests::installed_initialize_advertises_the_native_baseline_without_starting_a_session").unwrap();
    assert!(resolve_test("it::claude_question_protocol::no_such_test").is_err());
    assert!(resolve_test("it::no_such_module::anything").is_err());
    assert!(
        resolve_test("it::harness_support::the_test_resolver_rejects_what_is_not_an_ignored_test")
            .is_err()
    );
}

/// Two harnesses: `Tool`, with two required tests, and `Unavailable`.
/// Each gets its README `tested` cell and its walk and acceptance entries.
fn fixture(tool: (&str, &str), unavailable: (&str, &str)) -> (String, String) {
    let record = format!(
        r#"
[[harness]]
name = "Tool"
required_acceptance = ["it::a::one", "it::a::two"]

[harness.source_floor]
version = "1.0.0"
readme = "1.0.0 or later"
file = "f.rs"
line = 1
{}

[[harness]]
name = "Unavailable"
required_acceptance = "unavailable"

[harness.source_floor]
version = "none"
readme = "no version check"
{}
"#,
        tool.1, unavailable.1
    );
    let readme = format!(
        "intro\n\n{TABLE_HEADER}\n|---|---|---|\n| Tool | 1.0.0 or later | {} |\n| Unavailable | no version check | {} |\n\nafter\n",
        tool.0, unavailable.0
    );
    (record, readme)
}

const UNTESTED: (&str, &str) = (NOT_YET_TESTED, "");

fn walk(outcome: &str) -> String {
    format!(
        "\n[[harness.walks]]\nversion = \"1.2.3\"\ndate = \"2026-11-05\"\noutcome = \"{outcome}\"\nrough_edges = [\"a-rough-edge\"]\n"
    )
}

fn accepted(test: &str, outcome: &str) -> String {
    format!(
        "\n[[harness.acceptance]]\ntest = \"{test}\"\nversion = \"1.2.3\"\ndate = \"2026-11-05\"\noutcome = \"{outcome}\"\n"
    )
}

fn both_passed() -> String {
    accepted("it::a::one", "passed") + &accepted("it::a::two", "passed")
}

fn rejects((record, readme): (String, String), because: &str) {
    let errors = violations(&record, &readme);
    assert!(
        errors.iter().any(|e| e.contains(because)),
        "expected a violation containing `{because}`, got {errors:#?}"
    );
}

#[test]
fn a_fully_evidenced_claim_is_accepted() {
    let (record, readme) = fixture(("1.2.3", &(walk("passed") + &both_passed())), UNTESTED);
    assert_eq!(violations(&record, &readme), Vec::<String>::new());
    let (record, readme) = fixture(UNTESTED, UNTESTED);
    assert_eq!(violations(&record, &readme), Vec::<String>::new());
}

#[test]
fn a_readme_version_with_no_record_is_rejected() {
    rejects(
        fixture(("1.2.4", &(walk("passed") + &both_passed())), UNTESTED),
        "Tool: README version `1.2.4` matches no recorded version",
    );
}

#[test]
fn a_claim_whose_walk_was_not_run_is_rejected() {
    rejects(
        fixture(("1.2.3", &(walk("not-run") + &both_passed())), UNTESTED),
        "Tool: 1.2.3 is claimed tested, but its latest walk is NotRun",
    );
}

#[test]
fn a_claim_whose_walk_failed_is_rejected() {
    rejects(
        fixture(("1.2.3", &(walk("failed") + &both_passed())), UNTESTED),
        "Tool: 1.2.3 is claimed tested, but its latest walk is Failed",
    );
    let failed_later = walk("passed") + &walk("failed").replace("2026-11-05", "2026-11-06");
    rejects(
        fixture(("1.2.3", &(failed_later + &both_passed())), UNTESTED),
        "its latest walk is Failed",
    );
}

#[test]
fn a_claim_for_a_harness_with_unavailable_acceptance_is_rejected() {
    rejects(
        fixture(UNTESTED, ("1.2.3", &walk("passed"))),
        "Unavailable: 1.2.3 is claimed tested, but its required acceptance is `unavailable`",
    );
}

#[test]
fn a_claim_missing_one_of_two_required_tests_is_rejected() {
    rejects(
        fixture(
            (
                "1.2.3",
                &(walk("passed") + &accepted("it::a::one", "passed")),
            ),
            UNTESTED,
        ),
        "Tool: 1.2.3 is claimed tested, but it has no acceptance entry for it::a::two",
    );
    rejects(
        fixture(
            (
                "1.2.3",
                &(walk("passed")
                    + &accepted("it::a::one", "passed")
                    + &accepted("it::a::two", "failed")),
            ),
            UNTESTED,
        ),
        "Tool: 1.2.3 is claimed tested, but its latest it::a::two is Failed",
    );
}

#[test]
fn a_floor_or_row_out_of_step_with_the_record_is_rejected() {
    let (record, readme) = fixture(UNTESTED, UNTESTED);
    rejects(
        (
            record.clone(),
            readme.replace("| 1.0.0 or later |", "| 0.9 or later |"),
        ),
        "Tool: the README floor `0.9 or later` is not the source floor `1.0.0 or later`",
    );
    rejects(
        (
            record.clone(),
            readme.replace("| Unavailable | no version check | not yet tested |\n", ""),
        ),
        "Unavailable: recorded but missing from the README table",
    );
    rejects(
        (record.clone(), readme.replace("| Tool |", "| Other |")),
        "Other: a README row missing from the record",
    );
    rejects(
        (
            record.replace("[\"it::a::one\", \"it::a::two\"]", "[]"),
            readme,
        ),
        "Tool: required_acceptance is empty",
    );
}

#[test]
fn a_rough_edge_must_be_a_plan_stem() {
    let path = walk("failed").replace("a-rough-edge", ".clank/queue/001-a-rough-edge.md");
    rejects(
        fixture((NOT_YET_TESTED, &path), UNTESTED),
        "is not a plan stem",
    );
}

#[test]
fn an_acceptance_entry_for_a_test_not_required_is_rejected() {
    rejects(
        fixture(
            (NOT_YET_TESTED, &accepted("it::a::three", "passed")),
            UNTESTED,
        ),
        "acceptance entry `it::a::three` is not a required test",
    );
}

/// The provider-root variables `resolve_provider_root` reads, from its
/// parsed body.
fn provider_root_vars() -> BTreeSet<String> {
    struct Lits(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Lits {
        fn visit_lit_str(&mut self, s: &'ast syn::LitStr) {
            let v = s.value();
            if v.len() > 3 && v.bytes().all(|c| c.is_ascii_uppercase() || c == b'_') {
                self.0.insert(v);
            }
        }
    }
    let parsed = syn::parse_file(&read(&crates_dir().join("cli/src/cli/setup.rs"))).unwrap();
    let f = parsed
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(f) if f.sig.ident == "resolve_provider_root" => Some(f),
            _ => None,
        })
        .expect("resolve_provider_root");
    let mut lits = Lits(BTreeSet::new());
    lits.visit_item_fn(f);
    lits.0
}

fn session_name_max() -> usize {
    read(&crates_dir().join("cli/src/cli/open_zellij.rs"))
        .lines()
        .find_map(|l| l.strip_prefix("const SESSION_NAME_MAX: usize = "))
        .and_then(|rest| rest.strip_suffix(';'))
        .and_then(|n| n.parse().ok())
        .expect("SESSION_NAME_MAX in open_zellij.rs")
}

fn listing(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.insert(p);
        }
    }
    out
}

#[test]
fn the_walk_environment_reaches_no_production_profile_or_session() {
    const SENTINEL: &str = "/sentinel-production-profile";
    let roots = provider_root_vars();
    assert!(
        roots.contains("CLAUDE_CONFIG_DIR") && roots.contains("XDG_CONFIG_HOME"),
        "{roots:?}"
    );
    let identity = clank::agent_env::SESSION_IDENTITY_VARS;
    let temp = tempfile::tempdir().unwrap();
    let parent_home = temp.path().join("home");
    let tmp = temp.path().join("tmp");
    std::fs::create_dir_all(&parent_home).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    let before = listing(temp.path());
    let script = workspace().join("scripts/first-run/env.sh");
    for h in record().harness {
        let mut command = std::process::Command::new("sh");
        command
            .arg(&script)
            .args(["--print", &h.tool()])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &parent_home)
            .env("TMPDIR", &tmp)
            .env("TERM", "dumb")
            .env("LANG", "C")
            .env("USER", "walker");
        for var in &roots {
            command.env(var, format!("{SENTINEL}/{var}"));
        }
        for var in identity {
            command.env(var, format!("{SENTINEL}-{var}"));
        }
        let out = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{}: {stdout}{}",
            h.name,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !stdout.contains(SENTINEL),
            "{}: an inherited value leaked:\n{stdout}",
            h.name
        );
        let field = |key: &str| {
            stdout
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{key} ")))
                .unwrap_or_else(|| panic!("{}: no `{key}` line:\n{stdout}", h.name))
                .to_string()
        };
        let root = PathBuf::from(field("root"));
        let repo = PathBuf::from(field("repo"));
        assert!(
            root.starts_with(&tmp),
            "{}: {root:?} is outside $TMPDIR",
            h.name
        );
        assert!(repo.starts_with(&root), "{}: {repo:?}", h.name);
        assert!(!root.exists(), "{}: --print created {root:?}", h.name);
        let session = field("session");
        assert_eq!(
            session,
            format!("clank-{}", repo.file_name().unwrap().to_string_lossy()),
            "{}: `clank open` would name a different session",
            h.name
        );
        assert!(session.len() <= session_name_max(), "{}: {session}", h.name);
        let set: BTreeMap<&str, &str> = stdout
            .lines()
            .filter_map(|l| l.strip_prefix("set ")?.split_once('='))
            .collect();
        let unset: BTreeSet<&str> = stdout
            .lines()
            .filter_map(|l| l.strip_prefix("unset ")?.split_whitespace().next())
            .collect();
        assert_eq!(
            set.keys().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from(["HOME", "PATH", "TERM", "LANG", "USER"]),
            "{}: env -i keeps only PATH, TERM, LANG and USER, and sets HOME",
            h.name
        );
        assert_eq!(Path::new(set["HOME"]), root.join("home"), "{}", h.name);
        for var in &roots {
            let remapped = set
                .get(var.as_str())
                .is_some_and(|v| Path::new(v).starts_with(&root));
            assert!(
                remapped || unset.contains(var.as_str()),
                "{}: {var} is neither remapped under {root:?} nor unset",
                h.name
            );
        }
        for var in identity {
            assert!(
                unset.contains(var) && !set.contains_key(var),
                "{}: {var} survives",
                h.name
            );
        }
    }
    assert_eq!(listing(temp.path()), before, "--print created files");
}
