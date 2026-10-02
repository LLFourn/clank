//! Commit-keyed checkpoint store for [`RepoState`].
//!
//! Files live at
//! `<repo>/.clank/cache/repo-state/<head-sha>.<depth>.v<format>.bin`,
//! where `<depth>` is the commit's first-parent count from the repo
//! root. Depth lives in the FILENAME so the spacing policy
//! (`clank_core::checkpoint`) can plan pruning and the rebuild
//! lookup can order candidates from one `read_dir`, without
//! deserializing payloads. Depth is a placement/ordering hint only —
//! correctness of a loaded state never depends on it (lookups
//! confirm ancestry; a wrong depth merely mis-spaces future
//! checkpoints).
//!
//! Each file starts with a fixed-size header (magic, format
//! version, clank generation, head sha) validated before
//! deserialization. `RepoState.root` is NOT in the body — the
//! cache injects the canonical root at load time so a relocated
//! cache directory can't leak a stale absolute path.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use wincode::{SchemaRead, SchemaWrite};

use crate::lifecycle::CommitSha;
use crate::repo_state::RepoState;

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

const CACHE_MAGIC: &[u8] = b"CLANK-STATE\n";

/// - v1–v5: legacy/coexistence shapes pre-rename (invalid).
/// - v6: only the new sans-io fold (`clank_core::repo_state::RepoState`).
/// - v7: `RepoState::adopted` field gates pre-adoption AdHoc emission.
///   Older post-adoption caches would load with `adopted = false` and
///   silently suppress legitimate AdHoc events; bump forces a re-fold.
/// - v8: `CommitGateState::Blocked` variant added (appended at
///   position 4) for `status-blocks-dominate-gate`. Wincode is
///   position-encoded; existing v7 payloads would technically
///   decode, but bumping forces stale caches to be reclaimed via
///   filename mismatch — clean invalidation, no error path needed.
/// - v9: `CommitGateState::ContinuedPendingGate` variant added
///   (appended at position 5) for `teams-based-agent-registration`.
///   Same position-encoded discipline; bump forces clean
///   invalidation.
/// - v10: filename gains the commit depth
///   (`<sha>.<depth>.v10.bin`) for `fold-checkpoint-cache`.
///   Payload unchanged; the bump retires v9 names so the new
///   depth-aware listing never sees depthless files (mtime aging
///   reclaims them).
/// - v11: `RepoState` dropped the `active_plan_hint` field (the
///   classifier collapse — adhoc-commits-and-plan-tag-validation), so
///   the wincode payload shape changed; bump forces v10 caches to be
///   reclaimed rather than mis-deserialized.
/// - v12: `LogEvent::PlanFinalized` gained `subject` (the real whole-plan
///   finish message). Bump forces a one-time re-fold so existing finish
///   commits populate it — pre-v12 checkpoints have no subject and aren't
///   re-folded on incremental appends (ruthless ab4e174).
/// - v13: `RepoState` gained the durable `adopted_at` boundary used to
///   splice pre-adoption Git history without folding it. Old checkpoints
///   know only the boolean and cannot provide a safe floor.
// v14: optional active plan, scalar attribution, rejected-intro warning provenance.
// v15: finished runs persist their intro-to-finalize duration.
const CACHE_FORMAT_VERSION: u32 = 15;
const CLANK_CACHE_GENERATION: u32 = 1;

const HEADER_LEN: usize = CACHE_MAGIC.len() + 4 + 4 + 40;

const FRESH_HOURS: u64 = 24;
const MAX_BUCKET: u32 = 16;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache file too short for header")]
    HeaderTooShort,
    #[error("cache magic mismatch")]
    BadMagic,
    #[error("cache format version mismatch: file={file}, current={current}")]
    FormatVersion { file: u32, current: u32 },
    #[error("cache clank-generation mismatch: file={file}, current={current}")]
    ClankGeneration { file: u32, current: u32 },
    #[error("cache HEAD mismatch: file={file}, expected={expected}")]
    HeadMismatch { file: String, expected: String },
    #[error("wincode decode failure")]
    Decode,
    #[error("wincode encode failure")]
    Encode,
}

#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
struct Payload {
    fold: clank_core::repo_state::RepoState,
}

