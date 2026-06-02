//! `clank open <path>` — read-only inspector. Classifies the
//! input path so an editor can decide how to bootstrap a clank
//! session for it. Never mutates.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::OpenArgs;
use clank_core::agent_config::AgentConfig;
use clank_core::vocab::{Role, Tool};

pub async fn run(args: OpenArgs) -> anyhow::Result<()> {
    let response = inspect(&args.path).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        print_human(&response);
    }
    Ok(())
}

#[derive(Serialize)]
pub struct OpenResponse {
    pub requested_path: String,
    pub opened_path: String,
    pub state: OpenState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clank: Option<ClankInfo>,
    pub recommendations: Vec<Recommendation>,
    /// InitGaps `clank init` would actually fix on a clean
    /// re-run. Omitted when state is `clank_ready` (or pre-git
    /// states where init isn't applicable).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub init_gaps: Vec<InitGap>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenState {
    PathMissing,
    PathNotDirectory,
    EmptyDirectory,
    DirectoryNotGit,
    /// Repo is in a git tree but `clank init` has something to
    /// do (creating the dir, installing the hook, patching
    /// permissions, etc.). Details in `init_gaps`.
    ClankInitNeeded,
    /// `.clank/` and every other artifact `clank init` manages
    /// are present and current. The repo can be opened without
    /// running init.
    ClankReady,
}

/// One thing `clank init` would fix on a clean re-run.
#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InitGap {
    MissingClankDir,
    /// `.clank/.gitignore` is absent or matches a known legacy
    /// body that init silently upgrades.
    MissingClankGitignore,
    /// `.claude/settings.local.json` is absent or its
    /// `permissions.allow` array doesn't include one or more
    /// of clank's rules.
    MissingClaudePermissions,
    /// `hooks/post-rewrite` is absent, or it carries the clank
    /// marker but its body has drifted from the canonical
    /// `POST_REWRITE_BODY`.
    MissingPostRewriteHook,
}

impl InitGap {
    pub fn kind_str(&self) -> &'static str {
        match self {
            InitGap::MissingClankDir => "missing_clank_dir",
            InitGap::MissingClankGitignore => "missing_clank_gitignore",
            InitGap::MissingClaudePermissions => "missing_claude_permissions",
            InitGap::MissingPostRewriteHook => "missing_post_rewrite_hook",
        }
    }
}

#[derive(Serialize)]
pub struct GitInfo {
    pub is_repo: bool,
    pub git_dir: String,
    pub is_linked_worktree: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    pub detached_head: bool,
    pub dirty: bool,
}

#[derive(Serialize)]
pub struct ClankInfo {
    pub master_agents: Vec<String>,
    pub agents: Vec<AgentInfo>,
    pub active_plans: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
}

