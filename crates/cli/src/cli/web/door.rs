//! The door: who the remote lets in. The credential is ONE token,
//! the user's and not a session's, read off the TUI and pasted into
//! the page; it opens a session, which is a cookie the browser holds
//! and a hashed record here, closed when it is revoked from the TUI
//! or after [`SESSION_DAYS`]. A passkey was bound to a hostname and
//! so demanded a stable domain and the account behind it; a token is
//! bound to nothing (the-way-in-is-a-token).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

pub(crate) const SESSION_DAYS: i64 = 30;
/// A minted link is good for this long, and for one use.
pub(crate) const LINK_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// Login attempts one address may make in a minute before it is
/// refused for the rest of it.
pub(crate) const ATTEMPTS_PER_MINUTE: u32 = 10;
pub(crate) const COOKIE: &str = "clank_session";
const SESSIONS_FILE: &str = ".clank/remote-sessions.json";

/// A one-time link the TUI minted: the way in that saves a paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    Login,
}

/// One open session as the file keeps it: the hash of the cookie,
/// never the cookie.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SessionRecord {
    pub(crate) id_hash: String,
    /// How it was opened — the pasted token, or a minted link. The
    /// serde alias keeps sessions written before the token readable.
    #[serde(alias = "passkey")]
    pub(crate) how: String,
    pub(crate) created: String,
    pub(crate) last_seen: String,
}

#[derive(Default, Serialize, Deserialize)]
struct SessionsFile {
    #[serde(default)]
    sessions: Vec<SessionRecord>,
}

/// `~/.clank/config.json#/remote`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteSection {
    /// The tunnel that gives this machine a public URL, when there
    /// is one: where the phone's link points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tunnel: Option<super::tunnel::TunnelSection>,
    /// The one credential, minted on first use and kept HERE — at
    /// user level, so every repo's remote on this machine takes the
    /// same one and it survives restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<String>,
}

/// A session the door admitted: what the routes carry.
#[derive(Debug, Clone)]
pub(crate) struct Admitted {
    pub(crate) id_hash: String,
    /// When its term ends: a stream holding it ends then.
    pub(crate) expires: tokio::time::Instant,
}

/// A one-time link the TUI minted.
struct Minted {
    kind: Link,
    hash: [u8; 32],
    expires: tokio::time::Instant,
}

/// What this instance keeps in memory: the links it minted, the
/// attempts it has seen, the live set as last read — and, without a
/// home, the store itself.
#[derive(Default)]
struct Inner {
    minted: Vec<Minted>,
    attempts: HashMap<IpAddr, (tokio::time::Instant, u32)>,
    /// The live session ids as last read, so a change — another
    /// TUI's revocation, an expiry — is noticed and announced.
    live: std::collections::BTreeSet<String>,
    memory: Memory,
}

/// The store when there is no home: this process only.
#[derive(Default)]
struct Memory {
    sessions: Vec<SessionRecord>,
    remote: RemoteSection,
}

/// Both halves of the store, as one locked read gives them.
struct Store {
    sessions: Vec<SessionRecord>,
    remote: RemoteSection,
}

/// Which halves an edit changed, so only those are written.
struct Wrote {
    sessions: bool,
    remote: bool,
}

/// The remote's authority is the user's, not one TUI's: every TUI
/// reads and writes the same two files under one lock, and holds no
/// copy of them, so a revocation in one is refused by the next read
/// in another and a write never resurrects what a sibling removed
/// (codex on 5d24c05).
pub(crate) struct Door {
    home: Option<PathBuf>,
    inner: Mutex<Inner>,
    /// Bumped whenever the live set changes: a stream holding a
    /// session watches this and asks whether it is still live.
    revoked: tokio::sync::watch::Sender<u64>,
    /// How many times the store has been opened. The tests assert on
    /// it because "one authority transaction" is not observable from
    /// outcomes alone: a verify-then-open split into two holds gives
    /// the same answers until a rotation happens to land between
    /// them, which a test cannot schedule (codex on f71e7b9).
    #[cfg(test)]
    transactions: std::sync::atomic::AtomicUsize,
}

/// What the page posts to the paste box.
#[derive(Deserialize)]
pub(crate) struct Paste {
    pub(crate) token: String,
}

