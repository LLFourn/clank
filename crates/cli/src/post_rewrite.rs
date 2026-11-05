//! The `post-rewrite` git hook that carries review feedback through a
//! rebase or an amend: what clank writes there, who owns the slot git
//! runs, whether rewire is wired into it, and the install `clank init`
//! performs.
//!
//! Ownership ([`SlotState`]) decides what `init` may write. Wiring
//! ([`Wiring`]) is a separate fact: a hook clank does not own can still
//! run rewire, and a hook clank wrote can still be one git never runs.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::Context;

pub const HOOK: &str = "post-rewrite";
/// Where a chained foreign hook is kept, beside the wrapper that runs it.
pub const CHAINED: &str = "post-rewrite.clank-chained";
const LOCK: &str = ".clank-hook.lock";

pub const POST_REWRITE_MARKER: &str = "# clank rewire hook";

/// Canonical body of the hook clank installs into an empty slot.
pub const POST_REWRITE_BODY: &str = "#!/usr/bin/env sh\n\
# clank rewire hook\n\
exec clank rewire --from-stdin\n";

const WRAPPER_MARKER: &str = "# clank rewire chaining wrapper";

/// The hook clank puts in front of a foreign one. Git's stdin can be read
/// only once, so it is spooled to a file both consumers read in full.
/// git ignores a post-rewrite hook's status, so a failed rewire must not
/// stop the chained hook, whose status is the wrapper's.
pub const WRAPPER_BODY: &str = r#"#!/bin/sh
# clank rewire chaining wrapper
# The hook this replaced is post-rewrite.clank-chained; it runs after rewire.
chained="$(dirname "$0")/post-rewrite.clank-chained"
clank_in=$(mktemp 2>/dev/null) || {
  echo "clank: no temporary file, so rewire was skipped" >&2
  exec "$chained" "$@"
}
trap 'rm -f "$clank_in"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
cat > "$clank_in"
if command -v clank >/dev/null 2>&1; then
  clank rewire --from-stdin < "$clank_in" ||
    echo "clank: rewire failed; review feedback may not follow this rewrite" >&2
else
  echo "clank: not on PATH, so review feedback will not follow this rewrite" >&2
fi
"$chained" "$@" < "$clank_in"
"#;

/// The line that marks a hook clank does not own as running rewire.
pub const DELEGATED_MARKER: &str = "# clank rewire (delegated)";
pub const REWIRE_COMMAND: &str = "clank rewire --from-stdin";

/// What a hook clank does not own adds at its top to delegate to rewire.
/// It replays stdin, so the hook's own consumers still read git's stream.
pub const SHELL_STANZA: &str = r#"# clank rewire (delegated)
clank_in=$(mktemp); cat > "$clank_in"
clank rewire --from-stdin < "$clank_in" || echo "clank rewire failed" >&2
exec < "$clank_in"; rm -f "$clank_in"
"#;

pub const LEFTHOOK_RECIPE: &str = "post-rewrite:
  commands:
    clank-rewire:
      run: clank rewire --from-stdin
      use_stdin: true
";

/// The lefthook configs clank reads.
const LEFTHOOK_READABLE: &[&str] = &["lefthook.yml", ".lefthook.yml"];
/// Configs lefthook also loads, which clank does not read.
const LEFTHOOK_UNREAD: &[&str] = &[
    "lefthook.yaml",
    ".lefthook.yaml",
    "lefthook.toml",
    ".lefthook.toml",
    "lefthook.json",
    ".lefthook.json",
    ".config/lefthook.yml",
    ".config/lefthook.yaml",
];
/// Overrides lefthook merges into the main config.
const LEFTHOOK_LOCAL: &[&str] = &[
    "lefthook-local.yml",
    ".lefthook-local.yml",
    "lefthook-local.yaml",
    ".lefthook-local.yaml",
    "lefthook-local.toml",
    ".lefthook-local.toml",
    "lefthook-local.json",
    ".lefthook-local.json",
];
/// Top-level keys that pull `post-rewrite` entries in from elsewhere.
const LEFTHOOK_COMPOSING: &[&str] = &["extends", "remote", "remotes"];
const LEFTHOOK_HOOK_SCALARS: &[&str] = &[
    "parallel",
    "piped",
    "follow",
    "fail_on_changes",
    "exclude_tags",
    "skip",
    "only",
];
const LEFTHOOK_ENTRY_KEYS: &[&str] = &[
    "run",
    "use_stdin",
    "name",
    "tags",
    "glob",
    "files",
    "root",
    "skip",
    "only",
    "exclude",
    "fail_text",
    "stage_fixed",
    "interactive",
    "priority",
    "env",
    "runner",
    "timeout",
    "file_types",
    "exclude_tags",
];