#[derive(Serialize)]
pub struct AgentInfo {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_session_id: Option<String>,
    pub session_resumable: bool,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Recommendation {
    InitDirectory {
        path: String,
    },
    GitInit {
        cwd: String,
    },
    ClankInit {
        cwd: String,
        /// Snake-case kinds for the gaps init will fix at this
        /// path. Matches the top-level `init_gaps` array in
        /// content + order.
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        gaps: Vec<String>,
    },
    BindAgent {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    ResumeAgent {
        label: String,
        tool: String,
        session_id: String,
        command_hint: String,
    },
}

async fn inspect(requested: &str) -> anyhow::Result<OpenResponse> {
    let mut warnings = Vec::<String>::new();

    let exists = std::fs::exists(requested)
        .map_err(|e| anyhow::anyhow!("probing `{requested}` failed: {e}"))?;

    if !exists {
        let opened = lex_absolute(requested);
        return Ok(OpenResponse {
            requested_path: requested.to_string(),
            opened_path: opened.to_string_lossy().to_string(),
            state: OpenState::PathMissing,
            repo_root: None,
            git: None,
            clank: None,
            recommendations: vec![
                Recommendation::InitDirectory {
                    path: opened.to_string_lossy().to_string(),
                },
                Recommendation::GitInit {
                    cwd: opened.to_string_lossy().to_string(),
                },
                Recommendation::ClankInit {
                    cwd: opened.to_string_lossy().to_string(),
                    gaps: Vec::new(),
                },
                Recommendation::BindAgent {
                    label: None,
                    tool: None,
                },
            ],
            init_gaps: Vec::new(),
            warnings,
        });
    }

    let meta = std::fs::metadata(requested)
        .map_err(|e| anyhow::anyhow!("stat `{requested}` failed: {e}"))?;

    if !meta.is_dir() {
        let opened = lex_absolute(requested);
        return Ok(OpenResponse {
            requested_path: requested.to_string(),
            opened_path: opened.to_string_lossy().to_string(),
            state: OpenState::PathNotDirectory,
            repo_root: None,
            git: None,
            clank: None,
            recommendations: vec![],
            init_gaps: Vec::new(),
            warnings,
        });
    }

    let opened = match dunce::canonicalize(requested) {
        Ok(p) => p,
        Err(e) => {
            warnings.push(format!("canonicalize failed: {e}"));
            lex_absolute(requested)
        }
    };

    let git_probe = probe_git(&opened);

    let Some(git_probe) = git_probe else {
        let is_empty = directory_is_empty(&opened);
        let state = if is_empty {
            OpenState::EmptyDirectory
        } else {
            OpenState::DirectoryNotGit
        };
        let cwd = opened.to_string_lossy().to_string();
        return Ok(OpenResponse {
            requested_path: requested.to_string(),
            opened_path: cwd.clone(),
            state,
            repo_root: None,
            git: None,
            clank: None,
            recommendations: vec![
                Recommendation::GitInit { cwd: cwd.clone() },
                Recommendation::ClankInit {
                    cwd: cwd.clone(),
                    gaps: Vec::new(),
                },
                Recommendation::BindAgent {
                    label: None,
                    tool: None,
                },
            ],
            init_gaps: Vec::new(),
            warnings,
        });
    };

    let repo_root = git_probe.repo_root.clone();
    let is_linked_worktree = !git_probe.git_dir.starts_with(&repo_root);
    let (head_branch, detached_head) = head_branch_info(&opened);
    let dirty = worktree_dirty(&opened);
    let git = GitInfo {
        is_repo: true,
        git_dir: git_probe.git_dir.to_string_lossy().to_string(),
        is_linked_worktree,
        head_branch,
        detached_head,
        dirty,
    };

    // Probe each artifact `clank init` manages. Build init_gaps
    // (the cases init repairs on a clean re-run) and drift
    // warnings (foreign content init won't fix without
    // --force-hooks or manual cleanup) from the same shared
    // classifiers in `crate::init_facts`.
    let (init_gaps, drift_warnings) = probe_init_state(&repo_root);
    for w in drift_warnings {
        warnings.push(w);
    }

    let cwd = repo_root.to_string_lossy().to_string();

    // ClankInfo + agent recommendations only when .clank/ is
    // present. When the dir is missing the fold has nothing
    // to look at; skip cleanly.
    let clank_dir_present = repo_root.join(".clank").is_dir();
    let (clank, mut agent_recs) = if clank_dir_present {
        let (info, recs, fold_warning) = clank_info_for_repo(&repo_root).await;
        if let Some(w) = fold_warning {
            warnings.push(w);
        }
        // A fully-init'd repo with no on-disk agents still
        // wants the editor to bind one — emit a generic
        // BindAgent recommendation in that case.
        let recs = if info.agents.is_empty() && recs.is_empty() {
            vec![Recommendation::BindAgent {
                label: None,
                tool: None,
            }]
        } else {
            recs
        };
        (Some(info), recs)
    } else {
        (
            None,
            vec![Recommendation::BindAgent {
                label: None,
                tool: None,
            }],
        )
    };

    // Root / ancestor gitignore advisory. Mirrors what
    // `init.rs::warn_if_globally_excluded` checks — init only
    // warns about these and doesn't fix them, so they're
    // surfaced as `warnings` here, not as InitGaps.
    warnings.extend(probe_ancestor_gitignore_advisory(&repo_root));

    let (state, mut recommendations) = if init_gaps.is_empty() {
        (OpenState::ClankReady, Vec::new())
    } else {
        let gap_kinds: Vec<String> = init_gaps.iter().map(|g| g.kind_str().to_string()).collect();
        (
            OpenState::ClankInitNeeded,
            vec![Recommendation::ClankInit {
                cwd: cwd.clone(),
                gaps: gap_kinds,
            }],
        )
    };
    recommendations.append(&mut agent_recs);

    Ok(OpenResponse {
        requested_path: requested.to_string(),
        opened_path: opened.to_string_lossy().to_string(),
        state,
        repo_root: Some(cwd),
        git: Some(git),
        clank,
        recommendations,
        init_gaps,
        warnings,
    })
}

/// Probe every artifact `clank init` manages and return the
/// gaps + drift warnings.
fn probe_init_state(repo: &Path) -> (Vec<InitGap>, Vec<String>) {
    use crate::init_facts::{
        ClaudePermsState, GitignoreState, HookState, clank_gitignore_path,
        classify_clank_gitignore, classify_claude_perms, classify_post_rewrite_hook,
        claude_perms_path, post_rewrite_hook_path,
    };
    let mut gaps = Vec::new();
    let mut warnings = Vec::new();

    if !repo.join(".clank").is_dir() {
        gaps.push(InitGap::MissingClankDir);
    }

    match classify_clank_gitignore(repo) {
        GitignoreState::Missing | GitignoreState::Legacy => {
            gaps.push(InitGap::MissingClankGitignore);
        }
        GitignoreState::Drifted => {
            warnings.push(format!(
                "`{}` has drifted from the canonical body; \
                 clank init will bail until the file is removed \
                 or matches the canonical content.",
                clank_gitignore_path(repo).display()
            ));
        }
        GitignoreState::Canonical => {}
    }

    match classify_claude_perms(repo) {
        ClaudePermsState::Missing | ClaudePermsState::NeedsPatch { .. } => {
            gaps.push(InitGap::MissingClaudePermissions);
        }
        ClaudePermsState::Drifted => {
            warnings.push(format!(
                "`{}` isn't valid JSON / lacks `permissions.allow`; \
                 clank init won't repair this. Fix the file by hand \
                 or delete it.",
                claude_perms_path(repo).display()
            ));
        }
        ClaudePermsState::Complete => {}
    }

    match classify_post_rewrite_hook(repo) {
        HookState::Missing | HookState::Refreshable => {
            gaps.push(InitGap::MissingPostRewriteHook);
        }
        HookState::Foreign => {
            let path = post_rewrite_hook_path(repo)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".to_string());
            warnings.push(format!(
                "`{path}` is a foreign post-rewrite hook (no clank marker). \
                 clank init leaves it alone. \
                 Pass `clank init --force-hooks` to overwrite, \
                 or chain `clank rewire --from-stdin` into it manually."
            ));
        }
        HookState::Canonical | HookState::Indeterminate => {}
    }

    (gaps, warnings)
}

/// Probe whether an ancestor `.gitignore` (or `core.excludesFile`)
/// excludes any `.clank/` subpath that should be tracked. Mirrors
/// `init.rs::warn_if_globally_excluded` — init only WARNS about
/// these and doesn't write them, so they're advisories, not
/// InitGaps.
fn probe_ancestor_gitignore_advisory(repo: &Path) -> Vec<String> {
    const TRACKED_PROBES: &[&str] = &[".clank/plans", ".clank/finished"];
    let mut out = Vec::new();
    for rel in TRACKED_PROBES {
        let probe = repo.join(rel);
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["check-ignore", "-v"])
            .arg(probe.as_path())
            .output();
        let Ok(output) = output else { continue };
        if output.status.code() != Some(0) {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout.lines().next().unwrap_or("").trim_end();
        if line.is_empty() {
            continue;
        }
        // Suppress hits inside our own .clank/.gitignore — those
        // are intentional. `check-ignore -v` format:
        //   <source_file>:<line>:<pattern>\t<probed>
        let source = line.split_once('\t').map(|(s, _)| s).unwrap_or("");
        let source_file = source.split(':').next().unwrap_or("");
        let sp = Path::new(source_file);
        let is_managed = sp.file_name().is_some_and(|n| n == ".gitignore")
            && sp.parent().is_some_and(|p| p.ends_with(".clank"));
        if is_managed {
            continue;
        }
        out.push(format!(
            "ancestor .gitignore (or core.excludesFile) excludes `{rel}` — \
             tracked clank files would be hidden. Source: {line}. \
             `clank init` only warns about this; fix by editing the offending \
             gitignore yourself."
        ));
    }
    out
}

fn lex_absolute(p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        return collapse_dots(&path);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    collapse_dots(&cwd.join(path))
}

fn collapse_dots(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

struct GitProbe {
    repo_root: PathBuf,
    git_dir: PathBuf,
}

fn probe_git(path: &Path) -> Option<GitProbe> {
    let toplevel_out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !toplevel_out.status.success() {
        return None;
    }
    let raw_top = String::from_utf8_lossy(&toplevel_out.stdout)
        .trim()
        .to_string();
    if raw_top.is_empty() {
        return None;
    }
    let repo_root = dunce::canonicalize(&raw_top).unwrap_or_else(|_| PathBuf::from(&raw_top));

    let gitdir_out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if !gitdir_out.status.success() {
        return None;
    }
    let raw_git = String::from_utf8_lossy(&gitdir_out.stdout)
        .trim()
        .to_string();
    if raw_git.is_empty() {
        return None;
    }
    let raw_path = PathBuf::from(&raw_git);
    let absolute = if raw_path.is_absolute() {
        raw_path
    } else {
        path.join(raw_path)
    };
    let git_dir = dunce::canonicalize(&absolute).unwrap_or(absolute);
    Some(GitProbe { repo_root, git_dir })
}

fn head_branch_info(path: &Path) -> (Option<String>, bool) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return (Some(s), false);
            }
        }
    }
    (None, true)
}

