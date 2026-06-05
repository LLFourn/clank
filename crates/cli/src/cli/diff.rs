//! `clank diff <plan|range>` — launch the configured editor on a
//! plan or commit-range diff, with optional agent-supplied
//! prompt/focus hints.
//!
//! Two semantically-distinct kinds (`Plan` vs `Range`) per OQ5 of
//! `clank-diff-editor`'s plan body. Each surfaces distinct env
//! vars + template variables to the editor; no leakage between
//! kinds (a plan invocation does NOT set `CLANK_DIFF_RANGE`, and
//! vice versa).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;

use crate::lifecycle::{CommitSha, PlanKey};
use clank_core::agent_config::LaunchConfig;

use super::DiffArgs;

/// Two semantically-distinct invocation kinds. Each surfaces a
/// different env-var set and recognizes a different template-var
/// vocabulary in `LaunchConfig.args`.
#[derive(Debug, Clone)]
enum DiffKind {
    Plan {
        plan: PlanKey,
        commits: Vec<CommitSha>,
    },
    Range {
        from: Option<CommitSha>,
        to: CommitSha,
    },
}

impl DiffKind {
    fn kind_str(&self) -> &'static str {
        match self {
            DiffKind::Plan { .. } => "plan",
            DiffKind::Range { .. } => "range",
        }
    }
}

#[derive(Debug, Clone)]
struct ComposedDiffLaunch {
    program: String,
    args: Vec<String>,
    env_overrides: BTreeMap<String, String>,
    wait: bool,
}

pub async fn run(args: DiffArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let basename = super::repo_basename(&repo)?;

    let state = crate::rebuild::rebuild_repo_with_policy(&repo, crate::rebuild::CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;

    // OQ1 disambiguation: try plan first, then range, with an
    // explicit "neither" error listing candidates.
    let kind = resolve_kind(&repo, &state, &basename, &args).await?;

    let config = super::config::load(&repo);
    let editor = config.diff.editor.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "no editor configured; set diff.editor.command in ~/.clank/config.json \
             (or your repo's .clank/config.json). Example: \
             `clank config set diff.editor.command emacsclient`."
        )
    })?;

    // OQ4: wait flag overrides config default. `--wait` and
    // `--no-wait` are clap-conflicting; at most one is set.
    let wait = if args.wait {
        true
    } else if args.no_wait {
        false
    } else {
        config.diff.wait.unwrap_or(false)
    };

    let composed = compose(&repo, &kind, &editor, &args, wait)?;

    if args.print {
        print_composed(&composed);
        return Ok(());
    }

    spawn(composed).await
}

/// Resolve which kind of diff this invocation targets per OQ1.
async fn resolve_kind(
    repo: &Path,
    state: &crate::repo_state::RepoState,
    basename: &str,
    args: &DiffArgs,
) -> anyhow::Result<DiffKind> {
    if let Some(plan_arg) = &args.plan {
        let key = crate::cli::plan_resolve::resolve_plan(state, basename, Some(plan_arg))?;
        let commits = crate::cli::plan_resolve::commits_for_plan(repo, state, &key).await?;
        return Ok(DiffKind::Plan { plan: key, commits });
    }
    if let Some(range_arg) = &args.range {
        let (from, to) = parse_range(repo, range_arg)?;
        return Ok(DiffKind::Range { from, to });
    }
    if let Some(target) = &args.target {
        // OQ1 disambiguation: try plan-resolve first.
        match crate::cli::plan_resolve::resolve_plan(state, basename, Some(target)) {
            Ok(key) => {
                let commits = crate::cli::plan_resolve::commits_for_plan(repo, state, &key).await?;
                return Ok(DiffKind::Plan { plan: key, commits });
            }
            Err(_) => {
                // Fall through to range parse. If THAT fails too,
                // emit the OQ1 diagnostic with candidates listed.
                match parse_range(repo, target) {
                    Ok((from, to)) => return Ok(DiffKind::Range { from, to }),
                    Err(_) => {
                        let candidates = active_plan_candidates(state);
                        anyhow::bail!(
                            "`{target}` is not a known plan and is not a valid git range.\n\
                             Available plans: {candidates}.\n\
                             To pass a literal range, use `--range <arg>`.",
                            target = target,
                            candidates = candidates,
                        );
                    }
                }
            }
        }
    }
    // No positional, no --plan, no --range: infer single active
    // plan (mirrors `clank log` without --plan).
    let key = crate::cli::plan_resolve::resolve_plan(state, basename, None)?;
    let commits = crate::cli::plan_resolve::commits_for_plan(repo, state, &key).await?;
    Ok(DiffKind::Plan { plan: key, commits })
}

