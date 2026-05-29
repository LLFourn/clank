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
    GitWithoutClank,
    ClankInitialized,
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
                },
                Recommendation::BindAgent {
                    label: None,
                    tool: None,
                },
            ],
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
                Recommendation::ClankInit { cwd: cwd.clone() },
                Recommendation::BindAgent {
                    label: None,
                    tool: None,
                },
            ],
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

    let clank_config_path = repo_root.join(".clank/config.json");
    if !clank_config_path.is_file() {
        let cwd = repo_root.to_string_lossy().to_string();
        return Ok(OpenResponse {
            requested_path: requested.to_string(),
            opened_path: opened.to_string_lossy().to_string(),
            state: OpenState::GitWithoutClank,
            repo_root: Some(cwd.clone()),
            git: Some(git),
            clank: None,
            recommendations: vec![
                Recommendation::ClankInit { cwd: cwd.clone() },
                Recommendation::BindAgent {
                    label: None,
                    tool: None,
                },
            ],
            warnings,
        });
    }

    let (clank_info, agent_recs, fold_warning) =
        clank_info_for_repo(&repo_root).await;
    if let Some(w) = fold_warning {
        warnings.push(w);
    }

    Ok(OpenResponse {
        requested_path: requested.to_string(),
        opened_path: opened.to_string_lossy().to_string(),
        state: OpenState::ClankInitialized,
        repo_root: Some(repo_root.to_string_lossy().to_string()),
        git: Some(git),
        clank: Some(clank_info),
        recommendations: agent_recs,
        warnings,
    })
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
    let agent_configs = crate::agent_store::load_all_agent_configs_lossy(repo_root)
        .unwrap_or_default();

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
            if name.starts_with("rollout-")
                && name.ends_with(".jsonl")
                && name.contains(session_id)
            {
                return true;
            }
        }
    }
    false
}

async fn fold_summary(repo_root: &Path) -> (usize, Option<String>, Option<String>) {
    let state = match crate::rebuild::rebuild_repo_with_policy(
        repo_root,
        crate::rebuild::CachePolicy::Use,
    )
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
        MasterToImplement => "master to implement".to_string(),
        MasterToFinalize => "master to finalize".to_string(),
        MasterToCommit => "master to commit".to_string(),
    }
}

fn print_human(r: &OpenResponse) {
    println!("state:        {}", state_label(&r.state));
    println!("opened_path:  {}", r.opened_path);
    println!(
        "repo_root:    {}",
        r.repo_root.as_deref().unwrap_or("-"),
    );
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
        OpenState::GitWithoutClank => "GitWithoutClank",
        OpenState::ClankInitialized => "ClankInitialized",
    }
}

fn rec_label(r: &Recommendation) -> String {
    match r {
        Recommendation::InitDirectory { path } => format!("create directory `{path}`"),
        Recommendation::GitInit { cwd } => format!("git init in `{cwd}`"),
        Recommendation::ClankInit { cwd } => format!("clank init in `{cwd}`"),
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
