//! Commit-keyed cache for [`RepoState`].
//!
//! Files live at `<repo>/.clank/cache/repo-state/<head-sha>.v<format>.bin`.
//! Each file starts with a fixed-size header (magic + format_version
//! + clank_version + head_sha) validated before deserialization.
//! `RepoState.root` is NOT in the body — the cache injects the
//! canonical root at load time so a relocated cache directory can't
//! leak a stale absolute path.

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
const CACHE_FORMAT_VERSION: u32 = 7;
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

fn cache_file_for(repo_root: &Path, head: &CommitSha) -> PathBuf {
    cache_dir(repo_root).join(format!("{}.v{}.bin", head.as_str(), CACHE_FORMAT_VERSION))
}

/// Try to load a cached [`RepoState`] for `(repo_root, head)`. Any
/// error path removes the offending file before returning.
pub fn try_load(repo_root: &Path, head: &CommitSha) -> Result<Option<RepoState>, CacheError> {
    let path = cache_file_for(repo_root, head);
    match try_load_inner(&path, head) {
        Ok(opt) => Ok(opt.map(|payload| RepoState {
            root: repo_root.to_path_buf(),
            head: Some(head.clone()),
            fold: payload.fold,
        })),
        Err(e) => {
            let _ = fs::remove_file(&path);
            Err(e)
        }
    }
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

pub fn write(repo_root: &Path, state: &RepoState) -> Result<(), CacheError> {
    let Some(head) = state.head.as_ref() else {
        return Ok(());
    };
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

/// Every cached HEAD-SHA for `repo_root`, ordered most-recently-
/// modified first. Used by Phase-2 incremental cache loading.
pub fn list_cached_heads(repo_root: &Path) -> Vec<CommitSha> {
    let dir = cache_dir(repo_root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let suffix = format!(".v{}.bin", CACHE_FORMAT_VERSION);
    let mut candidates: Vec<(SystemTime, CommitSha)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let Some(sha_str) = name.strip_suffix(&suffix) else {
            continue;
        };
        let Ok(sha) = CommitSha::parse(sha_str) else {
            continue;
        };
        let mtime = match fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        candidates.push((mtime, sha));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().map(|(_, sha)| sha).collect()
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

    #[test]
    fn write_then_load_round_trips() {
        let dir = fresh_repo();
        let state = synth_state(dir.path());
        write(dir.path(), &state).unwrap();
        let loaded = try_load(dir.path(), state.head.as_ref().unwrap())
            .unwrap()
            .expect("cache file should exist after write");
        assert_eq!(loaded.head, state.head);
        assert_eq!(loaded.fold, state.fold);
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = fresh_repo();
        let head = CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap();
        assert!(try_load(dir.path(), &head).unwrap().is_none());
    }

    #[test]
    fn try_load_error_deletes_offending_file() {
        let dir = fresh_repo();
        let state = synth_state(dir.path());
        write(dir.path(), &state).unwrap();
        let path = cache_file_for(dir.path(), state.head.as_ref().unwrap());
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] = b'X';
        fs::write(&path, &bytes).unwrap();
        let _ = try_load(dir.path(), state.head.as_ref().unwrap()).unwrap_err();
        assert!(!path.exists());
    }
}
