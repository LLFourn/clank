use leptos::prelude::*;

use crate::api::{DiffHunk, DiffLine, DiffLineKind, FileDiff, FileDiffMode};

/// Render a list of `FileDiff`s as collapsible per-file sections with a
/// line-number gutter and insert/delete coloring. Empty input renders a
/// muted "no changes" line.
#[component]
pub fn StructuredDiff(files: Vec<FileDiff>) -> impl IntoView {
    if files.is_empty() {
        return view! { <p class="muted">"No changes."</p> }.into_any();
    }
    let header = view! {
        <ul class="diff-file-list">
            {files
                .iter()
                .map(|f| {
                    let target = format!("#diff-{}", slug(&f.path));
                    let counts = format!("+{} -{}", f.additions, f.deletions);
                    let path = f.path.clone();
                    view! {
                        <li>
                            <a href=target>
                                <code>{path}</code>
                            </a>
                            <span class="diff-counts">{counts}</span>
                        </li>
                    }
                })
                .collect_view()}
        </ul>
    };
    let bodies = files
        .into_iter()
        .map(|f| view! { <FileDiffBlock file=f/> })
        .collect_view();
    view! {
        <div class="structured-diff">
            {header}
            <div class="diff-bodies">{bodies}</div>
        </div>
    }
    .into_any()
}

#[component]
fn FileDiffBlock(file: FileDiff) -> impl IntoView {
    let anchor = format!("diff-{}", slug(&file.path));
    let counts = format!("+{} -{}", file.additions, file.deletions);
    let mode_chip = mode_chip(file.mode);
    let summary_path = match &file.old_path {
        Some(old) if !old.is_empty() && old != &file.path => {
            format!("{} → {}", old, file.path)
        }
        _ => file.path.clone(),
    };
    let body: AnyView = if file.binary {
        view! { <p class="muted">"(binary)"</p> }.into_any()
    } else if file.hunks.is_empty() {
        view! { <p class="muted">"(no textual changes)"</p> }.into_any()
    } else {
        file.hunks
            .into_iter()
            .map(|h| view! { <HunkBlock hunk=h/> })
            .collect_view()
            .into_any()
    };
    view! {
        <details
            id=anchor
            class="diff-file"
            open=move || !file.always_folded
        >
            <summary class="diff-file-summary">
                <span class=mode_chip.0>{mode_chip.1}</span>
                <code class="diff-file-path">{summary_path}</code>
                <span class="diff-counts">{counts}</span>
            </summary>
            <div class="diff-file-body">{body}</div>
        </details>
    }
}

#[component]
fn HunkBlock(hunk: DiffHunk) -> impl IntoView {
    view! {
        <div class="diff-hunk">
            <div class="diff-hunk-header">
                <code>{hunk.header}</code>
            </div>
            <table class="diff-table">
                <tbody>
                    {hunk
                        .lines
                        .into_iter()
                        .map(|l| view! { <DiffLineRow line=l/> })
                        .collect_view()}
                </tbody>
            </table>
        </div>
    }
}

#[component]
fn DiffLineRow(line: DiffLine) -> impl IntoView {
    let row_class = format!("diff-line diff-line-{}", line.kind);
    let old_no = line.old_lineno.map(|n| n.to_string()).unwrap_or_default();
    let new_no = line.new_lineno.map(|n| n.to_string()).unwrap_or_default();
    let marker = match line.kind {
        DiffLineKind::Insert => "+",
        DiffLineKind::Delete => "-",
        DiffLineKind::Meta => "@",
        DiffLineKind::Context => " ",
    };
    view! {
        <tr class=row_class>
            <td class="diff-num diff-num-old">{old_no}</td>
            <td class="diff-num diff-num-new">{new_no}</td>
            <td class="diff-marker">{marker}</td>
            <td class="diff-content">
                <code>{line.content}</code>
            </td>
        </tr>
    }
}

fn mode_chip(mode: FileDiffMode) -> (&'static str, &'static str) {
    match mode {
        FileDiffMode::Added => ("file-mode file-mode-added", "ADDED"),
        FileDiffMode::Removed => ("file-mode file-mode-removed", "REMOVED"),
        FileDiffMode::Renamed => ("file-mode file-mode-renamed", "RENAMED"),
        FileDiffMode::Modified => ("file-mode file-mode-modified", "MODIFIED"),
    }
}

/// Stable in-page anchor for a file path. Replaces non-ASCII /
/// non-alphanumeric chars with `-`.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
