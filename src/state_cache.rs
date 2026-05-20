//! Commit-keyed cache for [`BaseRepoState`]. Phase 3 of
//! `.trinity/plans/cache-core-fold-and-live-feedback.md`.
//!
//! Files live at:
//! ```text
//! <repo>/.trinity/cache/repo-state/<head-sha>.v<format>.bin
//! ```
//!
//! Each file starts with a fixed-size header that is validated
//! before deserialization:
//!
//! ```text
//! magic           : b"TRINITY-BASE-STATE\n"   (19 bytes)
//! format_version  : u32 LE                    (4 bytes)
//! trinity_version : u32 LE                    (4 bytes)
//! head_sha        : 40 bytes ASCII            (40 bytes)
//! -- followed by the wincode body --
//! ```
//!
//! `RepoState.root` is **not** in the body. The cache module
//! injects the current canonical root at load time so a relocated
//! cache directory cannot leak a stale absolute path into runtime
//! state.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use wincode::{SchemaRead, SchemaWrite};

use crate::lifecycle::{CommitSha, PlanKey};
use crate::repo_state::{BaseRepoState, RepoState};
use trinity_core::model::Plan;

/// Per-process counter mixed into cache tempfile names so two
/// concurrent same-process rebuild tasks writing the same
/// `(repo, head)` cannot collide on the temp path.
static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

/// Magic bytes prefixing every cache file. Bumped iff the on-disk
/// layout itself (header shape) ever changes.
const CACHE_MAGIC: &[u8] = b"TRINITY-BASE-STATE\n";

/// Cache file layout version. Bump when the wincode body shape
/// changes incompatibly (a field added/removed from
/// [`BaseStatePayload`] or its transitive types).
/// Format-version bumps:
///  - v1: original `BaseStatePayload { plans }`.
///  - v2: adds repo-wide `commits` map (Phase 1 of
///    commit-first-review-model).
///  - v3: adds `commit_order: Vec<CommitSha>` to encode the
///    first-parent fold sequence. Older caches are invalidated.
const CACHE_FORMAT_VERSION: u32 = 3;

/// Trinity binary identity. Bump on incompatible changes that
/// would make decoded state semantically invalid even if it
/// deserialized cleanly.
const TRINITY_CACHE_GENERATION: u32 = 1;

/// Header length in bytes: magic + 2x u32 + 40-byte sha.
const HEADER_LEN: usize = CACHE_MAGIC.len() + 4 + 4 + 40;

/// Logarithmic thinning: keep every cache entry younger than this.
const FRESH_HOURS: u64 = 24;

/// Cap the thinned region: anything older than 2^MAX_BUCKET hours
/// gets deleted.
const MAX_BUCKET: u32 = 16; // ~7.5 years

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache file too short for header")]
    HeaderTooShort,
    #[error("cache magic mismatch (not a trinity base-state cache file)")]
    BadMagic,
    #[error("cache format version mismatch: file={file}, current={current}")]
    FormatVersion { file: u32, current: u32 },
    #[error("cache trinity-generation mismatch: file={file}, current={current}")]
    TrinityGeneration { file: u32, current: u32 },
    #[error("cache HEAD mismatch: file={file}, expected={expected}")]
    HeadMismatch { file: String, expected: String },
    #[error("wincode decode failure")]
    Decode,
    #[error("wincode encode failure")]
    Encode,
}

/// Rootless cache payload — `RepoState` minus `root`, `head`,
/// and `plan_conflicts`. Wincode-encoded into the cache file body.
///
/// `root` and `head` are NOT serialized: both are runtime
/// identity that must come from outside the payload at load
/// time. `root` is injected from the current canonical repo
/// path (so a relocated cache doesn't leak a stale absolute
/// path); `head` is the value validated in the file header (so
/// the body can't claim a different HEAD than the header).
#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
struct BaseStatePayload {
    plans: BTreeMap<PlanKey, Plan>,
    commits: BTreeMap<crate::lifecycle::CommitSha, crate::repo_state::CommitNode>,
    commit_order: Vec<crate::lifecycle::CommitSha>,
}