/// Where git looks for hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HooksDir {
    /// `core.hooksPath = /dev/null`.
    Off,
    At(PathBuf),
}

/// The `post-rewrite` hook git would run for one worktree, and who owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSlot {
    /// The worktree's top level, where git runs hooks.
    pub top: PathBuf,
    /// The effective hooks directory; `/dev/null` when hooks are off.
    pub dir: PathBuf,
    pub state: SlotState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    Missing,
    Disabled(Disabled),
    /// A symlink, typically a hook manager's or a dotfiles setup's.
    Linked {
        target: PathBuf,
    },
    /// A regular executable whose dispatch clank must not disturb.
    Managed(Manager),
    /// `current` is false for an older clank body.
    Clank {
        current: bool,
    },
    /// clank's wrapper in front of [`CHAINED`].
    ClankChaining {
        current: bool,
    },
    Foreign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disabled {
    HooksOff,
    /// Git skips a hook it cannot execute.
    NotExecutable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Husky,
    Lefthook,
    PreCommit,
    /// Dispatches on its own name, so renaming it changes what it runs.
    NameDispatch,
}

impl Manager {
    pub fn name(self) -> &'static str {
        match self {
            Manager::Husky => "husky",
            Manager::Lefthook => "lefthook",
            Manager::PreCommit => "pre-commit",
            Manager::NameDispatch => "a hook that dispatches on its own name",
        }
    }
}

impl HookSlot {
    pub fn path(&self) -> PathBuf {
        self.dir.join(HOOK)
    }

    pub fn chained_path(&self) -> PathBuf {
        self.dir.join(CHAINED)
    }
}

pub fn classify(top: PathBuf, dir: HooksDir) -> HookSlot {
    let HooksDir::At(dir) = dir else {
        return HookSlot {
            top,
            dir: PathBuf::from("/dev/null"),
            state: SlotState::Disabled(Disabled::HooksOff),
        };
    };
    let state = state_of(&dir);
    HookSlot { top, dir, state }
}

fn state_of(dir: &Path) -> SlotState {
    let path = dir.join(HOOK);
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SlotState::Missing,
        // Installing then fails with the real error rather than guessing.
        Err(_) => return SlotState::Foreign,
    };
    if meta.file_type().is_symlink() {
        return SlotState::Linked {
            target: std::fs::read_link(&path).unwrap_or_default(),
        };
    }
    if !runnable(&path) {
        return SlotState::Disabled(Disabled::NotExecutable);
    }
    let body = std::fs::read(&path).unwrap_or_default();
    let text = String::from_utf8_lossy(&body);
    if text.contains(WRAPPER_MARKER) {
        return SlotState::ClankChaining {
            current: body == WRAPPER_BODY.as_bytes(),
        };
    }
    if text.contains(POST_REWRITE_MARKER) {
        return SlotState::Clank {
            current: body == POST_REWRITE_BODY.as_bytes(),
        };
    }
    match manager(dir, &text) {
        Some(manager) => SlotState::Managed(manager),
        None => SlotState::Foreign,
    }
}

fn manager(dir: &Path, text: &str) -> Option<Manager> {
    let lower = text.to_ascii_lowercase();
    if is_husky_dir(dir) || lower.contains("husky") {
        Some(Manager::Husky)
    } else if lower.contains("lefthook") {
        Some(Manager::Lefthook)
    } else if lower.contains("generated by pre-commit") {
        Some(Manager::PreCommit)
    } else if dispatches_on_name(text) {
        Some(Manager::NameDispatch)
    } else {
        None
    }
}

