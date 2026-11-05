//! Enforcement gate: the Pages workflow deploys only the book of the
//! commit it validated, rebuilds only for the book, and holds the deploy
//! credentials in the deploy job alone.
//!
//! Each rule is read from the workflow's structure, not from a string
//! search, because the failures that matter (an upload before the
//! check, a deploy that does not wait for the build) keep every action
//! name in place.

use crate::ci_workflow::{assert_every_test_step_unsets_every_identity_variable, read_workflow};

/// One line of block-style YAML and the more-indented lines under it.
#[derive(Debug)]
struct Node {
    text: String,
    children: Vec<Node>,
}

/// The workflow as a tree of lines by indentation, comments and blank
/// lines dropped. A `- key: v` sequence item's sibling keys are its
/// children, as they are indented under it.
fn parse(text: &str) -> Vec<Node> {
    let mut lines = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| (l.len() - l.trim_start().len(), l.trim().to_string()))
        .peekable();
    fn level(
        lines: &mut std::iter::Peekable<impl Iterator<Item = (usize, String)>>,
        indent: usize,
    ) -> Vec<Node> {
        let mut out = Vec::new();
        while let Some((at, _)) = lines.peek() {
            if *at < indent {
                break;
            }
            let (at, text) = lines.next().unwrap();
            let children = match lines.peek().map(|(next, _)| *next) {
                Some(next) if next > at => level(lines, next),
                _ => Vec::new(),
            };
            out.push(Node { text, children });
        }
        out
    }
    level(&mut lines, 0)
}

fn key_of(text: &str) -> &str {
    let text = text.strip_prefix("- ").unwrap_or(text);
    text.split_once(':').map_or(text, |(k, _)| k.trim())
}

fn value_of(text: &str) -> Option<&str> {
    let text = text.strip_prefix("- ").unwrap_or(text);
    text.split_once(':')
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

fn child<'a>(nodes: &'a [Node], key: &str) -> Option<&'a Node> {
    nodes.iter().find(|n| key_of(&n.text) == key)
}

fn at<'a>(nodes: &'a [Node], path: &[&str]) -> Option<&'a Node> {
    let (first, rest) = path.split_first()?;
    let node = child(nodes, first)?;
    if rest.is_empty() {
        Some(node)
    } else {
        at(&node.children, rest)
    }
}

fn value<'a>(nodes: &'a [Node], path: &[&str]) -> Option<&'a str> {
    value_of(&at(nodes, path)?.text)
}