fn active_plan_candidates(state: &crate::repo_state::RepoState) -> String {
    let mut names: Vec<String> = state
        .fold
        .plans
        .keys()
        .map(|k| k.as_str().to_string())
        .collect();
    names.sort();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// Parse a git range string into `(Option<from>, to)`. Accepts
/// `<from>..<to>` AND bare `<sha>` (latter expands to
/// `<sha>^..<sha>`).
fn parse_range(repo: &Path, raw: &str) -> anyhow::Result<(Option<CommitSha>, CommitSha)> {
    if let Some((from, to)) = raw.split_once("..") {
        let from_sha = resolve_to_sha(repo, from)?;
        let to_sha = resolve_to_sha(repo, to)?;
        return Ok((Some(from_sha), to_sha));
    }
    // Bare SHA: expand to `<sha>^..<sha>` semantics.
    let sha = resolve_to_sha(repo, raw)?;
    let parent = resolve_to_sha(repo, &format!("{raw}^")).ok();
    Ok((parent, sha))
}

fn resolve_to_sha(repo: &Path, rev: &str) -> anyhow::Result<CommitSha> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", rev])
        .output()
        .with_context(|| format!("invoking git rev-parse `{rev}`"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git rev-parse `{rev}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    CommitSha::parse(&sha).map_err(|e| anyhow::anyhow!("invalid SHA from rev-parse: {e}"))
}

fn compose(
    repo: &Path,
    kind: &DiffKind,
    editor: &LaunchConfig,
    args: &DiffArgs,
    wait: bool,
) -> anyhow::Result<ComposedDiffLaunch> {
    let program = editor
        .command
        .clone()
        .ok_or_else(|| anyhow::anyhow!("diff.editor.command is required but not set"))?;

    // Template substitution + tempfile synthesis (when {patch_file}
    // appears). Done in one pass so we only synthesize the patch
    // when actually requested.
    let mut composed_args: Vec<String> = Vec::with_capacity(editor.args.len());
    let mut patch_file_path: Option<PathBuf> = None;
    for raw in &editor.args {
        let substituted = substitute(raw, repo, kind, args, &mut patch_file_path)?;
        composed_args.push(substituted);
    }

    let mut env: BTreeMap<String, String> = editor.env.clone();
    env.insert("CLANK_DIFF_KIND".to_string(), kind.kind_str().to_string());
    env.insert("CLANK_DIFF_REPO".to_string(), repo.display().to_string());
    if let Some(prompt) = &args.prompt {
        env.insert("CLANK_DIFF_PROMPT".to_string(), prompt.clone());
    }
    if !args.focus.is_empty() {
        let normalized: Vec<String> = args.focus.iter().map(|f| normalize_focus(f)).collect();
        env.insert("CLANK_DIFF_FOCUS".to_string(), normalized.join("\n"));
    }
    match kind {
        DiffKind::Plan { plan, commits } => {
            env.insert("CLANK_DIFF_PLAN".to_string(), plan.as_str().to_string());
            let joined = commits
                .iter()
                .map(|c| c.as_str().to_string())
                .collect::<Vec<_>>()
                .join(",");
            env.insert("CLANK_DIFF_COMMITS".to_string(), joined);
        }
        DiffKind::Range { from, to } => {
            let range_str = match from {
                Some(f) => format!("{}..{}", f.as_str(), to.as_str()),
                None => to.as_str().to_string(),
            };
            env.insert("CLANK_DIFF_RANGE".to_string(), range_str);
        }
    }

    Ok(ComposedDiffLaunch {
        program,
        args: composed_args,
        env_overrides: env,
        wait,
    })
}

/// Substitute one template variable use in an arg. Errors when a
/// wrong-kind template variable is used (e.g. `{range}` in a plan
/// invocation).
fn substitute(
    raw: &str,
    repo: &Path,
    kind: &DiffKind,
    args: &DiffArgs,
    patch_file_path: &mut Option<PathBuf>,
) -> anyhow::Result<String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after
            .find('}')
            .ok_or_else(|| anyhow::anyhow!("unterminated `{{` in arg `{raw}`"))?;
        let var = &after[..end];
        let replacement = substitute_var(var, repo, kind, args, patch_file_path)?;
        out.push_str(&replacement);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn substitute_var(
    var: &str,
    repo: &Path,
    kind: &DiffKind,
    args: &DiffArgs,
    patch_file_path: &mut Option<PathBuf>,
) -> anyhow::Result<String> {
    match (var, kind) {
        ("repo", _) => Ok(repo.display().to_string()),
        ("prompt", _) => Ok(args.prompt.clone().unwrap_or_default()),
        ("patch_file", _) => {
            if patch_file_path.is_none() {
                let path = synthesize_patch_file(repo, kind)?;
                *patch_file_path = Some(path);
            }
            Ok(patch_file_path.as_ref().unwrap().display().to_string())
        }
        ("range", DiffKind::Range { from, to }) => Ok(match from {
            Some(f) => format!("{}..{}", f.as_str(), to.as_str()),
            None => to.as_str().to_string(),
        }),
        ("from", DiffKind::Range { from, .. }) => Ok(from
            .as_ref()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default()),
        ("to", DiffKind::Range { to, .. }) => Ok(to.as_str().to_string()),
        ("plan", DiffKind::Plan { plan, .. }) => Ok(plan.as_str().to_string()),
        ("commits", DiffKind::Plan { commits, .. }) => Ok(commits
            .iter()
            .map(|c| c.as_str().to_string())
            .collect::<Vec<_>>()
            .join(",")),
        ("first_commit", DiffKind::Plan { commits, .. }) => commits
            .first()
            .map(|c| c.as_str().to_string())
            .ok_or_else(|| anyhow::anyhow!("plan has no commits; `{{first_commit}}` unavailable")),
        ("last_commit", DiffKind::Plan { commits, .. }) => commits
            .last()
            .map(|c| c.as_str().to_string())
            .ok_or_else(|| anyhow::anyhow!("plan has no commits; `{{last_commit}}` unavailable")),
        // Wrong-kind variables → explicit error per Phase 4 spec.
        ("range" | "from" | "to", DiffKind::Plan { .. }) => anyhow::bail!(
            "`{{{var}}}` is a range-only template variable; this invocation is a plan diff. \
             Use `{{commits}}` / `{{first_commit}}` / `{{last_commit}}` or pass `--range`."
        ),
        ("plan" | "commits" | "first_commit" | "last_commit", DiffKind::Range { .. }) => {
            anyhow::bail!(
                "`{{{var}}}` is a plan-only template variable; this invocation is a range diff. \
             Use `{{range}}` / `{{from}}` / `{{to}}` or pass `--plan`."
            )
        }
        (other, _) => anyhow::bail!("unknown template variable `{{{other}}}`"),
    }
}

fn synthesize_patch_file(repo: &Path, kind: &DiffKind) -> anyhow::Result<PathBuf> {
    let mut tmp = tempfile::Builder::new()
        .prefix("clank-diff-")
        .suffix(".patch")
        .tempfile()
        .context("creating temp patch file")?;
    let body = match kind {
        DiffKind::Range { from, to } => {
            let range_str = match from {
                Some(f) => format!("{}..{}", f.as_str(), to.as_str()),
                None => to.as_str().to_string(),
            };
            let out = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["diff", &range_str])
                .output()
                .context("invoking git diff for patch synthesis")?;
            if !out.status.success() {
                anyhow::bail!(
                    "git diff `{range_str}` failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            out.stdout
        }
        DiffKind::Plan { commits, .. } => {
            // Stacked diff: per-commit `git show --format= --patch`
            // concatenated. Mirrors `git format-patch` output minus
            // mail headers.
            let mut combined = Vec::new();
            for sha in commits {
                let out = Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(["show", "--format=fuller", "--patch", sha.as_str()])
                    .output()
                    .context("invoking git show for plan patch synthesis")?;
                if !out.status.success() {
                    anyhow::bail!(
                        "git show `{}` failed: {}",
                        sha.as_str(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                combined.extend_from_slice(&out.stdout);
                combined.push(b'\n');
            }
            combined
        }
    };
    tmp.write_all(&body).context("writing patch tempfile")?;
    tmp.flush().context("flushing patch tempfile")?;
    let (_, path) = tmp.keep().context("persisting patch tempfile")?;
    Ok(path)
}

/// Normalize a `--focus` arg per OQ3: `file:line` → `file:line-line`;
/// `file` → `file` (whole-file form).
fn normalize_focus(raw: &str) -> String {
    if let Some((file, lines)) = raw.split_once(':') {
        if lines.contains('-') {
            return raw.to_string();
        }
        // Single-line shorthand: `:N` → `:N-N`.
        return format!("{file}:{lines}-{lines}");
    }
    // No `:`: whole-file form. Pass through verbatim.
    raw.to_string()
}

fn print_composed(c: &ComposedDiffLaunch) {
    let mut line = shell_quote(&c.program);
    for a in &c.args {
        line.push(' ');
        line.push_str(&shell_quote(a));
    }
    println!("{line}");
    for (k, v) in &c.env_overrides {
        // Escape newlines so multi-line values (CLANK_DIFF_FOCUS
        // with multiple --focus flags, etc.) stay on one line.
        // Tests + zellij-style consumers split on \n to recover.
        let escaped = v.replace('\\', "\\\\").replace('\n', "\\n");
        eprintln!("env: {k}={escaped}");
    }
    eprintln!("wait: {}", c.wait);
}

fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(unix)]
async fn spawn(c: ComposedDiffLaunch) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    if c.wait {
        // --wait: install a SIGINT handler that forwards to the
        // child PID and continues waiting (per OQ6 of the plan
        // body). Clank's default SIGINT behavior would terminate
        // clank before the child exits; the explicit handler
        // keeps clank alive until the editor cleans up.
        return wait_with_sigint_forward(c).await;
    }

    // Fire-and-forget: detach stdio, spawn, drop. The child
    // outlives clank.
    let mut cmd = Command::new(&c.program);
    cmd.args(&c.args);
    for (k, v) in &c.env_overrides {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Make the child immune to clank's SIGINT — start a new
    // session so it doesn't share clank's controlling tty
    // signal target.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn `{}`: {e}", c.program))?;
    Ok(())
}

#[cfg(unix)]
async fn wait_with_sigint_forward(c: ComposedDiffLaunch) -> anyhow::Result<()> {
    use tokio::process::Command as TokioCommand;
    use tokio::signal::unix::{SignalKind, signal};

    let mut cmd = TokioCommand::new(&c.program);
    cmd.args(&c.args);
    for (k, v) in &c.env_overrides {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn `{}`: {e}", c.program))?;
    let child_id = child.id();

    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| anyhow::anyhow!("failed to install SIGINT handler: {e}"))?;

    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status
                    .map_err(|e| anyhow::anyhow!("waiting on `{}`: {e}", c.program))?;
                if !status.success() {
                    let code = status
                        .code()
                        .or_else(|| {
                            use std::os::unix::process::ExitStatusExt;
                            status.signal().map(|s| 128 + s)
                        })
                        .unwrap_or(1);
                    std::process::exit(code);
                }
                return Ok(());
            }
            _ = sigint.recv() => {
                // Forward SIGINT to the child; keep waiting.
                // The editor decides cleanup. Per OQ6 of the
                // plan body.
                if let Some(pid) = child_id {
                    unsafe { libc::kill(pid as i32, libc::SIGINT) };
                }
            }
        }
    }
}

#[cfg(not(unix))]
async fn spawn(c: ComposedDiffLaunch) -> anyhow::Result<()> {
    // Non-unix fallback: no signal-forwarding semantics. Windows'
    // SIGINT story is different (CTRL_C events vs unix signals);
    // we get the default behavior for now and can revisit if a
    // Windows user reports breakage.
    let mut cmd = Command::new(&c.program);
    cmd.args(&c.args);
    for (k, v) in &c.env_overrides {
        cmd.env(k, v);
    }
    if c.wait {
        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("failed to spawn `{}`: {e}", c.program))?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
        return Ok(());
    }
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn `{}`: {e}", c.program))?;
    Ok(())
}