/// husky (v9) points `core.hooksPath` at `.husky/_` and runs the user's
/// `.husky/<hook>` from there.
fn is_husky_dir(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == "_")
        && dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == ".husky")
}

/// Whether the script derives anything from its own name. Only the
/// forms that yield its directory alone survive a rename in that
/// directory; any other use of `$0` counts, since chaining it would
/// silently change what it runs.
fn dispatches_on_name(text: &str) -> bool {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| {
            line.match_indices("$0")
                .chain(line.match_indices("${0"))
                .any(|(at, _)| !yields_only_the_directory(&line[..at], &line[at..]))
        })
}

fn yields_only_the_directory(before: &str, from: &str) -> bool {
    if from.starts_with("${0%/*}") {
        return true;
    }
    let after_dollar_zero = from.strip_prefix("$0").unwrap_or_default();
    let closed =
        after_dollar_zero.is_empty() || after_dollar_zero.starts_with(['"', ')', ' ', '`', '\t']);
    let before = before.trim_end_matches(['"', ' ', '\t']);
    let before = before.strip_suffix("--").unwrap_or(before).trim_end();
    closed && from.starts_with("$0") && before.ends_with("dirname")
}

/// Whether rewire runs when git runs this slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wiring {
    /// clank's own hook or wrapper, in the slot git executes.
    Wired,
    /// The slot's owner runs clank's stanza, in this file.
    Delegated(PathBuf),
    Unwired(Recipe),
    /// Git runs no hook here.
    Off(Disabled),
}

/// What makes an unwired slot run rewire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recipe {
    /// The slot is empty or plainly foreign: `clank init` installs or chains.
    RunInit,
    /// Put [`SHELL_STANZA`] at the top of this file.
    Shell(PathBuf),
    /// Add [`LEFTHOOK_RECIPE`] to this config (none exists yet when `None`).
    Lefthook(Option<PathBuf>),
    /// Lefthook gives stdin to one `use_stdin` command only, and these
    /// already take it.
    LefthookContended {
        config: PathBuf,
        competitors: Vec<String>,
    },
    /// The set of `use_stdin` commands could not be established, so
    /// whether rewire is the only one is unknown.
    LefthookUndetermined { config: PathBuf, why: String },
    /// pre-commit has no supported way to share the hook's stdin.
    PreCommit,
}

pub fn wiring(slot: &HookSlot) -> Wiring {
    match &slot.state {
        SlotState::Clank { .. } | SlotState::ClankChaining { .. } => Wiring::Wired,
        SlotState::Disabled(why) => Wiring::Off(*why),
        SlotState::Missing => Wiring::Unwired(Recipe::RunInit),
        SlotState::Foreign if delegates(&slot.path()) => Wiring::Delegated(slot.path()),
        SlotState::Foreign => Wiring::Unwired(Recipe::RunInit),
        SlotState::Linked { .. } => match std::fs::canonicalize(slot.path()) {
            Ok(target) if runnable(&target) => owner_wiring(slot, &target, None),
            _ => Wiring::Off(Disabled::NotExecutable),
        },
        SlotState::Managed(manager) => owner_wiring(slot, &slot.path(), Some(*manager)),
    }
}