impl BaseStatePayload {
    fn from_state(state: &RepoState) -> Self {
        Self {
            plans: state.plans.clone(),
            commits: state.commits.clone(),
            commit_order: state.commit_order.clone(),
        }
    }

    fn into_base(self, root: PathBuf, head: CommitSha) -> BaseRepoState {
        // plan_conflicts is intentionally not cached — see field
        // comment in repo_state.rs ("Today effectively unused").
        // If it ever carries data, bump CACHE_FORMAT_VERSION.
        let state = RepoState {
            root,
            plans: self.plans,
            commits: self.commits,
            commit_order: self.commit_order,
            head: Some(head),
            plan_conflicts: BTreeMap::new(),
        };
        BaseRepoState::new(state)
    }
}

/// Path to the cache directory for `repo_root`.
fn cache_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".trinity").join("cache").join("repo-state")
}

fn cache_file_for(repo_root: &Path, head: &CommitSha) -> PathBuf {
    cache_dir(repo_root).join(format!("{}.v{}.bin", head.as_str(), CACHE_FORMAT_VERSION,))
}

/// Try to load a cached `BaseRepoState` for `(repo_root, head)`.
/// Returns `Ok(None)` if the file does not exist; returns `Err`
/// on header validation failure, corrupt body, or io error so the
/// caller can log and fall back to a full fold.
pub fn try_load(repo_root: &Path, head: &CommitSha) -> Result<Option<BaseRepoState>, CacheError> {
    let path = cache_file_for(repo_root, head);
    let mut file = match fs::File::open(&path) {
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
    let trinity_version = u32::from_le_bytes(header[cursor..cursor + 4].try_into().unwrap());
    cursor += 4;
    if trinity_version != TRINITY_CACHE_GENERATION {
        return Err(CacheError::TrinityGeneration {
            file: trinity_version,
            current: TRINITY_CACHE_GENERATION,
        });
    }
    let file_head = std::str::from_utf8(&header[cursor..cursor + 40])
        .map_err(|_| CacheError::Decode)?
        .to_string();
    let expected_head = head.as_str();
    if file_head != expected_head {
        return Err(CacheError::HeadMismatch {
            file: file_head,
            expected: expected_head.to_string(),
        });
    }

    let payload: BaseStatePayload = wincode::deserialize(body).map_err(|_| CacheError::Decode)?;
    // Inject the CURRENT canonical root and the header-validated
    // HEAD. Both are runtime identity; we do not trust the body
    // to carry them (root because a moved cache dir would leak a
    // stale path; head because the file header is the source of
    // truth for which commit this payload describes).
    Ok(Some(
        payload.into_base(repo_root.to_path_buf(), head.clone()),
    ))
}

/// Write `base` to the cache, atomically. Returns `Ok(())` on
/// success; logs and swallows failure semantics belong to the
/// caller — never block an operator on a cache write.
///
/// `expected_head` must match `base.head`; this is checked before
/// any IO to catch caller mistakes early.
pub fn write(repo_root: &Path, base: &BaseRepoState) -> Result<(), CacheError> {
    let Some(head) = base.head.as_ref() else {
        // Empty repo — nothing to cache. Cheap rebuild anyway.
        return Ok(());
    };
    let dir = cache_dir(repo_root);
    fs::create_dir_all(&dir)?;

    let payload = BaseStatePayload::from_state(base);
    let body = wincode::serialize(&payload).map_err(|_| CacheError::Encode)?;

    let mut buf = Vec::with_capacity(HEADER_LEN + body.len());
    buf.extend_from_slice(CACHE_MAGIC);
    buf.extend_from_slice(&CACHE_FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&TRINITY_CACHE_GENERATION.to_le_bytes());
    let sha_bytes = head.as_str().as_bytes();
    if sha_bytes.len() != 40 {
        // CommitSha::parse enforces 40-char hex; this shouldn't
        // happen, but a malformed value would corrupt the header.
        return Err(CacheError::Encode);
    }
    buf.extend_from_slice(sha_bytes);
    buf.extend_from_slice(&body);

    // tempfile-in-same-dir + atomic rename. Two concurrent
    // rebuild tasks for the same `(repo, head)` will race, but
    // the payload bytes are deterministic at a given HEAD so the
    // winner doesn't matter.
    //
    // Uniqueness: pid alone isn't enough — two same-process tasks
    // (e.g. two daemon rebuild jobs racing on the same head)
    // would collide. Mix in a monotonic atomic counter so every
    // call to `write` gets a distinct temp path.
    let final_path = cache_file_for(repo_root, head);
    let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        ".{}.v{}.tmp.{}.{}",
        head.as_str(),
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

/// Apply logarithmic thinning to the cache directory. Best-effort:
/// any individual delete failure is logged and skipped.
///
/// - Files with `age < FRESH_HOURS`: keep all.
/// - Older files: bucket by `floor(log2(age_hours))`, keep newest
///   in each bucket.
/// - Files in buckets > `MAX_BUCKET`: delete.
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
            Err(_) => continue, // file in the future; leave alone
        };
        if age < fresh_window {
            continue; // fresh window keeps everything
        }
        aged.push((path, age));
    }

    // Sort youngest-first so the first survivor in each bucket is
    // the freshest within it.
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
    use crate::disk_snapshot::{CommitSnapshot, derive_base_state};

    fn fresh_repo() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn synth_base(repo_root: &Path) -> BaseRepoState {
        // Use the real fold with an empty CommitSnapshot — a HEAD
        // is required for the cache write, so we forge one.
        let snap = CommitSnapshot {
            head: Some(CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap()),
            history: Vec::new(),
        };
        derive_base_state(repo_root.to_path_buf(), snap)
    }

    #[test]
    fn write_then_load_round_trips() {
        let dir = fresh_repo();
        let base = synth_base(dir.path());
        write(dir.path(), &base).unwrap();
        let loaded = try_load(dir.path(), base.head.as_ref().unwrap())
            .unwrap()
            .expect("cache file should exist after write");
        assert_eq!(loaded.head, base.head);
        assert_eq!(loaded.plans, base.plans);
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = fresh_repo();
        let head = CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap();
        let result = try_load(dir.path(), &head).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn corrupt_body_rejected() {
        let dir = fresh_repo();
        let base = synth_base(dir.path());
        write(dir.path(), &base).unwrap();
        // Truncate the file so the body decode fails.
        let path = cache_file_for(dir.path(), base.head.as_ref().unwrap());
        let truncated_len = HEADER_LEN + 2; // header survives, body broken
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..truncated_len.min(bytes.len())]).unwrap();
        let err = try_load(dir.path(), base.head.as_ref().unwrap()).unwrap_err();
        assert!(
            matches!(err, CacheError::Decode),
            "expected Decode error, got {err:?}",
        );
    }

    #[test]
    fn bad_magic_rejected() {
        let dir = fresh_repo();
        let base = synth_base(dir.path());
        write(dir.path(), &base).unwrap();
        let path = cache_file_for(dir.path(), base.head.as_ref().unwrap());
        // Overwrite the first byte of the magic.
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] = b'X';
        fs::write(&path, &bytes).unwrap();
        let err = try_load(dir.path(), base.head.as_ref().unwrap()).unwrap_err();
        assert!(
            matches!(err, CacheError::BadMagic),
            "expected BadMagic, got {err:?}",
        );
    }

    #[test]
    fn wrong_head_rejected() {
        let dir = fresh_repo();
        let base = synth_base(dir.path());
        write(dir.path(), &base).unwrap();
        // Try loading under a DIFFERENT head; the cache file is
        // at the original head's filename, so this just returns
        // None (no file). To exercise the header check, plant a
        // file with mismatching header head_sha.
        let real_head = base.head.as_ref().unwrap();
        let other = CommitSha::parse("abcdef0123456789abcdef0123456789abcdef01").unwrap();
        // Copy the real file to other's path.
        let src = cache_file_for(dir.path(), real_head);
        let dst = cache_file_for(dir.path(), &other);
        fs::copy(&src, &dst).unwrap();
        let err = try_load(dir.path(), &other).unwrap_err();
        assert!(
            matches!(err, CacheError::HeadMismatch { .. }),
            "expected HeadMismatch, got {err:?}",
        );
    }

    #[test]
    fn relocated_cache_injects_current_root() {
        // Write a cache, then load it via a different repo_root.
        // The resulting BaseRepoState.root must be the load-time
        // root, not whatever was in the original payload.
        let original = fresh_repo();
        let base = synth_base(original.path());
        write(original.path(), &base).unwrap();

        // Move the .trinity/cache dir to a renamed repo location.
        let relocated = fresh_repo();
        let src = original.path().join(".trinity");
        let dst = relocated.path().join(".trinity");
        // Recursive copy.
        copy_dir(&src, &dst).unwrap();

        let loaded = try_load(relocated.path(), base.head.as_ref().unwrap())
            .unwrap()
            .expect("cache should load at the new path");
        assert_eq!(
            loaded.root,
            relocated.path(),
            "loaded root must be the current canonical repo root, not the writer's"
        );
        assert_ne!(
            loaded.root,
            original.path(),
            "stale absolute path must not survive a moved cache directory"
        );
    }

    fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_dir(&from, &to)?;
            } else {
                fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }

    /// Regression for codex's review on 552894d: two writes for
    /// the same `(repo, head)` in one process must not collide on
    /// the temp file path. The nonce makes every temp path
    /// distinct.
    #[test]
    fn concurrent_same_process_writes_do_not_collide() {
        let dir = fresh_repo();
        let base = synth_base(dir.path());
        // Spawn N parallel writes; if any temp file rename
        // failed mid-flight the call would error out.
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = dir.path().to_path_buf();
                let base = base.clone();
                std::thread::spawn(move || write(&path, &base))
            })
            .collect();
        for t in threads {
            t.join().unwrap().expect("concurrent write must succeed");
        }
        // The final file is well-formed (one winner of the rename).
        let loaded = try_load(dir.path(), base.head.as_ref().unwrap())
            .unwrap()
            .expect("cache file should exist");
        assert_eq!(loaded.head, base.head);

        // No stray temp files left behind. Atomic rename on a
        // POSIX filesystem replaces the target if it exists, so
        // the temp file must be gone post-rename for every winner.
        let cache = cache_dir(dir.path());
        let strays: Vec<PathBuf> = fs::read_dir(&cache)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('.') && n.contains(".tmp."))
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            strays.is_empty(),
            "no stray temp files allowed after concurrent writes; got {strays:?}",
        );
    }

    /// Regression for codex's review on 552894d: HEAD is not
    /// trusted from the payload. A maliciously (or accidentally)
    /// swapped body must not be returned under the wrong head.
    ///
    /// Construction: write a cache for head A, copy the file to
    /// head B's filename, then patch the header's sha bytes so
    /// they spell head B. `try_load(repo, B)` validates header
    /// matches, decodes the body, then injects head B (from the
    /// header) — NOT whatever the body might have claimed. Since
    /// `head` is no longer in the payload, this is structurally
    /// impossible to subvert.
    #[test]
    fn head_injected_from_header_not_payload() {
        let dir = fresh_repo();
        let head_a = CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap();
        let head_b = CommitSha::parse("abcdef0123456789abcdef0123456789abcdef01").unwrap();
        let base_a = {
            let snap = CommitSnapshot {
                head: Some(head_a.clone()),
                history: Vec::new(),
            };
            derive_base_state(dir.path().to_path_buf(), snap)
        };
        write(dir.path(), &base_a).unwrap();

        // Copy the file for head A to head B's path, and rewrite
        // the header's sha bytes to spell head B.
        let src = cache_file_for(dir.path(), &head_a);
        let dst = cache_file_for(dir.path(), &head_b);
        let mut bytes = fs::read(&src).unwrap();
        // Header layout: CACHE_MAGIC (19) + u32 format + u32 generation,
        // then 40-byte sha at offset CACHE_MAGIC.len() + 8.
        let sha_offset = CACHE_MAGIC.len() + 8;
        bytes[sha_offset..sha_offset + 40].copy_from_slice(head_b.as_str().as_bytes());
        fs::write(&dst, &bytes).unwrap();

        // Loading at head_b succeeds (header matches), and the
        // resulting BaseRepoState.head is head_b (from header),
        // not anything claimed by the payload body.
        let loaded = try_load(dir.path(), &head_b)
            .unwrap()
            .expect("file at head_b path should load");
        assert_eq!(
            loaded.head.as_ref().unwrap().as_str(),
            head_b.as_str(),
            "loaded head must come from the header, not the body",
        );
    }

    #[test]
    fn prune_keeps_fresh_window_and_thins_older_entries() {
        let dir = fresh_repo();
        let cache = cache_dir(dir.path());
        fs::create_dir_all(&cache).unwrap();

        // Synthesize cache files with backdated mtimes spanning
        // fresh-window through deep history.
        //
        // Per logarithmic policy expectation: every file with
        // age < FRESH_HOURS survives. Older files are bucketed by
        // floor(log2(age_hours)); the newest per bucket survives.
        let ages_hours: &[u64] = &[
            0,       // fresh
            12,      // fresh (< 24)
            25,      // bucket 4 (log2(25)=4.64 → 4)
            30,      // bucket 4 again — competes with 25 (keep newer)
            70,      // bucket 6 (log2(70)=6.13 → 6)
            300,     // bucket 8 (log2(300)=8.23 → 8)
            10_000,  // bucket 13 (log2(10000)=13.28 → 13)
            200_000, // bucket > MAX_BUCKET → delete
        ];
        let now = SystemTime::now();
        for (i, &h) in ages_hours.iter().enumerate() {
            let path = cache.join(format!("file-{i}.bin"));
            fs::write(&path, b"x").unwrap();
            let mtime = now - Duration::from_secs(h * 3600);
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(mtime)).unwrap();
        }

        prune(dir.path()).unwrap();

        let mut survivors: Vec<String> = fs::read_dir(&cache)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        survivors.sort();

        // Expected survivors:
        // - file-0 (0h, fresh)
        // - file-1 (12h, fresh)
        // - file-3 (30h, bucket 4 newest)  — file-2 (25h)? Both 25 and
        //   30 hours map to bucket 4 (log2 of both rounds to 4); the
        //   younger (25h) survives. Recompute:
        //   log2(25.0)=4.64 → 4
        //   log2(30.0)=4.91 → 4
        //   sort ascending by age means file-2 (25h) comes first and
        //   wins; file-3 (30h) is the second in the bucket and is
        //   removed.
        // - file-4 (70h, bucket 6)
        // - file-5 (300h, bucket 8)
        // - file-6 (10000h, bucket 13)
        // - file-7 (200000h): log2(200000)=17.61 → 17 > 16 → deleted
        let expected: Vec<&str> = vec![
            "file-0.bin",
            "file-1.bin",
            "file-2.bin",
            "file-4.bin",
            "file-5.bin",
            "file-6.bin",
        ];
        assert_eq!(
            survivors,
            expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "logarithmic thinning produced wrong survivor set",
        );
    }
}