fn worktree_dirty(path: &Path) -> bool {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output();
    match out {
        Ok(o) => o.status.success() && !o.stdout.is_empty(),
        Err(_) => false,
    }
}

fn directory_is_empty(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == ".DS_Store" {
            continue;
        }
        return false;
    }
    true
}

async fn clank_info_for_repo(repo_root: &Path) -> (ClankInfo, Vec<Recommendation>, Option<String>) {
    let agent_configs =
        crate::agent_store::load_all_agent_configs_lossy(repo_root).unwrap_or_default();

    let mut master_agents = Vec::<String>::new();
    let mut agents = Vec::<AgentInfo>::new();
    let mut recommendations = Vec::<Recommendation>::new();
    for (label, cfg) in &agent_configs {
        if cfg.role == Role::Master {
            master_agents.push(label.as_str().to_string());
        }
        let (tool_str, session_id_str, resumable) = agent_session_info(cfg);
        agents.push(AgentInfo {
            label: label.as_str().to_string(),
            tool: tool_str.clone(),
            last_session_id: session_id_str.clone(),
            session_resumable: resumable,
        });
        if resumable {
            let tool = tool_str.clone().unwrap_or_default();
            let session_id = session_id_str.clone().unwrap_or_default();
            let command_hint = match tool.as_str() {
                "claude" => format!("claude --resume {session_id}"),
                "codex" => format!("codex resume {session_id}"),
                _ => format!("# unknown tool `{tool}`"),
            };
            recommendations.push(Recommendation::ResumeAgent {
                label: label.as_str().to_string(),
                tool,
                session_id,
                command_hint,
            });
        } else {
            recommendations.push(Recommendation::BindAgent {
                label: Some(label.as_str().to_string()),
                tool: tool_str,
            });
        }
    }

    let (active_plans, waiting_on, fold_warning) = fold_summary(repo_root).await;

    (
        ClankInfo {
            master_agents,
            agents,
            active_plans,
            waiting_on,
        },
        recommendations,
        fold_warning,
    )
}