/// Why the door did not open, in words for the browser.
#[derive(Debug)]
pub(crate) enum Refused {
    /// The token is not this machine's. A SPENT LINK is not here: it
    /// redirects to the page saying so, which is where someone who
    /// followed a stale link can paste the token instead.
    Token,
    /// The store could not be read or written; nothing was changed.
    Store(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Token => write!(
                f,
                "that is not this machine's token — read it off the remote page in the TUI"
            ),
            Refused::Store(why) => write!(f, "the store refused: {why}"),
        }
    }
}

impl Door {
    /// The door for this user: the token in `~/.clank/config.json`,
    /// open sessions in `~/.clank/remote-sessions.json`, read on
    /// every use. Without a home nothing persists — a session lasts
    /// as long as the process.
    pub(crate) fn new(home: Option<PathBuf>) -> Self {
        let door = Self {
            home,
            inner: Mutex::new(Inner::default()),
            revoked: tokio::sync::watch::channel(0).0,
            #[cfg(test)]
            transactions: std::sync::atomic::AtomicUsize::new(0),
        };
        door.refresh();
        door
    }

    pub(crate) fn watch_revocations(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revoked.subscribe()
    }

    // ---- the store ----

    /// Read, edit and write the sessions as one step under the
    /// user-level lock. `edit` says whether it changed anything;
    /// nothing is written otherwise, and nothing is kept here either
    /// way.
    fn with_sessions<R>(
        &self,
        edit: impl FnOnce(&mut Vec<SessionRecord>) -> (R, bool),
    ) -> anyhow::Result<R> {
        self.with_store(|store| {
            let (r, changed) = edit(&mut store.sessions);
            (
                r,
                Wrote {
                    sessions: changed,
                    remote: false,
                },
            )
        })
    }

    /// The `remote` section, the same way, in the user config.
    fn with_remote<R>(
        &self,
        edit: impl FnOnce(&mut RemoteSection) -> (R, bool),
    ) -> anyhow::Result<R> {
        self.with_store(|store| {
            let (r, changed) = edit(&mut store.remote);
            (
                r,
                Wrote {
                    sessions: false,
                    remote: changed,
                },
            )
        })
    }