/// Whether git would run `path`: a regular file, after following
/// symlinks, that this user may execute. Git's own test is
/// `access(X_OK)`, which an execute bit for some other user does not pass.
pub fn runnable(path: &Path) -> bool {
    let executable = rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok();
    executable && std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// Delegation is looked for only where the owner runs it from: the file
/// git executes, husky's user script, or lefthook's config.
fn owner_wiring(slot: &HookSlot, executed: &Path, manager: Option<Manager>) -> Wiring {
    if delegates(executed) {
        return Wiring::Delegated(executed.to_path_buf());
    }
    if is_husky_dir(&slot.dir) {
        let script = slot.dir.parent().unwrap_or(&slot.dir).join(HOOK);
        return if delegates(&script) {
            Wiring::Delegated(script)
        } else {
            Wiring::Unwired(Recipe::Shell(script))
        };
    }
    match manager {
        Some(Manager::Lefthook) => lefthook_wiring(&slot.top),
        Some(Manager::PreCommit) => Wiring::Unwired(Recipe::PreCommit),
        _ => Wiring::Unwired(Recipe::Shell(executed.to_path_buf())),
    }
}

/// Whether the file carries clank's stanza: the marker line and the
/// command it runs.
fn delegates(path: &Path) -> bool {
    let Ok(body) = std::fs::read(path) else {
        return false;
    };
    let text = String::from_utf8_lossy(&body);
    text.lines().any(|l| l.trim() == DELEGATED_MARKER)
        && text.lines().any(|l| l.contains(REWIRE_COMMAND))
}

fn lefthook_wiring(top: &Path) -> Wiring {
    let present = |names: &[&str]| -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| top.join(n))
            .filter(|p| p.symlink_metadata().is_ok())
            .collect()
    };
    let readable = present(LEFTHOOK_READABLE);
    let unread = present(LEFTHOOK_UNREAD);
    let local = present(LEFTHOOK_LOCAL);
    let undetermined = |config: &Path, why: &str| {
        Wiring::Unwired(Recipe::LefthookUndetermined {
            config: config.to_path_buf(),
            why: why.to_string(),
        })
    };
    if let Some(local) = local.first() {
        return undetermined(local, "it overrides the main lefthook config");
    }
    let config = match (readable.as_slice(), unread.as_slice()) {
        ([], []) => return Wiring::Unwired(Recipe::Lefthook(None)),
        ([config], []) => config.clone(),
        ([], [config, ..]) => {
            return undetermined(config, "clank reads only lefthook.yml and .lefthook.yml");
        }
        ([first, ..], _) => {
            return undetermined(first, "lefthook finds more than one config");
        }
    };
    let text = match std::fs::read_to_string(&config) {
        Ok(text) => text,
        Err(e) => return undetermined(&config, &format!("it cannot be read ({e})")),
    };
    let mut consumers = match lefthook_stdin_consumers(&text) {
        Ok(consumers) => consumers,
        Err(why) => return undetermined(&config, &why),
    };
    let clank = consumers.iter().position(|c| c.runs_rewire);
    if let Some(clank) = clank {
        consumers.remove(clank);
    }
    match (clank, consumers.is_empty()) {
        (Some(_), true) => Wiring::Delegated(config),
        (None, true) => Wiring::Unwired(Recipe::Lefthook(Some(config))),
        (_, false) => Wiring::Unwired(Recipe::LefthookContended {
            config,
            competitors: consumers.into_iter().map(|c| c.name).collect(),
        }),
    }
}

/// A `post-rewrite` entry in a lefthook config that sets `use_stdin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdinConsumer {
    pub name: String,
    pub runs_rewire: bool,
}

/// What a line under `post-rewrite:` is, given its parent's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// `commands:` or `scripts:`, a map of named entries.
    Entries,
    /// `jobs:`, a list of entries.
    Jobs,
    HookScalar,
    Entry,
    EntryKey {
        env: bool,
    },
    EnvVar,
}