fn cache_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".clank").join("cache").join("repo-state")
}

fn cache_file_for(repo_root: &Path, head: &CommitSha, depth: u64) -> PathBuf {
    cache_dir(repo_root).join(format!(
        "{}.{}.v{}.bin",
        head.as_str(),
        depth,
        CACHE_FORMAT_VERSION
    ))
}

/// One on-disk checkpoint: a `RepoState` snapshot at `sha`, whose
/// first-parent depth from the repo root is `depth` (read from the
/// filename — see the module doc for its trust level).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRef {
    pub sha: CommitSha,
    pub depth: u64,
}

/// Try to load the checkpoint `cp` refers to. Any error path
/// removes the offending file before returning.
pub fn try_load(repo_root: &Path, cp: &CheckpointRef) -> Result<Option<RepoState>, CacheError> {
    let path = cache_file_for(repo_root, &cp.sha, cp.depth);
    match try_load_inner(&path, &cp.sha) {
        Ok(opt) => Ok(opt.map(|payload| RepoState {
            root: repo_root.to_path_buf(),
            head: Some(cp.sha.clone()),
            fold: payload.fold,
        })),
        Err(e) => {
            let _ = fs::remove_file(&path);
            Err(e)
        }
    }
}

/// Delete one checkpoint file. Losing a race to another pruning
/// process is fine — deletion is idempotent.
pub fn remove(repo_root: &Path, cp: &CheckpointRef) {
    let _ = fs::remove_file(cache_file_for(repo_root, &cp.sha, cp.depth));
}

fn try_load_inner(path: &Path, head: &CommitSha) -> Result<Option<Payload>, CacheError> {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    if buf.len() < HEADER_LEN {
        return Err(CacheError::HeaderTooShort);
    }
    let (header, body) = buf.split_at(HEADER_LEN);
    if &header[..CACHE_MAGIC.len()] != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    let mut cursor = CACHE_MAGIC.len();
    let format_version = u32::from_le_bytes(header[cursor..cursor + 4].try_into().unwrap());
    cursor += 4;
    if format_version != CACHE_FORMAT_VERSION {
        return Err(CacheError::FormatVersion {
            file: format_version,
            current: CACHE_FORMAT_VERSION,
        });
    }
    let clank_version = u32::from_le_bytes(header[cursor..cursor + 4].try_into().unwrap());
    cursor += 4;
    if clank_version != CLANK_CACHE_GENERATION {
        return Err(CacheError::ClankGeneration {
            file: clank_version,
            current: CLANK_CACHE_GENERATION,
        });
    }
    let file_head = std::str::from_utf8(&header[cursor..cursor + 40])
        .map_err(|_| CacheError::Decode)?
        .to_string();
    if file_head != head.as_str() {
        return Err(CacheError::HeadMismatch {
            file: file_head,
            expected: head.as_str().to_string(),
        });
    }
    let payload: Payload = wincode::deserialize(body).map_err(|_| CacheError::Decode)?;
    Ok(Some(payload))
}