    /// BOTH halves of the store — the credential and the sessions it
    /// opened — read, edited and written under ONE hold of the
    /// user-level lock. They are two files but one authority: a
    /// verify-then-open across two holds let another TUI's rotation
    /// land between them and hand out a live session for a credential
    /// that no longer exists (codex on f71e7b9).
    ///
    /// The sessions are written FIRST. The two files cannot be made
    /// atomic, so the order decides which way a half-written change
    /// fails: sessions-then-credential leaves the old credential
    /// valid over emptied sessions, which refuses too much. The
    /// reverse would leave a rotated credential over sessions the old
    /// one opened, which admits too much.
    fn with_store<R>(&self, edit: impl FnOnce(&mut Store) -> (R, Wrote)) -> anyhow::Result<R> {
        #[cfg(test)]
        self.transactions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(home) = &self.home else {
            let mut inner = self.lock();
            let Memory { sessions, remote } = &mut inner.memory;
            let mut store = Store {
                sessions: std::mem::take(sessions),
                remote: remote.clone(),
            };
            let (r, _) = edit(&mut store);
            *sessions = store.sessions;
            *remote = store.remote;
            return Ok(r);
        };
        let _lock = store_lock(home)?;
        let path = sessions_path(home);
        let sessions = match std::fs::read_to_string(&path) {
            Ok(s) => {
                serde_json::from_str::<SessionsFile>(&s)
                    .map_err(|e| anyhow::anyhow!("parsing `{}`: {e}", path.display()))?
                    .sessions
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => anyhow::bail!("reading `{}`: {e}", path.display()),
        };
        let mut cfg = crate::cli::team::read_user_config(home)?;
        let mut store = Store {
            sessions,
            remote: cfg.remote.clone().unwrap_or_default(),
        };
        let (r, wrote) = edit(&mut store);
        if wrote.sessions {
            crate::agent_store::write_typed_config(
                &path,
                &SessionsFile {
                    sessions: store.sessions,
                },
            )?;
        }
        if wrote.remote {
            cfg.remote = Some(store.remote);
            crate::cli::team::write_user_config(home, &cfg)?;
        }
        Ok(r)
    }

    // ---- the token ----

    /// This machine's token, minted on first use and kept at user
    /// level from then on. Read under the same lock as everything
    /// else, so two TUIs minting at once settle on one token rather
    /// than overwriting each other.
    pub(crate) fn token(&self) -> anyhow::Result<String> {
        self.with_remote(|remote| match &remote.token {
            Some(t) if !t.is_empty() => (t.clone(), false),
            _ => {
                let minted = random_token();
                remote.token = Some(minted.clone());
                (minted, true)
            }
        })
    }

    /// Mint a new token and end every session the old one opened,
    /// as ONE transition: rotation is the revocation of the
    /// credential itself, so a reader must never see the new token
    /// beside the old one's sessions.
    pub(crate) fn rotate_token(&self) -> anyhow::Result<String> {
        let minted = random_token();
        let fresh = minted.clone();
        self.with_store(move |store| {
            store.sessions.clear();
            store.remote.token = Some(fresh);
            (
                (),
                Wrote {
                    sessions: true,
                    remote: true,
                },
            )
        })?;
        self.refresh();
        Ok(minted)
    }

    /// Verify `presented` and open a session for it in ONE hold of
    /// the lock, so a rotation cannot land between the two and leave
    /// the old credential holding a live session (codex on f71e7b9).
    /// `None` is a token that is not this machine's.
    pub(crate) fn open_session_for_token(&self, presented: &str) -> anyhow::Result<Option<String>> {
        let cookie = random_token();
        let now = rfc3339(now_utc());
        let record = SessionRecord {
            id_hash: hex(&hash_token(&cookie)),
            how: "token".to_string(),
            created: now.clone(),
            last_seen: now,
        };
        let opened = self.with_store(move |store| {
            let ours = match store.remote.token.clone() {
                Some(t) if !t.is_empty() => t,
                // Nothing minted yet: mint here rather than admit,
                // so a first paste cannot race the first mint.
                _ => {
                    let minted = random_token();
                    store.remote.token = Some(minted.clone());
                    minted
                }
            };
            let minted_now = store.remote.token.as_deref() != Some(ours.as_str());
            if !admits(&ours, presented) {
                return (
                    None,
                    Wrote {
                        sessions: false,
                        remote: !minted_now,
                    },
                );
            }
            store.sessions.push(record);
            (
                Some(cookie),
                Wrote {
                    sessions: true,
                    remote: true,
                },
            )
        })?;
        if opened.is_some() {
            self.refresh();
        }
        Ok(opened)
    }

    /// Whether `presented` is this machine's token, as a question
    /// on its own. TEST-ONLY on purpose: answering it without also
    /// opening the session leaves a gap a rotation can land in, so
    /// production has no way to ask it separately — the only
    /// admission is [`Door::open_session_for_token`], which does
    /// both under one lock (codex on f71e7b9).
    #[cfg(test)]
    pub(crate) fn admits_token(&self, presented: &str) -> anyhow::Result<bool> {
        let ours = self.token()?;
        Ok(admits(&ours, presented))
    }

    /// Read the live set again and, if it is not what it was, tell
    /// the streams. The remote polls this so another TUI's
    /// revocation, or an expiry, reaches a stream that would
    /// otherwise wait for a request the browser never makes.
    pub(crate) fn refresh(&self) {
        let now = now_utc();
        let live = self.with_sessions(|sessions| {
            let changed = expire(sessions, now);
            (
                sessions
                    .iter()
                    .map(|s| s.id_hash.clone())
                    .collect::<std::collections::BTreeSet<_>>(),
                changed,
            )
        });
        let live = match live {
            Ok(live) => live,
            Err(e) => {
                eprintln!("remote: {e:#}");
                return;
            }
        };
        let mut inner = self.lock();
        if inner.live != live {
            inner.live = live;
            self.revoked.send_modify(|n| *n += 1);
        }
    }

    // ---- sessions ----

    /// The session a `Cookie` header carries, if it is one this door
    /// opened and it has not been revoked or expired. Being seen
    /// updates the record, to the minute. A store that cannot be
    /// read admits nobody.
    pub(crate) fn admit(&self, cookie_header: Option<&str>) -> Option<Admitted> {
        let token = cookie_value(cookie_header?, COOKIE)?;
        let hash = hash_token(&token);
        let now = now_utc();
        let found = self.with_sessions(|sessions| {
            let mut changed = expire(sessions, now);
            let Some(record) = sessions
                .iter_mut()
                .find(|s| hex_to_hash(&s.id_hash).is_some_and(|h| bool::from(h.ct_eq(&hash))))
            else {
                return (None, changed);
            };
            let seen = rfc3339(now);
            if record.last_seen.get(..16) != seen.get(..16) {
                record.last_seen = seen;
                changed = true;
            }
            (
                Some(Admitted {
                    id_hash: record.id_hash.clone(),
                    expires: deadline(record, now),
                }),
                changed,
            )
        });
        let admitted = match found {
            Ok(a) => a,
            Err(e) => {
                eprintln!("remote: {e:#}");
                None
            }
        };
        self.refresh();
        admitted
    }

    /// Open a session, `how` saying what opened it; the cookie
    /// value, handed out once. Not opened at all if it cannot be
    /// written.
    pub(crate) fn open_session(&self, how: &str) -> anyhow::Result<String> {
        let token = random_token();
        let now = rfc3339(now_utc());
        let record = SessionRecord {
            id_hash: hex(&hash_token(&token)),
            how: how.to_string(),
            created: now.clone(),
            last_seen: now,
        };
        self.with_sessions(|sessions| {
            sessions.push(record);
            ((), true)
        })?;
        self.refresh();
        Ok(token)
    }

    pub(crate) fn sessions(&self) -> anyhow::Result<Vec<SessionRecord>> {
        let now = now_utc();
        self.with_sessions(|sessions| {
            let changed = expire(sessions, now);
            (sessions.clone(), changed)
        })
    }

    /// Whether a session is still open — what a stream asks after
    /// each change of the live set.
    pub(crate) fn is_live(&self, id_hash: &str) -> bool {
        self.with_sessions(|sessions| (sessions.iter().any(|s| s.id_hash == id_hash), false))
            .unwrap_or(false)
    }

    /// Close a session: its streams end, its cookie admits nothing.
    /// An error means it is NOT closed — the file still has it.
    pub(crate) fn revoke_session(&self, id_hash: &str) -> anyhow::Result<()> {
        self.with_sessions(|sessions| {
            let before = sessions.len();
            sessions.retain(|s| s.id_hash != id_hash);
            ((), sessions.len() != before)
        })?;
        self.refresh();
        Ok(())
    }

    // ---- links ----

    /// Mint a token for one of the doors: five minutes, one use.
    pub(crate) fn mint(&self, kind: Link) -> String {
        let token = random_token();
        let mut inner = self.lock();
        let now = tokio::time::Instant::now();
        inner.minted.retain(|t| t.expires > now);
        inner.minted.push(Minted {
            kind,
            hash: hash_token(&token),
            expires: now + LINK_TTL,
        });
        token
    }

    /// Spend a token; true if it was minted for `kind`, unspent and
    /// unexpired.
    pub(crate) fn consume(&self, kind: Link, token: &str) -> bool {
        let hash = hash_token(token);
        let mut inner = self.lock();
        let now = tokio::time::Instant::now();
        inner.minted.retain(|t| t.expires > now);
        let at = inner
            .minted
            .iter()
            .position(|t| t.kind == kind && bool::from(t.hash.ct_eq(&hash)));
        match at {
            Some(i) => {
                inner.minted.remove(i);
                true
            }
            None => false,
        }
    }

    // ---- rate limit ----

    /// Whether `peer` may make another login attempt this minute.
    pub(crate) fn attempt_allowed(&self, peer: IpAddr) -> bool {
        let mut inner = self.lock();
        let now = tokio::time::Instant::now();
        let minute = std::time::Duration::from_secs(60);
        inner.attempts.retain(|_, (start, _)| now - *start < minute);
        let (_, count) = inner.attempts.entry(peer).or_insert((now, 0));
        *count += 1;
        *count <= ATTEMPTS_PER_MINUTE
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many times the store has been opened so far.
    #[cfg(test)]
    pub(crate) fn transactions(&self) -> usize {
        self.transactions.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Move a session's creation back, so its expiry is soon: for the
    /// tests of what expiry ends.
    #[cfg(test)]
    pub(crate) fn backdate(&self, id_hash: &str, by: time::Duration) -> anyhow::Result<()> {
        self.with_sessions(|sessions| {
            for s in sessions.iter_mut().filter(|s| s.id_hash == id_hash) {
                if let Some(c) = parse_rfc3339(&s.created) {
                    s.created = rfc3339(c - by);
                }
            }
            ((), true)
        })
    }
}

/// Whole, and in constant time.
fn admits(ours: &str, presented: &str) -> bool {
    bool::from(hash_token(ours).ct_eq(&hash_token(presented)))
}

/// Drop the sessions past their term; whether any were.
fn expire(sessions: &mut Vec<SessionRecord>, now: time::OffsetDateTime) -> bool {
    let before = sessions.len();
    sessions.retain(|s| {
        parse_rfc3339(&s.created).is_some_and(|c| now - c < time::Duration::days(SESSION_DAYS))
    });
    sessions.len() != before
}

/// When a session's term ends, as an instant a stream can sleep to.
fn deadline(record: &SessionRecord, now: time::OffsetDateTime) -> tokio::time::Instant {
    let left = parse_rfc3339(&record.created)
        .map(|c| c + time::Duration::days(SESSION_DAYS) - now)
        .and_then(|d| std::time::Duration::try_from(d).ok())
        .unwrap_or_default();
    tokio::time::Instant::now() + left
}

/// The one lock every TUI of this user takes around a read-edit-write
/// of the sessions file or the `remote` section. BLOCKING: the
/// sections are one small file each.
fn store_lock(home: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let dir = home.join(".clank");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("remote.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("opening `{}`: {e}", path.display()))?;
    // SAFETY: valid owned fd; flock has no memory effects.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        anyhow::bail!(
            "locking `{}`: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(file)
}

/// The `Set-Cookie` that carries a session: unreadable to scripts,
/// never sent cross-site, and `Secure` when it is for the tunnel's
/// host — plain loopback would never send it back.
pub(crate) fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        SESSION_DAYS * 24 * 60 * 60,
        if secure { "; Secure" } else { "" }
    )
}

/// One cookie's value out of a `Cookie` header.
fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// The `t=` of a `/login?t=…` or `/register?t=…`.
pub(crate) fn query_token(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "t").then(|| v.to_string())
    })
}