/// Every entry under the top-level `post-rewrite:` key that sets
/// `use_stdin: true`. `Err` says why the set cannot be established: the
/// section must be entirely in the block form this reads, with only
/// lefthook's known keys, and the config must not compose `post-rewrite`
/// from elsewhere. Anything else could hide another stdin consumer.
pub fn lefthook_stdin_consumers(text: &str) -> Result<Vec<StdinConsumer>, String> {
    let raw: Vec<&str> = text.lines().collect();
    let meaningful = |l: &str| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#')
    };
    let top_level = |l: &str| meaningful(l) && !l.starts_with([' ', '\t']);
    let mut start = None;
    let mut seen_key = false;
    for (n, l) in raw.iter().enumerate().filter(|(_, l)| top_level(l)) {
        if l.trim() == "---" && !seen_key {
            continue;
        }
        let line = strict_line(n, l)?;
        seen_key = true;
        if LEFTHOOK_COMPOSING.contains(&line.key.as_str()) {
            return Err(format!("`{}` composes the config from others", line.key));
        }
        if line.key == "post-rewrite" {
            if start.is_some() {
                return Err("`post-rewrite` appears twice".to_string());
            }
            if !line.value.is_empty() {
                return Err(format!(
                    "line {}: `post-rewrite` is not a block mapping",
                    n + 1
                ));
            }
            start = Some(n);
        }
    }
    let Some(start) = start else {
        return Ok(Vec::new());
    };
    let mut lines = Vec::new();
    for (n, l) in raw.iter().enumerate().skip(start + 1) {
        if top_level(l) {
            break;
        }
        if meaningful(l) {
            lines.push(strict_line(n, l)?);
        }
    }
    let mut roles: Vec<Role> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let parent = lines[..i].iter().rposition(|l| l.indent < line.indent);
        let unexpected = || {
            Err(format!(
                "`{}` is not a post-rewrite key clank reads in that place",
                line.key
            ))
        };
        let empty = line.value.is_empty();
        let role = match parent.map(|p| roles[p]) {
            None if line.item => return unexpected(),
            None if empty && matches!(line.key.as_str(), "commands" | "scripts") => Role::Entries,
            None if empty && line.key == "jobs" => Role::Jobs,
            None if !empty && LEFTHOOK_HOOK_SCALARS.contains(&line.key.as_str()) => {
                Role::HookScalar
            }
            Some(Role::Entries) if !line.item && empty => Role::Entry,
            Some(Role::Jobs)
                if line.item && !empty && LEFTHOOK_ENTRY_KEYS.contains(&line.key.as_str()) =>
            {
                Role::Entry
            }
            Some(Role::Entry) if !line.item && LEFTHOOK_ENTRY_KEYS.contains(&line.key.as_str()) => {
                let env = line.key == "env";
                if env != empty {
                    return unexpected();
                }
                Role::EntryKey { env }
            }
            Some(Role::EntryKey { env: true }) if !line.item && !empty => Role::EnvVar,
            _ => return unexpected(),
        };
        roles.push(role);
    }
    let mut out = Vec::new();
    for (i, opener) in lines.iter().enumerate() {
        if roles[i] != Role::Entry {
            continue;
        }
        // A list item's opening line carries its first field.
        let nested = lines[i + 1..]
            .iter()
            .zip(&roles[i + 1..])
            .take_while(|(l, _)| l.indent > opener.indent)
            .filter(|(_, role)| **role == Role::EntryKey { env: false })
            .map(|(l, _)| l);
        let fields: Vec<&YamlLine> = opener
            .item
            .then_some(opener)
            .into_iter()
            .chain(nested)
            .collect();
        let mut keys = std::collections::HashSet::new();
        if let Some(repeated) = fields.iter().find(|l| !keys.insert(l.key.as_str())) {
            return Err(format!("an entry repeats `{}`", repeated.key));
        }
        let field = |key: &str| {
            fields
                .iter()
                .find(|l| l.key == key)
                .map(|l| l.value.as_str())
        };
        if !field("use_stdin").is_some_and(is_yaml_true) {
            continue;
        }
        let name = if opener.item {
            field("name").unwrap_or("an unnamed job").to_string()
        } else {
            opener.key.clone()
        };
        out.push(StdinConsumer {
            name,
            runs_rewire: field("run") == Some(REWIRE_COMMAND),
        });
    }
    Ok(out)
}

/// `raw` (line `n`, zero-based) as a plain block `key: value`, or why not.
fn strict_line(n: usize, raw: &str) -> Result<YamlLine, String> {
    let leading = &raw[..raw.len() - raw.trim_start().len()];
    if leading.contains('\t') {
        return Err(format!("line {} is indented with a tab", n + 1));
    }
    let line =
        YamlLine::parse(raw).ok_or_else(|| format!("line {} is not a `key: value` line", n + 1))?;
    let special = |s: &str| s.starts_with(['{', '[', '&', '*', '!', '|', '>']);
    if special(&line.key) || line.key.starts_with("<<") || special(&line.value) {
        return Err(format!(
            "line {} uses YAML that clank does not read (flow, anchors, aliases, tags or block scalars)",
            n + 1
        ));
    }
    Ok(line)
}