fn agent_session_info(cfg: &AgentConfig) -> (Option<String>, Option<String>, bool) {
    let Some(session) = cfg.session.as_ref() else {
        return (None, None, false);
    };
    let tool_str = match session.tool {
        Tool::Claude => "claude",
        Tool::Codex => "codex",
    }
    .to_string();
    let session_id = session.id.as_str().to_string();
    let resumable = session_jsonl_exists(&session.tool, &session_id);
    (Some(tool_str), Some(session_id), resumable)
}

fn session_jsonl_exists(tool: &Tool, session_id: &str) -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let home = PathBuf::from(home);
    match tool {
        Tool::Claude => claude_session_jsonl_exists(&home, session_id),
        Tool::Codex => codex_session_jsonl_exists(&home, session_id),
    }
}

fn claude_session_jsonl_exists(home: &Path, session_id: &str) -> bool {
    let projects = home.join(".claude").join("projects");
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return false;
    };
    let file_name = format!("{session_id}.jsonl");
    for entry in entries.flatten() {
        if entry.path().join(&file_name).is_file() {
            return true;
        }
    }
    false
}

fn codex_session_jsonl_exists(home: &Path, session_id: &str) -> bool {
    let root = home.join(".codex").join("sessions");
    walk_for_session(&root, session_id, 4)
}

fn walk_for_session(dir: &Path, session_id: &str, depth: usize) -> bool {
    if depth == 0 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if walk_for_session(&path, session_id, depth - 1) {
                return true;
            }
        } else if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name.starts_with("rollout-") && name.ends_with(".jsonl") && name.contains(session_id)
            {
                return true;
            }
        }
    }
    false
}