/// Persist `state` as the checkpoint at `(state.head, depth)`.
/// Atomic (temp + rename); concurrent writers of the same
/// checkpoint converge on identical content.
pub fn write(repo_root: &Path, state: &RepoState, depth: u64) -> Result<(), CacheError> {
    let Some(head) = state.head.as_ref() else {
        return Ok(());
    };

    // Idempotent: a checkpoint payload is a pure function of
    // `(head sha, depth)`, so an existing file is already byte-correct.
    // Skip the rewrite entirely (no serialize, no rename) — rewriting
    // would only churn the file's mtime/inode, which the status watcher
    // sees and turns into a render→write-cache→wake loop
    // (status-tui-watch-cpu). A pruned/missing checkpoint is absent
    // here, so it still gets written.
    let final_path = cache_file_for(repo_root, head, depth);
    if final_path.exists() {
        return Ok(());
    }

    let dir = cache_dir(repo_root);
    fs::create_dir_all(&dir)?;

    let payload = Payload {
        fold: state.fold.clone(),
    };
    let body = wincode::serialize(&payload).map_err(|_| CacheError::Encode)?;

    let mut buf = Vec::with_capacity(HEADER_LEN + body.len());
    buf.extend_from_slice(CACHE_MAGIC);
    buf.extend_from_slice(&CACHE_FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&CLANK_CACHE_GENERATION.to_le_bytes());
    let sha_bytes = head.as_str().as_bytes();
    if sha_bytes.len() != 40 {
        return Err(CacheError::Encode);
    }
    buf.extend_from_slice(sha_bytes);
    buf.extend_from_slice(&body);

    let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        ".{}.{}.v{}.tmp.{}.{}",
        head.as_str(),
        depth,
        CACHE_FORMAT_VERSION,
        std::process::id(),
        nonce,
    ));
    {
        let mut tmp = fs::File::create(&tmp_path)?;
        tmp.write_all(&buf)?;
        tmp.sync_data().ok();
    }
    fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Every checkpoint for `repo_root`, ordered deepest (closest to
/// tip) first — the order rebuild lookups probe in. One `read_dir`;
/// no payloads are opened.
pub fn list_checkpoints(repo_root: &Path) -> Vec<CheckpointRef> {
    let dir = cache_dir(repo_root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let suffix = format!(".v{}.bin", CACHE_FORMAT_VERSION);
    let mut candidates: Vec<CheckpointRef> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let Some(stem) = name.strip_suffix(&suffix) else {
            continue;
        };
        let Some((sha_str, depth_str)) = stem.split_once('.') else {
            continue;
        };
        let Ok(sha) = CommitSha::parse(sha_str) else {
            continue;
        };
        let Ok(depth) = depth_str.parse::<u64>() else {
            continue;
        };
        candidates.push(CheckpointRef { sha, depth });
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.depth));
    candidates
}

