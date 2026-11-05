//! A GitHub workflow read as a tree of lines by indentation, enough for
//! the workflow gates to check structure rather than search strings.

/// One line of block-style YAML and the more-indented lines under it.
#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) text: String,
    pub(crate) children: Vec<Node>,
}

/// The workflow as a tree of lines by indentation, comments and blank
/// lines dropped. A `- key: v` sequence item's sibling keys are its
/// children, as they are indented under it.
pub(crate) fn parse(text: &str) -> Vec<Node> {
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

pub(crate) fn key_of(text: &str) -> &str {
    let text = text.strip_prefix("- ").unwrap_or(text);
    text.split_once(':').map_or(text, |(k, _)| k.trim())
}

pub(crate) fn value_of(text: &str) -> Option<&str> {
    let text = text.strip_prefix("- ").unwrap_or(text);
    text.split_once(':')
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

pub(crate) fn child<'a>(nodes: &'a [Node], key: &str) -> Option<&'a Node> {
    nodes.iter().find(|n| key_of(&n.text) == key)
}

pub(crate) fn at<'a>(nodes: &'a [Node], path: &[&str]) -> Option<&'a Node> {
    let (first, rest) = path.split_first()?;
    let node = child(nodes, first)?;
    if rest.is_empty() {
        Some(node)
    } else {
        at(&node.children, rest)
    }
}

pub(crate) fn value<'a>(nodes: &'a [Node], path: &[&str]) -> Option<&'a str> {
    value_of(&at(nodes, path)?.text)
}

/// A mapping's entries as `key: value` lines, sorted.
pub(crate) fn entries(node: Option<&Node>) -> Vec<String> {
    let mut out: Vec<String> = node
        .map(|n| n.children.iter().map(|c| c.text.clone()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

pub(crate) fn keys(node: Option<&Node>) -> Vec<String> {
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
pub(crate) fn step_field<'a>(step: &'a Node, key: &str) -> Option<&'a str> {
    if key_of(&step.text) == key {
        return value_of(&step.text);
    }
    child(&step.children, key).and_then(|n| value_of(&n.text))
}

pub(crate) fn steps<'a>(root: &'a [Node], job: &str) -> &'a [Node] {
    at(root, &["jobs", job, "steps"]).map_or(&[], |n| &n.children)
}

pub(crate) fn uses_action(step: &Node, action: &str) -> bool {
    step_field(step, "uses").is_some_and(|u| u.starts_with(&format!("{action}@")))
}

/// A step's `run` script: its one line, or a `run: |` block's lines.
pub(crate) fn run_text(step: &Node) -> Option<String> {
    let run = if key_of(&step.text) == "run" {
        step
    } else {
        child(&step.children, "run")?
    };
    fn lines(nodes: &[Node], out: &mut Vec<String>) {
        for n in nodes {
            out.push(n.text.clone());
            lines(&n.children, out);
        }
    }
    match value_of(&run.text) {
        Some("|") => {
            let mut out = Vec::new();
            lines(&run.children, &mut out);
            Some(out.join("\n"))
        }
        v => v.map(str::to_string),
    }
}