/// One `key: value` line of block YAML. A list item sits one column
/// right of its dash, so the keys aligned under its first key nest in it.
struct YamlLine {
    indent: usize,
    item: bool,
    key: String,
    value: String,
}

impl YamlLine {
    fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim_start_matches(' ');
        if trimmed.trim().is_empty() || trimmed.starts_with('#') {
            return None;
        }
        let mut indent = raw.len() - trimmed.len();
        let (item, rest) = match trimmed.strip_prefix("- ") {
            Some(rest) => {
                indent += 1;
                (true, rest.trim_start())
            }
            None => (false, trimmed),
        };
        let (key, value) = rest.split_once(':')?;
        let value = match value.find(" #") {
            Some(at) => &value[..at],
            None => value,
        };
        Some(Self {
            indent,
            item,
            key: unquote(key.trim()).to_string(),
            value: unquote(value.trim()).to_string(),
        })
    }
}

fn unquote(s: &str) -> &str {
    ['"', '\'']
        .iter()
        .find_map(|q| s.strip_prefix(*q).and_then(|s| s.strip_suffix(*q)))
        .unwrap_or(s)
}

fn is_yaml_true(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "on")
}

/// What [`install`] did to the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookChange {
    Wrote,
    Refreshed,
    UpToDate,
    Chained,
    /// The slot is not clank's to write.
    Left(Left),
    NoGitDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Left {
    Off(Disabled),
    Linked,
    Managed(Manager),
    /// A foreign hook that already runs clank's stanza.
    Delegated,
}

enum Step {
    Settled(HookChange),
    Install,
    Refresh(&'static str),
    Chain,
}

fn step(slot: &HookSlot) -> anyhow::Result<Step> {
    Ok(match &slot.state {
        SlotState::Missing => Step::Install,
        SlotState::Clank { current: true } => Step::Settled(HookChange::UpToDate),
        SlotState::Clank { current: false } => Step::Refresh(POST_REWRITE_BODY),
        SlotState::ClankChaining { current } => {
            if let Some(defect) = chained_defect(slot)? {
                anyhow::bail!(
                    "{} is clank's chaining wrapper, but the hook it runs, {}, {defect}; \
                     restore it, or delete the wrapper and run `clank init` again",
                    slot.path().display(),
                    slot.chained_path().display()
                );
            }
            match current {
                true => Step::Settled(HookChange::UpToDate),
                false => Step::Refresh(WRAPPER_BODY),
            }
        }
        SlotState::Foreign if delegates(&slot.path()) => {
            Step::Settled(HookChange::Left(Left::Delegated))
        }
        SlotState::Foreign => Step::Chain,
        SlotState::Disabled(why) => Step::Settled(HookChange::Left(Left::Off(*why))),
        SlotState::Linked { .. } => Step::Settled(HookChange::Left(Left::Linked)),
        SlotState::Managed(m) => Step::Settled(HookChange::Left(Left::Managed(*m))),
    })
}

/// Make the slot git runs for `worktree` run rewire, where clank may
/// write it: install into an empty slot, refresh an older clank body,
/// or chain a plain foreign hook behind clank's wrapper. Slots owned by
/// something else are left untouched.
pub fn install(worktree: &Path) -> anyhow::Result<HookChange> {
    let Ok(slot) = crate::git_io::post_rewrite_slot(worktree) else {
        return Ok(HookChange::NoGitDir);
    };
    if let Step::Settled(change) = step(&slot)? {
        return Ok(change);
    }
    std::fs::create_dir_all(&slot.dir)
        .with_context(|| format!("creating hooks dir `{}`", slot.dir.display()))?;
    let _lock = crate::agent_store::exclusive_lock(&slot.dir.join(LOCK))?;
    // Another install may have finished while this one waited.
    let slot = crate::git_io::post_rewrite_slot(worktree)?;
    Ok(match step(&slot)? {
        Step::Settled(change) => change,
        Step::Install => {
            replace(&slot.dir, &slot.path(), POST_REWRITE_BODY)?;
            HookChange::Wrote
        }
        Step::Refresh(body) => {
            replace(&slot.dir, &slot.path(), body)?;
            HookChange::Refreshed
        }
        Step::Chain => {
            chain(&slot)?;
            HookChange::Chained
        }
    })
}

/// Move the foreign hook to [`CHAINED`] and put the wrapper in its
/// place. Until the final rename the original hook stays where git runs
/// it; after it, the wrapper runs the copy.
fn chain(slot: &HookSlot) -> anyhow::Result<()> {
    let hook = slot.path();
    let chained = slot.chained_path();
    let original = std::fs::read(&hook).with_context(|| format!("reading `{}`", hook.display()))?;
    let mode = std::fs::metadata(&hook)?.permissions().mode() & 0o7777;
    let refuse = |why: &str| {
        anyhow::anyhow!(
            "refusing to chain {}: {} already exists and {why}, so nothing was changed; \
             move one of them aside and run `clank init` again",
            hook.display(),
            chained.display()
        )
    };
    match chained_defect(slot)? {
        Some(ChainedDefect::Missing) => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&chained)
                .with_context(|| format!("creating `{}`", chained.display()))?;
            std::io::Write::write_all(&mut file, &original)?;
            // The umask may have dropped bits `mode` asked for.
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
            file.sync_all()?;
        }
        Some(defect) => return Err(refuse(&defect.to_string())),
        // An earlier install stopped before the rename.
        None if std::fs::read(&chained)? == original => {}
        None => return Err(refuse("differs from it")),
    }
    replace(&slot.dir, &hook, WRAPPER_BODY)
}