async fn fold_summary(repo_root: &Path) -> (usize, Option<String>, Option<String>) {
    let state =
        match crate::rebuild::rebuild_repo_with_policy(repo_root, crate::rebuild::CachePolicy::Use)
            .await
        {
            Ok(s) => s,
            Err(e) => return (0, None, Some(format!("fold failed: {e}"))),
        };
    let config = crate::cli::config::load(repo_root);
    let work_policy = clank_core::wait::WorkPolicy {
        plan_feedback: config.review.plan_feedback,
        adhoc_feedback: config.review.adhoc_feedback,
    };
    let reviews = crate::fs_review_lookup::FsReviewLookup::new(repo_root, state.head.as_ref());
    let work_status = state.fold.derive_status(&reviews, &work_policy);
    let active_plans = work_status.plans.len();
    let waiting_on = if work_status.plans.len() == 1 {
        Some(format_waiting_on(&work_status.plans[0].waiting_on))
    } else if work_status.plans.is_empty() {
        None
    } else {
        Some("multiple plans".to_string())
    };
    (active_plans, waiting_on, None)
}

fn format_waiting_on(w: &clank_core::plan_view::WaitingOn) -> String {
    use clank_core::plan_view::WaitingOn::*;
    match w {
        FirstReview => "first review".to_string(),
        ReviewerApprovalsMissing { .. } => "reviewers".to_string(),
        MasterToRevise { .. } => "master to revise".to_string(),
        MasterToContinue => "master to continue".to_string(),
        MasterToFinalize => "master to finalize".to_string(),
        MasterToCommit => "master to commit".to_string(),
    }
}

fn print_human(r: &OpenResponse) {
    let state_line = if r.init_gaps.is_empty() {
        state_label(&r.state).to_string()
    } else {
        format!(
            "{} ({} gap{})",
            state_label(&r.state),
            r.init_gaps.len(),
            if r.init_gaps.len() == 1 { "" } else { "s" }
        )
    };
    println!("state:        {state_line}");
    if !r.init_gaps.is_empty() {
        for gap in &r.init_gaps {
            println!("              - {}", gap.kind_str());
        }
    }
    println!("opened_path:  {}", r.opened_path);
    println!("repo_root:    {}", r.repo_root.as_deref().unwrap_or("-"),);
    if let Some(g) = &r.git {
        let branch = g.head_branch.as_deref().unwrap_or("(detached)");
        let clean = if g.dirty { "dirty" } else { "clean" };
        let wt = if g.is_linked_worktree {
            ", linked worktree"
        } else {
            ""
        };
        println!("git:          branch={branch}, {clean}{wt}");
    } else {
        println!("git:          -");
    }
    if let Some(c) = &r.clank {
        let master = if c.master_agents.is_empty() {
            "none".to_string()
        } else {
            format!("[{}]", c.master_agents.join(", "))
        };
        let plans = if c.active_plans == 1 {
            "1 active plan".to_string()
        } else {
            format!("{} active plans", c.active_plans)
        };
        let waiting = c
            .waiting_on
            .as_deref()
            .map(|w| format!(", waiting on {w}"))
            .unwrap_or_default();
        println!(
            "clank:        master={master}, {} agents, {plans}{waiting}",
            c.agents.len()
        );
    } else {
        println!("clank:        -");
    }
    if !r.recommendations.is_empty() {
        println!();
        println!("recommendations:");
        for rec in &r.recommendations {
            println!("  - {}", rec_label(rec));
        }
    }
    if !r.warnings.is_empty() {
        println!();
        println!("warnings:");
        for w in &r.warnings {
            println!("  - {w}");
        }
    }
}

fn state_label(s: &OpenState) -> &'static str {
    match s {
        OpenState::PathMissing => "PathMissing",
        OpenState::PathNotDirectory => "PathNotDirectory",
        OpenState::EmptyDirectory => "EmptyDirectory",
        OpenState::DirectoryNotGit => "DirectoryNotGit",
        OpenState::ClankInitNeeded => "ClankInitNeeded",
        OpenState::ClankReady => "ClankReady",
    }
}

fn rec_label(r: &Recommendation) -> String {
    match r {
        Recommendation::InitDirectory { path } => format!("create directory `{path}`"),
        Recommendation::GitInit { cwd } => format!("git init in `{cwd}`"),
        Recommendation::ClankInit { cwd, gaps } => {
            if gaps.is_empty() {
                format!("clank init in `{cwd}`")
            } else {
                format!(
                    "clank init in `{cwd}` ({} gap{})",
                    gaps.len(),
                    if gaps.len() == 1 { "" } else { "s" }
                )
            }
        }
        Recommendation::BindAgent { label, tool } => {
            let l = label.as_deref().unwrap_or("<new>");
            let t = tool.as_deref().unwrap_or("<choose>");
            format!("bind agent `{l}` (tool: {t})")
        }
        Recommendation::ResumeAgent {
            label,
            command_hint,
            ..
        } => format!("resume agent `{label}` ({command_hint})"),
    }
}