pub(crate) fn random_token() -> String {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn fill_random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("the OS random source answers");
}

fn hash_token(token: &str) -> [u8; 32] {
    *blake3::hash(token.as_bytes()).as_bytes()
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_to_hash(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

fn now_utc() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn parse_rfc3339(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

/// Where the sessions file is, for the operator and the tests.
pub(crate) fn sessions_path(home: &Path) -> PathBuf {
    home.join(SESSIONS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(token: &str) -> String {
        format!("other=1; {COOKIE}={token}")
    }

    /// A link is for one use, five minutes.
    #[tokio::test(start_paused = true)]
    async fn a_link_admits_once_and_not_after_five_minutes() {
        let door = Door::new(None);
        let t = door.mint(Link::Login);
        assert!(door.consume(Link::Login, &t));
        assert!(!door.consume(Link::Login, &t), "spent");
        let late = door.mint(Link::Login);
        tokio::time::advance(LINK_TTL + std::time::Duration::from_secs(1)).await;
        assert!(!door.consume(Link::Login, &late), "expired");
        assert!(!door.consume(Link::Login, "not-a-token"));
    }

    /// The token is minted once, kept at user level, and read back
    /// by any door on the same home — another repo's remote, or a
    /// fresh process. Only the whole token admits.
    #[tokio::test]
    async fn the_token_is_the_users_and_outlives_the_process() {
        let home = tempfile::tempdir().unwrap();
        let door = Door::new(Some(home.path().to_path_buf()));
        let token = door.token().unwrap();
        assert!(token.len() >= 32, "a guessable token is no token");
        assert_eq!(door.token().unwrap(), token, "minted once, not per call");

        // Another repo's remote on this machine, and a fresh
        // process, take the same one: it lives in the user config.
        let elsewhere = Door::new(Some(home.path().to_path_buf()));
        assert_eq!(elsewhere.token().unwrap(), token);
        let cfg = crate::cli::team::read_user_config(home.path()).unwrap();
        assert_eq!(cfg.remote.unwrap().token.as_deref(), Some(token.as_str()));

        assert!(door.admits_token(&token).unwrap());
        assert!(!door.admits_token("").unwrap());
        assert!(!door.admits_token(&token[..token.len() - 1]).unwrap());
        assert!(
            !door.admits_token(&format!("{token}x")).unwrap(),
            "a prefix of the token is not the token"
        );
    }

    /// Rotating mints a new token, refuses the old one, and ends
    /// every session the old one opened — the credential's own
    /// revocation.
    #[tokio::test]
    async fn rotating_the_token_ends_the_sessions_it_opened() {
        let home = tempfile::tempdir().unwrap();
        let door = Door::new(Some(home.path().to_path_buf()));
        let token = door.token().unwrap();
        let cookie_a = door.open_session("token").unwrap();
        let cookie_b = door.open_session("token").unwrap();
        assert!(door.admit(Some(&cookie(&cookie_a))).is_some());
        assert_eq!(door.sessions().unwrap().len(), 2);
        let watch = door.watch_revocations();

        let fresh = door.rotate_token().unwrap();
        assert_ne!(fresh, token);
        assert!(
            !door.admits_token(&token).unwrap(),
            "the old one is refused"
        );
        assert!(door.admits_token(&fresh).unwrap());
        assert!(door.sessions().unwrap().is_empty(), "its sessions are gone");
        assert!(door.admit(Some(&cookie(&cookie_a))).is_none());
        assert!(door.admit(Some(&cookie(&cookie_b))).is_none());
        assert!(watch.has_changed().unwrap(), "open streams are told");
        assert_eq!(
            Door::new(Some(home.path().to_path_buf())).token().unwrap(),
            fresh,
            "the rotation is persisted"
        );
    }

    /// A login that verified against the old token must not land a
    /// session after another TUI has rotated. Verification and the
    /// session are ONE hold of the lock, so the interleaving that
    /// used to hand out a live session for a dead credential cannot
    /// be constructed: whichever order the two doors run in, a
    /// rotation leaves NO session behind (codex on f71e7b9).
    #[tokio::test]
    async fn a_login_cannot_straddle_a_rotation() {
        let home = tempfile::tempdir().unwrap();
        let a = Door::new(Some(home.path().to_path_buf()));
        let b = Door::new(Some(home.path().to_path_buf()));
        let old = a.token().unwrap();

        // Verification and the session are ONE hold of the store.
        // This is the assertion a split cannot satisfy: with two
        // holds there is a gap, and a rotation landing in it hands
        // out a session for a credential that no longer exists.
        let before = a.transactions();
        let opened = a.open_session_for_token(&old).unwrap().expect("admitted");
        assert_eq!(
            a.transactions() - before,
            2,
            "one transaction verifies AND opens together, one refreshes \
             the live set for the streams; a split verify/open is three"
        );
        assert!(a.admit(Some(&cookie(&opened))).is_some());
        b.rotate_token().unwrap();
        assert!(
            a.admit(Some(&cookie(&opened))).is_none(),
            "the rotation ended it"
        );
        assert!(a.sessions().unwrap().is_empty());

        // The rotation lands first: the old token opens nothing at
        // all, so there is no session to be left behind.
        let older = b.token().unwrap();
        b.rotate_token().unwrap();
        assert!(
            a.open_session_for_token(&older).unwrap().is_none(),
            "a spent credential opens nothing"
        );
        assert!(a.sessions().unwrap().is_empty(), "and leaves nothing");
        let now = b.token().unwrap();
        assert!(a.open_session_for_token(&now).unwrap().is_some());
    }

    /// The file keeps the hash and never the cookie; a revoked
    /// session admits nothing and its watchers are told; a record
    /// older than the term is gone when the door opens.
    #[tokio::test]
    async fn a_session_is_hashed_on_disk_and_ends_when_revoked_or_old() {
        let home = tempfile::tempdir().unwrap();
        let door = Door::new(Some(home.path().to_path_buf()));
        assert!(door.admit(Some("clank_session=nothing")).is_none());
        let token = door.open_session("token").unwrap();
        let file = std::fs::read_to_string(sessions_path(home.path())).unwrap();
        assert!(!file.contains(&token), "the cookie is not on disk");
        let admitted = door.admit(Some(&cookie(&token))).expect("admitted");
        assert!(file.contains(&admitted.id_hash));
        assert_eq!(door.sessions().unwrap().len(), 1);
        assert_eq!(door.sessions().unwrap()[0].how, "token");
        assert!(door.is_live(&admitted.id_hash));

        let watch = door.watch_revocations();
        door.revoke_session(&admitted.id_hash).unwrap();
        assert!(watch.has_changed().unwrap(), "watchers are woken");
        assert!(!door.is_live(&admitted.id_hash));
        assert!(door.admit(Some(&cookie(&token))).is_none());
        assert!(door.sessions().unwrap().is_empty());

        // A reopened door reads the file: a session from another
        // process is admitted, an old one is not.
        let fresh = Door::new(Some(home.path().to_path_buf()));
        let token = fresh.open_session("token").unwrap();
        let old = time::OffsetDateTime::now_utc() - time::Duration::days(SESSION_DAYS + 1);
        let mut records = fresh.sessions().unwrap();
        records.push(SessionRecord {
            id_hash: "00".repeat(32),
            how: "stale".into(),
            created: rfc3339(old),
            last_seen: rfc3339(old),
        });
        crate::agent_store::write_typed_config(
            &sessions_path(home.path()),
            &SessionsFile { sessions: records },
        )
        .unwrap();
        let reopened = Door::new(Some(home.path().to_path_buf()));
        assert!(reopened.admit(Some(&cookie(&token))).is_some());
        assert_eq!(
            reopened.sessions().unwrap().len(),
            1,
            "the stale one is gone"
        );
    }

    /// A session written before the token — its field named
    /// `passkey` — is still readable rather than locking the user out
    /// of every open session on upgrade.
    #[test]
    fn a_session_written_before_the_token_still_reads() {
        let file: SessionsFile = serde_json::from_str(
            r#"{"sessions":[{"id_hash":"ab","passkey":"phone","created":"c","last_seen":"s"}]}"#,
        )
        .unwrap();
        assert_eq!(file.sessions[0].how, "phone");
    }

    /// Two TUIs, one user: a session opened by one is admitted by
    /// the other; revoked by the other, it is refused by the first
    /// at once and its watch is bumped by the next refresh; and the
    /// first, writing afterwards, does not bring it back. A store
    /// that cannot be written revokes nothing and says so.
    #[tokio::test]
    async fn two_doors_share_one_authority() {
        let home = tempfile::tempdir().unwrap();
        let a = Door::new(Some(home.path().to_path_buf()));
        let b = Door::new(Some(home.path().to_path_buf()));
        let token = a.open_session("token").unwrap();
        let admitted = b
            .admit(Some(&cookie(&token)))
            .expect("b admits a's session");
        let mut watch = a.watch_revocations();
        b.revoke_session(&admitted.id_hash).unwrap();
        assert!(
            a.admit(Some(&cookie(&token))).is_none(),
            "a refuses it at once"
        );
        assert!(!a.is_live(&admitted.id_hash));
        assert!(
            watch.has_changed().unwrap(),
            "a's own read tells its streams"
        );
        watch.mark_unchanged();
        let other = a.open_session("token").unwrap();
        let file = std::fs::read_to_string(sessions_path(home.path())).unwrap();
        assert!(
            !file.contains(&admitted.id_hash),
            "a's write resurrects nothing"
        );
        let laptop = b.admit(Some(&cookie(&other))).expect("b admits it");
        watch.mark_unchanged();
        b.revoke_session(&laptop.id_hash).unwrap();
        assert!(!watch.has_changed().unwrap(), "a has not read yet");
        a.refresh();
        assert!(
            watch.has_changed().unwrap(),
            "the poll's refresh tells a's streams"
        );

        let path = sessions_path(home.path());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            b.admit(Some(&cookie(&other))).is_none(),
            "an unreadable store admits nobody"
        );
        assert!(a.revoke_session("anything").is_err());
        assert!(a.open_session("token").is_err());
        assert!(a.sessions().is_err());
    }

    /// Ten in a minute per address, then no more until the minute
    /// is up.
    #[tokio::test(start_paused = true)]
    async fn attempts_are_ten_a_minute_per_address() {
        let door = Door::new(None);
        let a: IpAddr = "127.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        for _ in 0..ATTEMPTS_PER_MINUTE {
            assert!(door.attempt_allowed(a));
        }
        assert!(!door.attempt_allowed(a));
        assert!(door.attempt_allowed(b), "another address has its own");
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        assert!(door.attempt_allowed(a));
    }

    #[test]
    fn the_cookie_is_read_out_of_the_header_and_set_when_secure() {
        assert_eq!(
            cookie_value("a=1; clank_session=tok ; b=2", COOKIE).as_deref(),
            Some("tok")
        );
        assert_eq!(cookie_value("a=1", COOKIE), None);
        assert_eq!(query_token(Some("x=1&t=abc")).as_deref(), Some("abc"));
        assert_eq!(query_token(None), None);
        let plain = session_cookie("tok", false);
        assert!(plain.contains("HttpOnly") && plain.contains("SameSite=Strict"));
        assert!(!plain.contains("Secure"));
        assert!(session_cookie("tok", true).ends_with("; Secure"));
    }
}