/// Why [`CHAINED`] cannot be the hook the wrapper runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainedDefect {
    Missing,
    Symlink,
    NotRegular,
    NotExecutable,
    /// It is `post-rewrite` itself, so the wrapper would run itself.
    SameFile,
}

impl std::fmt::Display for ChainedDefect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChainedDefect::Missing => "is missing",
            ChainedDefect::Symlink => "is a symlink",
            ChainedDefect::NotRegular => "is not a regular file",
            ChainedDefect::NotExecutable => "is not executable",
            ChainedDefect::SameFile => "is the same file as post-rewrite",
        })
    }
}

/// `None` when [`CHAINED`] is a regular executable file distinct from
/// `post-rewrite`. A symlink is refused even when it resolves elsewhere:
/// what it points at can change under the wrapper.
pub fn chained_defect(slot: &HookSlot) -> anyhow::Result<Option<ChainedDefect>> {
    use std::os::unix::fs::MetadataExt;
    let chained = slot.chained_path();
    let meta = match chained.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(ChainedDefect::Missing));
        }
        Err(e) => return Err(e).with_context(|| format!("inspecting `{}`", chained.display())),
    };
    let hook = std::fs::metadata(slot.path())
        .with_context(|| format!("inspecting `{}`", slot.path().display()))?;
    Ok(if meta.file_type().is_symlink() {
        Some(ChainedDefect::Symlink)
    } else if !meta.is_file() {
        Some(ChainedDefect::NotRegular)
    } else if (meta.dev(), meta.ino()) == (hook.dev(), hook.ino()) {
        Some(ChainedDefect::SameFile)
    } else if !runnable(&chained) {
        Some(ChainedDefect::NotExecutable)
    } else {
        None
    })
}

/// Write an executable `body` to `dest` by renaming a finished temp file
/// over it, so git never runs a half-written hook.
fn replace(dir: &Path, dest: &Path, body: &str) -> anyhow::Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temp file in `{}`", dir.display()))?;
    std::io::Write::write_all(&mut tmp, body.as_bytes())?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    tmp.as_file().sync_all()?;
    tmp.persist(dest)
        .map_err(|e| anyhow::anyhow!("writing `{}`: {}", dest.display(), e.error))?;
    Ok(())
}

#[cfg(test)]
#[path = "post_rewrite_tests.rs"]
mod tests;