/// Logarithmic thinning. Files under `FRESH_HOURS` survive; older
/// files keep one per `floor(log2(age_hours))` bucket; anything past
/// `MAX_BUCKET` is deleted.
pub fn prune(repo_root: &Path) -> Result<(), CacheError> {
    let dir = cache_dir(repo_root);
    let entries = match fs::read_dir(&dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let now = SystemTime::now();
    let fresh_window = Duration::from_secs(FRESH_HOURS * 3600);
    let mut aged: Vec<(PathBuf, Duration)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let mtime = match fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age = match now.duration_since(mtime) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if age < fresh_window {
            continue;
        }
        aged.push((path, age));
    }

    aged.sort_by_key(|(_, age)| *age);
    let mut buckets_seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (path, age) in aged {
        let hours = age.as_secs() as f64 / 3600.0;
        let bucket = if hours <= 1.0 {
            0
        } else {
            hours.log2().floor() as u32
        };
        let beyond_floor = bucket > MAX_BUCKET;
        let bucket_taken = !beyond_floor && !buckets_seen.insert(bucket);
        if beyond_floor || bucket_taken {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_repo() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn synth_state(repo_root: &Path) -> RepoState {
        let mut state = RepoState::empty(repo_root.to_path_buf());
        state.head = Some(CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap());
        state
    }

    fn cp_for(state: &RepoState, depth: u64) -> CheckpointRef {
        CheckpointRef {
            sha: state.head.clone().unwrap(),
            depth,
        }
    }

    // Captured with the v13 writer at 281235815474, before changing the fold.
    // Both overlapping active maps are genuinely encoded in these old bytes.
    const V13: &[u8] = include_bytes!("../tests/it/fixtures/repo-state-v13.bin");

    // Frozen v13/v14 record: it predates the recorded duration field.
    #[derive(wincode::SchemaRead)]
    struct LegacyFinishedPlan {
        plan: crate::lifecycle::PlanKey,
        intro: CommitSha,
        finalized_at: CommitSha,
    }

    #[test]
    fn legacy_scalar_checkpoint_preserves_real_finished_bytes_but_requires_refolding() {
        use clank_core::repo_state::{ActivePlan, AdHocEvent, RepoWarning};
        #[derive(wincode::SchemaRead)]
        struct V14Fold {
            active_plan: Option<ActivePlan>,
            finished_plans: Vec<LegacyFinishedPlan>,
            ad_hoc: Vec<AdHocEvent>,
            warnings: Vec<RepoWarning>,
            adopted: bool,
            adopted_at: Option<CommitSha>,
        }
        // Captured with e5d34f7's actual v14 writer before adding duration.
        let bytes = include_bytes!("../tests/it/fixtures/repo-state-v14.bin");
        let old: V14Fold = wincode::deserialize(&bytes[HEADER_LEN..]).unwrap();
        assert_eq!(old.active_plan.unwrap().key.as_str(), "next");
        assert_eq!(old.finished_plans.len(), 1);
        let first = &old.finished_plans[0];
        assert_eq!(first.plan.as_str(), "first");
        assert_ne!(first.intro, first.finalized_at);
        assert!(old.ad_hoc.is_empty() && old.warnings.is_empty());
        assert!(old.adopted && old.adopted_at.is_some());
        let dir = fresh_repo();
        let state = synth_state(dir.path());
        let cp = cp_for(&state, 4);
        fs::create_dir_all(cache_dir(dir.path())).unwrap();
        let legacy = cache_dir(dir.path()).join(format!("{}.4.v14.bin", cp.sha.as_str()));
        fs::write(&legacy, bytes).unwrap();
        assert!(list_checkpoints(dir.path()).is_empty());
        assert!(try_load(dir.path(), &cp).unwrap().is_none());
        let alias = cache_file_for(dir.path(), &cp.sha, cp.depth);
        fs::write(&alias, bytes).unwrap();
        assert!(matches!(
            try_load(dir.path(), &cp),
            Err(CacheError::FormatVersion {
                file: 14,
                current: CACHE_FORMAT_VERSION
            })
        ));
        assert!(!alias.exists());
        assert_eq!(fs::read(legacy).unwrap(), bytes);
    }

    #[test]
    fn legacy_map_checkpoint_is_skipped_and_rejected_before_decoding() {
        use clank_core::repo_state::{AdHocEvent, PlanState, RepoWarning};
        #[derive(wincode::SchemaRead)]
        struct V13Fold {
            plans: std::collections::BTreeMap<crate::lifecycle::PlanKey, PlanState>,
            finished_plans: Vec<LegacyFinishedPlan>,
            ad_hoc: Vec<AdHocEvent>,
            warnings: Vec<RepoWarning>,
            adopted: bool,
            adopted_at: Option<CommitSha>,
        }
        // Prove this is the old payload, not a current payload with an old header.
        let old: V13Fold = wincode::deserialize(&V13[HEADER_LEN..]).unwrap();
        assert_eq!(
            old.plans.keys().map(|k| k.as_str()).collect::<Vec<_>>(),
            vec!["accepted", "overlapping"]
        );
        assert!(old.plans.values().all(|p| p.commits.len() == 1));
        assert!(old.finished_plans.is_empty() && old.ad_hoc.is_empty() && old.warnings.is_empty());
        assert!(old.adopted && old.adopted_at.is_some());

        let dir = fresh_repo();
        let state = synth_state(dir.path());
        let cp = cp_for(&state, 2);
        fs::create_dir_all(cache_dir(dir.path())).unwrap();
        let legacy = cache_dir(dir.path()).join(format!("{}.2.v13.bin", cp.sha.as_str()));
        fs::write(&legacy, V13).unwrap();
        assert!(list_checkpoints(dir.path()).is_empty());
        assert!(try_load(dir.path(), &cp).unwrap().is_none());
        assert_eq!(fs::read(&legacy).unwrap(), V13);

        // A renamed old checkpoint must fail version validation, not decode as
        // a scalar fold. The existing corrupt-cache recovery discards this alias.
        let current = cache_file_for(dir.path(), &cp.sha, cp.depth);
        fs::write(&current, V13).unwrap();
        assert!(matches!(
            try_load(dir.path(), &cp),
            Err(CacheError::FormatVersion {
                file: 13,
                current: CACHE_FORMAT_VERSION
            })
        ));
        assert!(!current.exists());
        assert!(legacy.exists());
    }

    #[test]
    fn ignored_run_provenance_survives_checkpoint_and_active_finalization() {
        use clank_core::repo_state::{CommitEvent, PlanTouchInput, TouchKind};
        let dir = fresh_repo();
        let mut state = synth_state(dir.path());
        let event = |name: &str, kind, n: u8| CommitEvent {
            sha: CommitSha::parse(&format!("{n:040x}")).unwrap(),
            author_ts: n as i64,
            subject: format!("[{name}] work"),
            plan_touches: vec![PlanTouchInput {
                plan: crate::lifecycle::PlanKey::parse(name).unwrap(),
                kind,
            }],
            has_code_changes: true,
        };
        state
            .fold
            .apply_commit(&event("accepted", TouchKind::Intro, 1));
        state
            .fold
            .apply_commit(&event("ignored", TouchKind::Intro, 2));
        write(dir.path(), &state, 2).unwrap();
        let mut loaded = try_load(dir.path(), &cp_for(&state, 2)).unwrap().unwrap();
        assert_eq!(loaded.fold, state.fold);
        loaded
            .fold
            .apply_commit(&event("accepted", TouchKind::Finish, 3));
        let before = loaded.fold.clone();
        assert!(
            loaded
                .fold
                .apply_commit(&event("ignored", TouchKind::Finish, 4))
                .is_empty()
        );
        assert_eq!(loaded.fold, before);
        assert!(loaded.fold.active_plan.is_none());
        assert_eq!(loaded.fold.finished_plans.len(), 1);
    }

    #[test]
    fn write_then_load_round_trips() {
        let dir = fresh_repo();
        let state = synth_state(dir.path());
        write(dir.path(), &state, 7).unwrap();
        let loaded = try_load(dir.path(), &cp_for(&state, 7))
            .unwrap()
            .expect("cache file should exist after write");
        assert_eq!(loaded.head, state.head);
        assert_eq!(loaded.fold, state.fold);
    }

    #[test]
    fn write_is_idempotent_when_file_exists() {
        // A second write of the same (head, depth) must NOT replace the
        // file — the payload is determined by (head, depth), so a
        // rewrite only churns the mtime/inode and feeds the status-tui
        // wake loop (status-tui-watch-cpu). Proven by writing a sentinel
        // over the real file and asserting `write` leaves it untouched.
        let dir = fresh_repo();
        let state = synth_state(dir.path());
        write(dir.path(), &state, 7).unwrap();
        let path = cache_file_for(dir.path(), state.head.as_ref().unwrap(), 7);

        fs::write(&path, b"SENTINEL").unwrap();
        write(dir.path(), &state, 7).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            b"SENTINEL",
            "write must skip when the checkpoint file already exists"
        );
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = fresh_repo();
        let head = CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap();
        assert!(
            try_load(
                dir.path(),
                &CheckpointRef {
                    sha: head,
                    depth: 1
                }
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn try_load_error_deletes_offending_file() {
        let dir = fresh_repo();
        let state = synth_state(dir.path());
        write(dir.path(), &state, 3).unwrap();
        let path = cache_file_for(dir.path(), state.head.as_ref().unwrap(), 3);
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] = b'X';
        fs::write(&path, &bytes).unwrap();
        let _ = try_load(dir.path(), &cp_for(&state, 3)).unwrap_err();
        assert!(!path.exists());
    }

    #[test]
    fn list_checkpoints_orders_deepest_first_and_skips_foreign_names() {
        let dir = fresh_repo();
        let mut state = synth_state(dir.path());
        write(dir.path(), &state, 5).unwrap();
        state.head = Some(CommitSha::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap());
        write(dir.path(), &state, 12).unwrap();
        // Old-format (depthless) and unrelated files are ignored.
        fs::write(
            cache_dir(dir.path()).join(format!(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.v{CACHE_FORMAT_VERSION}.bin"
            )),
            b"junk",
        )
        .unwrap();
        fs::write(cache_dir(dir.path()).join("README"), b"junk").unwrap();

        let listed = list_checkpoints(dir.path());
        assert_eq!(
            listed.iter().map(|c| c.depth).collect::<Vec<_>>(),
            vec![12, 5],
            "deepest first"
        );
    }

    #[test]
    fn remove_deletes_only_the_named_checkpoint() {
        let dir = fresh_repo();
        let state = synth_state(dir.path());
        write(dir.path(), &state, 5).unwrap();
        write(dir.path(), &state, 9).unwrap();
        remove(dir.path(), &cp_for(&state, 5));
        let listed = list_checkpoints(dir.path());
        assert_eq!(listed.iter().map(|c| c.depth).collect::<Vec<_>>(), vec![9]);
    }
}