/// A mapping's entries as `key: value` lines, sorted.
fn entries(node: Option<&Node>) -> Vec<String> {
    let mut out: Vec<String> = node
        .map(|n| n.children.iter().map(|c| c.text.clone()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

fn keys(node: Option<&Node>) -> Vec<String> {
    let mut out: Vec<String> = node
        .map(|n| {
            n.children
                .iter()
                .map(|c| key_of(&c.text).to_string())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Every field of a step, its first line's included.
fn step_field<'a>(step: &'a Node, key: &str) -> Option<&'a str> {
    if key_of(&step.text) == key {
        return value_of(&step.text);
    }
    child(&step.children, key).and_then(|n| value_of(&n.text))
}

fn steps<'a>(root: &'a [Node], job: &str) -> &'a [Node] {
    at(root, &["jobs", job, "steps"]).map_or(&[], |n| &n.children)
}

fn uses_action(step: &Node, action: &str) -> bool {
    step_field(step, "uses").is_some_and(|u| u.starts_with(&format!("{action}@")))
}

const VALIDATION: &str = "cargo test --locked -p clank --test it -- book_is_true:: cli_reference::";

/// Every rule the workflow `text` breaks.
fn violations(text: &str) -> Vec<String> {
    let root = parse(text);
    let mut bad = Vec::new();
    let mut want = |ok: bool, rule: &str| {
        if !ok {
            bad.push(rule.to_string());
        }
    };

    want(
        keys(at(&root, &["on"])) == ["push", "workflow_dispatch"],
        "it triggers on push and workflow_dispatch only",
    );
    want(
        value(&root, &["on", "push", "branches"]) == Some("[master]"),
        "a push rebuilds only from master",
    );
    want(
        entries(at(&root, &["on", "push", "paths"]))
            == [r#"- ".github/workflows/pages.yml""#, r#"- "book/**""#],
        "the paths filter is exactly the book and this workflow",
    );
    want(
        entries(at(&root, &["concurrency"])) == ["cancel-in-progress: true", "group: pages"],
        "one pages concurrency group cancels the run in progress",
    );
    want(
        keys(at(&root, &["jobs"])) == ["build", "deploy"],
        "the jobs are build and deploy",
    );
    want(
        at(&root, &["permissions"]).is_none(),
        "no workflow-wide permissions",
    );

    want(
        entries(at(&root, &["jobs", "build", "permissions"])) == ["contents: read"],
        "build may only read contents",
    );
    let build = steps(&root, "build");
    let checkout = build
        .iter()
        .position(|s| uses_action(s, "actions/checkout"));
    let validation = build
        .iter()
        .position(|s| step_field(s, "run").is_some_and(|r| r.ends_with(VALIDATION)));
    let upload = build
        .iter()
        .position(|s| uses_action(s, "actions/upload-pages-artifact"));
    want(checkout == Some(0), "build checks out the commit first");
    want(validation.is_some(), "build runs the book's validation");
    want(
        upload.is_some_and(|u| validation.is_some_and(|v| v < u)),
        "build validates before it uploads",
    );
    want(
        upload.is_some_and(|u| {
            at(&build[u].children, &["with"]).is_some_and(|w| entries(Some(w)) == ["path: book"])
        }),
        "build uploads book/ and nothing else",
    );

    let deploy = at(&root, &["jobs", "deploy"]).map_or(&[][..], |n| &n.children);
    want(
        value(deploy, &["needs"]) == Some("build"),
        "deploy needs build",
    );
    want(
        value(deploy, &["environment", "name"]) == Some("github-pages"),
        "deploy runs in the github-pages environment",
    );
    want(
        value(deploy, &["environment", "url"]) == Some("${{ steps.deployment.outputs.page_url }}"),
        "the environment's url is the deployment's",
    );
    want(
        entries(at(deploy, &["permissions"])) == ["id-token: write", "pages: write"],
        "deploy holds exactly the pages and id-token permissions",
    );
    let deploy_steps = steps(&root, "deploy");
    want(
        deploy_steps.len() == 1
            && uses_action(&deploy_steps[0], "actions/deploy-pages")
            && step_field(&deploy_steps[0], "id") == Some("deployment"),
        "deploy runs actions/deploy-pages as the deployment step",
    );

    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    want(
        !words.iter().any(|w| *w == "fly" || w.starts_with("flyctl")),
        "nothing deploys to fly",
    );
    want(!lower.contains("secrets."), "no secret is read");
    bad
}

#[test]
fn the_pages_workflow_deploys_only_the_validated_book() {
    let bad = violations(&read_workflow("pages.yml"));
    assert!(bad.is_empty(), "pages.yml breaks: {bad:?}");
}

#[test]
fn the_pages_workflow_runs_its_tests_without_an_agent_identity() {
    assert_every_test_step_unsets_every_identity_variable(&read_workflow("pages.yml"));
}

/// The real workflow with `old` replaced by `new`.
fn mutated(old: &str, new: &str) -> String {
    let text = read_workflow("pages.yml");
    assert!(text.contains(old), "pages.yml lacks {old:?}");
    text.replacen(old, new, 1)
}

fn assert_rejected(text: &str, rule: &str) {
    let bad = violations(text);
    assert!(
        bad.iter().any(|b| b == rule),
        "expected {rule:?}, got {bad:?}"
    );
}

#[test]
fn a_deploy_that_does_not_wait_for_the_build_is_rejected() {
    assert_rejected(&mutated("    needs: build\n", ""), "deploy needs build");
}

#[test]
fn an_upload_before_the_validation_is_rejected() {
    let text = read_workflow("pages.yml");
    let upload = "      - uses: actions/upload-pages-artifact@";
    let start = text.find(upload).expect("an upload step");
    let end = start + text[start..].find("\n\n").expect("a blank line after it") + 2;
    let step = &text[start..end];
    let without = format!("{}{}", &text[..start], &text[end..]);
    let validation = "      # The artifact is";
    let at = without
        .find(validation)
        .expect("the validation step's comment");
    let moved = format!("{}{step}{}", &without[..at], &without[at..]);
    assert_rejected(&moved, "build validates before it uploads");
}

#[test]
fn a_workflow_without_the_concurrency_group_is_rejected() {
    assert_rejected(
        &mutated(
            "concurrency:\n  group: pages\n  cancel-in-progress: true\n",
            "",
        ),
        "one pages concurrency group cancels the run in progress",
    );
    assert_rejected(
        &mutated("cancel-in-progress: true", "cancel-in-progress: false"),
        "one pages concurrency group cancels the run in progress",
    );
}

#[test]
fn a_wider_paths_filter_is_rejected() {
    assert_rejected(
        &mutated(
            "      - \"book/**\"\n",
            "      - \"book/**\"\n      - \"crates/**\"\n",
        ),
        "the paths filter is exactly the book and this workflow",
    );
}

#[test]
fn deploy_permissions_on_the_build_job_are_rejected() {
    assert_rejected(
        &mutated(
            "      contents: read\n",
            "      contents: read\n      pages: write\n",
        ),
        "build may only read contents",
    );
}

#[test]
fn a_secret_is_rejected() {
    assert_rejected(
        &mutated(
            "          path: book\n",
            "          path: book\n          token: ${{ secrets.FLY_API_TOKEN }}\n",
        ),
        "no secret is read",
    );
}
