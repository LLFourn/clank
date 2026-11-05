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

pub const SESSION_DAYS: i64 = 30;
/// A minted link is good for this long, and for one use.
pub const LINK_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// Login attempts one address may make in a minute before it is
/// refused for the rest of it.
pub const ATTEMPTS_PER_MINUTE: u32 = 10;
pub const COOKIE: &str = "clank_session";
const SESSIONS_FILE: &str = ".clank/remote-sessions.json";
const LINKS_FILE: &str = ".clank/remote-links.json";

/// A one-time link the TUI minted: the way in that saves a paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Link {
    Login,
}

/// One open session as the file keeps it: the hash of the cookie,
/// never the cookie.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub id_hash: String,
    /// How it was opened — the pasted token, or a minted link. The
    /// serde alias keeps sessions written before the token readable.
    #[serde(alias = "passkey")]
    pub how: String,
    pub created: String,
    pub last_seen: String,
}

#[derive(Default, Serialize, Deserialize)]
struct SessionsFile {
    #[serde(default)]
    sessions: Vec<SessionRecord>,
}

pub use crate::config::RemoteSection;

/// A session the door admitted: what the routes carry.
#[derive(Debug, Clone)]
pub struct Admitted {
    pub id_hash: String,
    /// When its term ends: a stream holding it ends then.
    pub expires: tokio::time::Instant,
}

/// A one-time link the TUI minted.
struct Minted {
    kind: Link,
    hash: [u8; 32],
    expires: tokio::time::Instant,
}

#[derive(Default, Serialize, Deserialize)]
struct SharedLinks {
    #[serde(default)]
    links: Vec<SharedLink>,
}

#[derive(Serialize, Deserialize)]
struct SharedLink {
    kind: Link,
    hash: String,
    expires_unix: i64,
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
    token: Option<String>,
}

/// Both halves of the store, as one locked read gives them.
struct Store {
    sessions: Vec<SessionRecord>,
    token: Option<String>,
}

/// Which halves an edit changed, so only those are written.
#[derive(Default)]
struct Wrote {
    sessions: bool,
    token: bool,
}

/// The remote's authority is the user's, not one TUI's: every TUI
/// reads and writes the same two files under one lock, and holds no
/// copy of them, so a revocation in one is refused by the next read
/// in another and a write never resurrects what a sibling removed
/// (codex on 5d24c05).
pub struct Door {
    home: Option<PathBuf>,
    /// Where the token is kept; present exactly when `home` is.
    secrets: Option<std::sync::Arc<dyn crate::secret::Secrets>>,
    /// The user config's schema, which this door does not own: held
    /// to it, the door's `remote` edit is never written beside a
    /// setting the CLI cannot read.
    check: clank_user_config::Check,
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
pub struct Paste {
    pub token: String,
}

/// Why the door did not open, in words for the browser.
#[derive(Debug)]
pub enum Refused {
    /// The token is not this machine's. A SPENT LINK is not here: it
    /// redirects to the page saying so, which is where someone who
    /// followed a stale link can paste the token instead.
    Token,
    /// The store could not be read or written; nothing was changed.
    Store(String),
}

/// The token is in both the user config and its store, and they differ.
/// Refused rather than resolved: either could be the one a browser
/// holds, and picking wrongly locks the user out or lets a rotated-away
/// token back in.
#[derive(Debug)]
pub struct Conflict {
    pub config: PathBuf,
    pub store: String,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the remote token in `{}` (`remote.token`) differs from the one in {}; \
             the remote stays off until one is removed — delete `remote.token` from \
             the config to keep the stored one, or the stored one to keep the config's",
            self.config.display(),
            self.store
        )
    }
}

impl std::error::Error for Conflict {}

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
    /// The door for this user: the token in this platform's
    /// [`crate::secret::platform`] store, open sessions in
    /// `~/.clank/remote-sessions.json`, read on every use. Without a
    /// home nothing persists — a session lasts as long as the process.
    pub fn new(home: Option<PathBuf>, check: clank_user_config::Check) -> Self {
        match home {
            Some(home) => {
                let secrets = crate::secret::platform(&home);
                Self::with_secrets(home, check, secrets)
            }
            None => Self::build(None, None, check),
        }
    }

    /// [`Door::new`] for `home`, with the token kept in `secrets`.
    pub fn with_secrets(
        home: PathBuf,
        check: clank_user_config::Check,
        secrets: std::sync::Arc<dyn crate::secret::Secrets>,
    ) -> Self {
        Self::build(Some(home), Some(secrets), check)
    }

    fn build(
        home: Option<PathBuf>,
        secrets: Option<std::sync::Arc<dyn crate::secret::Secrets>>,
        check: clank_user_config::Check,
    ) -> Self {
        let door = Self {
            home,
            secrets,
            check,
            inner: Mutex::new(Inner::default()),
            revoked: tokio::sync::watch::channel(0).0,
            #[cfg(test)]
            transactions: std::sync::atomic::AtomicUsize::new(0),
        };
        door.refresh();
        door
    }

    pub fn watch_revocations(&self) -> tokio::sync::watch::Receiver<u64> {
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
        self.with_store(false, |store| {
            let (r, changed) = edit(&mut store.sessions);
            (
                r,
                Wrote {
                    sessions: changed,
                    token: false,
                },
            )
        })
    }

    /// The token, the same way, in its store.
    fn with_token<R>(
        &self,
        edit: impl FnOnce(&mut Option<String>) -> (R, bool),
    ) -> anyhow::Result<R> {
        self.with_store(true, |store| {
            let (r, changed) = edit(&mut store.token);
            (
                r,
                Wrote {
                    sessions: false,
                    token: changed,
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
    ///
    /// The token's store joins the same hold, and a token still in
    /// the user config, where every earlier clank kept it, is moved
    /// out first ([`Door::migrate`]). Unless `reads_token`, the store
    /// is not asked and the edit sees no token: the remote polls the
    /// sessions, and a keychain read on every poll is a keychain
    /// prompt on every poll when its access is not yet granted.
    fn with_store<R>(
        &self,
        reads_token: bool,
        edit: impl FnOnce(&mut Store) -> (R, Wrote),
    ) -> anyhow::Result<R> {
        #[cfg(test)]
        self.transactions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (Some(home), Some(secrets)) = (&self.home, &self.secrets) else {
            let mut inner = self.lock();
            let Memory { sessions, token } = &mut inner.memory;
            let mut store = Store {
                sessions: std::mem::take(sessions),
                token: token.clone(),
            };
            let (r, _) = edit(&mut store);
            *sessions = store.sessions;
            *token = store.token;
            return Ok(r);
        };
        let _lock = store_lock(home)?;
        let token = self.migrate(home, secrets.as_ref(), reads_token)?;
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
        let mut store = Store { sessions, token };
        let (r, wrote) = edit(&mut store);
        debug_assert!(reads_token || !wrote.token, "a token written unread");
        if wrote.sessions {
            write_typed_config(
                &path,
                &SessionsFile {
                    sessions: store.sessions,
                },
            )?;
        }
        if reads_token
            && wrote.token
            && let Some(token) = &store.token
        {
            secrets
                .write(token)
                .map_err(|e| store_error(secrets.as_ref(), "writing", e, &[token]))?;
        }
        Ok(r)
    }

    /// The token as its store holds it, after moving any copy out of
    /// the user config. Its own user-config transaction, committed
    /// before the caller's edit, so a later failure in that edit can
    /// never leave the config's copy beside a rotated store.
    ///
    /// The config's copy is removed only once the store has been read
    /// back holding it. Whatever fails, the config keeps it, and the
    /// remote is refused rather than run from plaintext or from a
    /// token minted to replace one that could not be read.
    fn migrate(
        &self,
        home: &Path,
        secrets: &dyn crate::secret::Secrets,
        reads_token: bool,
    ) -> anyhow::Result<Option<String>> {
        clank_user_config::transact(home, self.check, |cfg| {
            let remote: RemoteSection = cfg
                .get("remote")
                .cloned()
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
            let in_config = remote.token.filter(|t| !t.is_empty());
            if in_config.is_none() && !reads_token {
                return Ok(None);
            }
            let masks: Vec<&str> = in_config.iter().map(String::as_str).collect();
            let stored = secrets
                .read()
                .map_err(|e| store_error(secrets, "reading", e, &masks))?;
            let Some(legacy) = in_config.as_deref() else {
                return Ok(stored);
            };
            match stored {
                Some(stored) if admits(&stored, legacy) => {}
                Some(_) => {
                    return Err(Conflict {
                        config: clank_user_config::path(home),
                        store: secrets.describe(),
                    }
                    .into());
                }
                None => {
                    secrets
                        .write(legacy)
                        .map_err(|e| store_error(secrets, "moving the token into", e, &masks))?;
                    let back = secrets
                        .read()
                        .map_err(|e| store_error(secrets, "reading back", e, &masks))?;
                    anyhow::ensure!(
                        back.as_deref().is_some_and(|back| admits(back, legacy)),
                        "{} did not read back the token written to it; it stays in the \
                         user config and the remote stays off",
                        secrets.describe()
                    );
                }
            }
            // Removed from the document itself: a round trip through
            // `RemoteSection` would drop every field this build does
            // not know, beside the token and inside the tunnel.
            if let Some(remote) = cfg.get_mut("remote").and_then(|r| r.as_object_mut()) {
                remote.remove("token");
            }
            Ok(Some(legacy.to_string()))
        })
    }

    // ---- the token ----

    /// This machine's token, minted on first use and kept at user
    /// level from then on. Read under the same lock as everything
    /// else, so two TUIs minting at once settle on one token rather
    /// than overwriting each other.
    pub fn token(&self) -> anyhow::Result<String> {
        self.with_token(|token| match token {
            Some(t) if !t.is_empty() => (t.clone(), false),
            _ => {
                let minted = random_token();
                *token = Some(minted.clone());
                (minted, true)
            }
        })
    }

    /// The credential, to a session that is STILL one — the session
    /// checked and the token read under ONE hold of the lock, for the
    /// reason [`with_store`] gives. Admitting in one transaction and
    /// reading in another let a rotation land between them and hand
    /// the NEW token to the session that rotation had just revoked,
    /// which is a revoked device signing itself back in for good
    /// (codex on 1c932ab).
    ///
    /// `None` means the session is no longer one; the caller refuses
    /// as it would refuse any request without a session.
    pub fn token_for_session(&self, id_hash: &str) -> anyhow::Result<Option<String>> {
        self.with_store(true, |store| {
            let now = now_utc();
            let live = store
                .sessions
                .iter()
                .any(|s| s.id_hash == id_hash && is_fresh(s, now));
            if !live {
                return (None, Wrote::default());
            }
            match &store.token {
                Some(t) if !t.is_empty() => (Some(t.clone()), Wrote::default()),
                // First use mints it, as `token` does: a page asking
                // before anyone has looked at the TUI is not an error.
                _ => {
                    let minted = random_token();
                    store.token = Some(minted.clone());
                    (
                        Some(minted),
                        Wrote {
                            sessions: false,
                            token: true,
                        },
                    )
                }
            }
        })
    }

    /// Mint a new token and end every session the old one opened,
    /// as ONE transition: rotation is the revocation of the
    /// credential itself, so a reader must never see the new token
    /// beside the old one's sessions.
    pub fn rotate_token(&self) -> anyhow::Result<String> {
        let minted = random_token();
        let fresh = minted.clone();
        self.with_store(true, move |store| {
            store.sessions.clear();
            store.token = Some(fresh);
            (
                (),
                Wrote {
                    sessions: true,
                    token: true,
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
    pub fn open_session_for_token(&self, presented: &str) -> anyhow::Result<Option<String>> {
        let cookie = random_token();
        let now = rfc3339(now_utc());
        let record = SessionRecord {
            id_hash: hex(&hash_token(&cookie)),
            how: "token".to_string(),
            created: now.clone(),
            last_seen: now,
        };
        let opened = self.with_store(true, move |store| {
            let (ours, minted_now) = match store.token.clone() {
                Some(t) if !t.is_empty() => (t, false),
                // Nothing minted yet: mint here rather than admit,
                // so a first paste cannot race the first mint.
                _ => {
                    let minted = random_token();
                    store.token = Some(minted.clone());
                    (minted, true)
                }
            };
            if !admits(&ours, presented) {
                return (
                    None,
                    Wrote {
                        sessions: false,
                        token: minted_now,
                    },
                );
            }
            store.sessions.push(record);
            (
                Some(cookie),
                Wrote {
                    sessions: true,
                    token: minted_now,
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
    pub fn admits_token(&self, presented: &str) -> anyhow::Result<bool> {
        let ours = self.token()?;
        Ok(admits(&ours, presented))
    }

    /// Read the live set again and, if it is not what it was, tell
    /// the streams. The remote polls this so another TUI's
    /// revocation, or an expiry, reaches a stream that would
    /// otherwise wait for a request the browser never makes.
    pub fn refresh(&self) {
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
    pub fn admit(&self, cookie_header: Option<&str>) -> Option<Admitted> {
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
    pub fn open_session(&self, how: &str) -> anyhow::Result<String> {
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

    pub fn sessions(&self) -> anyhow::Result<Vec<SessionRecord>> {
        let now = now_utc();
        self.with_sessions(|sessions| {
            let changed = expire(sessions, now);
            (sessions.clone(), changed)
        })
    }

    /// Whether a session is still open — what a stream asks after
    /// each change of the live set.
    pub fn is_live(&self, id_hash: &str) -> bool {
        self.with_sessions(|sessions| (sessions.iter().any(|s| s.id_hash == id_hash), false))
            .unwrap_or(false)
    }

    /// Close a session: its streams end, its cookie admits nothing.
    /// An error means it is NOT closed — the file still has it.
    pub fn revoke_session(&self, id_hash: &str) -> anyhow::Result<()> {
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
    pub fn mint(&self, kind: Link) -> String {
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
    pub fn consume(&self, kind: Link, token: &str) -> bool {
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

    /// Mint a link in the user-level store, so a TUI can hand it to a
    /// separate server process. The raw token only leaves this call
    /// in the URL; the shared file retains its hash and expiry.
    pub fn mint_shared(&self, kind: Link) -> anyhow::Result<String> {
        let Some(home) = &self.home else {
            return Ok(self.mint(kind));
        };
        let token = random_token();
        let _lock = store_lock(home)?;
        let path = home.join(LINKS_FILE);
        let mut links = read_shared_links(&path)?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        links.links.retain(|link| link.expires_unix > now);
        links.links.push(SharedLink {
            kind,
            hash: hex(&hash_token(&token)),
            expires_unix: now + LINK_TTL.as_secs() as i64,
        });
        write_typed_config(&path, &links)?;
        Ok(token)
    }

    /// Consume a shared link under the same lock as minting. A failed
    /// write refuses the link so it cannot be spent twice.
    pub fn consume_shared(&self, kind: Link, token: &str) -> anyhow::Result<bool> {
        let Some(home) = &self.home else {
            return Ok(self.consume(kind, token));
        };
        let _lock = store_lock(home)?;
        let path = home.join(LINKS_FILE);
        let mut links = read_shared_links(&path)?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let hash = hash_token(token);
        links.links.retain(|link| link.expires_unix > now);
        let at = links.links.iter().position(|link| {
            link.kind == kind
                && hex_to_hash(&link.hash).is_some_and(|stored| bool::from(stored.ct_eq(&hash)))
        });
        if let Some(at) = at {
            links.links.remove(at);
        }
        if at.is_some() {
            write_typed_config(&path, &links)?;
        }
        Ok(at.is_some())
    }

    // ---- rate limit ----

    /// Whether `peer` may make another login attempt this minute.
    pub fn attempt_allowed(&self, peer: IpAddr) -> bool {
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
    pub fn transactions(&self) -> usize {
        self.transactions.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Move a session's creation back, so its expiry is soon: for the
    /// tests of what expiry ends.
    #[cfg(any(test, feature = "test-support"))]
    pub fn backdate(&self, id_hash: &str, by: time::Duration) -> anyhow::Result<()> {
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

/// A store's failure, in words that name the store and carry none of
/// `tokens`, whatever the store itself said.
fn store_error(
    secrets: &dyn crate::secret::Secrets,
    doing: &str,
    error: anyhow::Error,
    tokens: &[&str],
) -> anyhow::Error {
    let said = tokens
        .iter()
        .filter(|t| !t.is_empty())
        .fold(format!("{error:#}"), |said, t| {
            said.replace(t, "[redacted]")
        });
    anyhow::anyhow!(
        "{doing} {}: {said}; the remote stays off until it answers",
        secrets.describe()
    )
}

/// Whole, and in constant time.
fn admits(ours: &str, presented: &str) -> bool {
    bool::from(hash_token(ours).ct_eq(&hash_token(presented)))
}

/// Drop the sessions past their term; whether any were.
fn expire(sessions: &mut Vec<SessionRecord>, now: time::OffsetDateTime) -> bool {
    let before = sessions.len();
    sessions.retain(|s| is_fresh(s, now));
    sessions.len() != before
}

/// Whether a session is still within its term.
fn is_fresh(s: &SessionRecord, now: time::OffsetDateTime) -> bool {
    parse_rfc3339(&s.created).is_some_and(|c| now - c < time::Duration::days(SESSION_DAYS))
}

/// When a session's term ends, as an instant a stream can sleep to.
fn deadline(record: &SessionRecord, now: time::OffsetDateTime) -> tokio::time::Instant {
    let left = parse_rfc3339(&record.created)
        .map(|c| c + time::Duration::days(SESSION_DAYS) - now)
        .and_then(|d| std::time::Duration::try_from(d).ok())
        .unwrap_or_default();
    tokio::time::Instant::now() + left
}

#[cfg(test)]
fn user_config_path(home: &Path) -> PathBuf {
    clank_user_config::path(home)
}

/// Read the user document as an object so the server can update its
/// own `remote` section while preserving settings owned by the CLI.
pub(crate) fn read_user_config(
    home: &Path,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    clank_user_config::read(home)
}

pub(crate) fn write_typed_config<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn read_shared_links(path: &Path) -> anyhow::Result<SharedLinks> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(serde_json::from_str(&body)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SharedLinks::default()),
        Err(error) => Err(error.into()),
    }
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
pub fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        SESSION_DAYS * 24 * 60 * 60,
        if secure { "; Secure" } else { "" }
    )
}

/// The `Set-Cookie` that ENDS a session: the same cookie, spent. A
/// browser told to forget a device must not keep sending a name the
/// door has already revoked.
pub fn expired_cookie(secure: bool) -> String {
    format!(
        "{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
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
pub fn query_token(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "t").then(|| v.to_string())
    })
}

pub fn random_token() -> String {
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
pub fn sessions_path(home: &Path) -> PathBuf {
    home.join(SESSIONS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The part of the schema this crate owns, standing in for the CLI's.
    fn remote_reads(path: &Path, doc: &clank_user_config::Document) -> anyhow::Result<()> {
        if let Some(remote) = doc.get("remote") {
            serde_json::from_value::<RemoteSection>(remote.clone())
                .map_err(|e| clank_user_config::versioned::Refusal::malformed(path, e))?;
        }
        Ok(())
    }

    fn cookie(token: &str) -> String {
        format!("other=1; {COOKIE}={token}")
    }

    /// The door the tests use: the token in `home`'s own file, never a
    /// keychain.
    fn door_at(home: &Path) -> Door {
        Door::with_secrets(
            home.to_path_buf(),
            remote_reads,
            std::sync::Arc::new(crate::secret::TokenFile::at(home)),
        )
    }

    #[test]
    fn editing_remote_preserves_the_rest_of_user_config() {
        let home = tempfile::tempdir().unwrap();
        let original = serde_json::json!({
            "auto": "on",
            "agents": {"example": {"tool": "codex"}},
            "future_setting": {"nested": [1, 2, 3]},
            "remote": {"tunnel": {"provider": "quick"}}
        });
        write_typed_config(&user_config_path(home.path()), &original).unwrap();
        let door = door_at(home.path());
        let token = door.token().unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(user_config_path(home.path())).unwrap()).unwrap();
        assert_eq!(saved["auto"], original["auto"]);
        assert_eq!(saved["agents"], original["agents"]);
        assert_eq!(saved["future_setting"], original["future_setting"]);
        assert_eq!(saved["remote"]["tunnel"], original["remote"]["tunnel"]);
        assert!(saved["remote"].get("token").is_none(), "not in the config");
        assert!(!serde_json::to_string(&saved).unwrap().contains(&token));
        assert_eq!(
            crate::secret::Secrets::read(&crate::secret::TokenFile::at(home.path()))
                .unwrap()
                .as_deref(),
            Some(token.as_str())
        );
    }

    /// A link is for one use, five minutes.
    #[tokio::test(start_paused = true)]
    async fn a_link_admits_once_and_not_after_five_minutes() {
        let door = Door::new(None, remote_reads);
        let t = door.mint(Link::Login);
        assert!(door.consume(Link::Login, &t));
        assert!(!door.consume(Link::Login, &t), "spent");
        let late = door.mint(Link::Login);
        tokio::time::advance(LINK_TTL + std::time::Duration::from_secs(1)).await;
        assert!(!door.consume(Link::Login, &late), "expired");
        assert!(!door.consume(Link::Login, "not-a-token"));
    }

    #[test]
    fn a_shared_link_crosses_door_processes_and_is_spent_once() {
        let home = tempfile::tempdir().unwrap();
        let tui = door_at(home.path());
        let host = door_at(home.path());
        let token = tui.mint_shared(Link::Login).unwrap();
        assert!(
            !std::fs::read_to_string(home.path().join(LINKS_FILE))
                .unwrap()
                .contains(&token)
        );
        assert!(host.consume_shared(Link::Login, &token).unwrap());
        assert!(!tui.consume_shared(Link::Login, &token).unwrap());
    }

    /// The token is minted once, kept at user level, and read back
    /// by any door on the same home — another repo's remote, or a
    /// fresh process. Only the whole token admits.
    #[tokio::test]
    async fn the_token_is_the_users_and_outlives_the_process() {
        let home = tempfile::tempdir().unwrap();
        let door = door_at(home.path());
        let token = door.token().unwrap();
        assert!(token.len() >= 32, "a guessable token is no token");
        assert_eq!(door.token().unwrap(), token, "minted once, not per call");

        // Another repo's remote on this machine, and a fresh
        // process, take the same one: it lives in the user's store.
        let elsewhere = door_at(home.path());
        assert_eq!(elsewhere.token().unwrap(), token);

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
        let door = door_at(home.path());
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
            door_at(home.path()).token().unwrap(),
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
        let a = door_at(home.path());
        let b = door_at(home.path());
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

    /// The credential goes only to a session that is still one, and
    /// the check and the read are ONE hold of the store. Two holds
    /// let a rotation land between them: the first accepts the old
    /// session, the rotation revokes it, and the second hands that
    /// revoked session the NEW token — which it could then sign back
    /// in with, past the revocation that was the point of rotating
    /// (codex on 1c932ab).
    #[tokio::test]
    async fn a_revoked_session_is_handed_no_token() {
        let home = tempfile::tempdir().unwrap();
        let a = door_at(home.path());
        let b = door_at(home.path());
        let opened = a.open_session("token").unwrap();
        let id_hash = a.admit(Some(&cookie(&opened))).expect("admitted").id_hash;

        let before = a.transactions();
        assert_eq!(
            a.token_for_session(&id_hash).unwrap().as_deref(),
            Some(a.token().unwrap().as_str()),
            "a live session is handed the credential"
        );
        assert_eq!(
            a.transactions() - before,
            2,
            "one transaction checks AND reads together; the second is \
             this assertion's own read of the token"
        );

        // The interleaving: admitted, then rotated elsewhere.
        let fresh = b.rotate_token().unwrap();
        assert_eq!(
            a.token_for_session(&id_hash).unwrap(),
            None,
            "the session the rotation ended is handed nothing"
        );
        assert_ne!(fresh, "", "and the new credential is intact");
        assert_eq!(b.token().unwrap(), fresh);

        // A session opened on the new credential is handed it.
        let after = a.open_session_for_token(&fresh).unwrap().expect("admitted");
        let id_hash = a.admit(Some(&cookie(&after))).expect("admitted").id_hash;
        assert_eq!(
            a.token_for_session(&id_hash).unwrap().as_deref(),
            Some(fresh.as_str())
        );

        // And a session that was revoked on its own is handed nothing.
        a.revoke_session(&id_hash).unwrap();
        assert_eq!(a.token_for_session(&id_hash).unwrap(), None);
    }

    /// The file keeps the hash and never the cookie; a revoked
    /// session admits nothing and its watchers are told; a record
    /// older than the term is gone when the door opens.
    #[tokio::test]
    async fn a_session_is_hashed_on_disk_and_ends_when_revoked_or_old() {
        let home = tempfile::tempdir().unwrap();
        let door = door_at(home.path());
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
        let fresh = door_at(home.path());
        let token = fresh.open_session("token").unwrap();
        let old = time::OffsetDateTime::now_utc() - time::Duration::days(SESSION_DAYS + 1);
        let mut records = fresh.sessions().unwrap();
        records.push(SessionRecord {
            id_hash: "00".repeat(32),
            how: "stale".into(),
            created: rfc3339(old),
            last_seen: rfc3339(old),
        });
        write_typed_config(
            &sessions_path(home.path()),
            &SessionsFile { sessions: records },
        )
        .unwrap();
        let reopened = door_at(home.path());
        assert!(reopened.admit(Some(&cookie(&token))).is_some());
        assert_eq!(
            reopened.sessions().unwrap().len(),
            1,
            "the stale one is gone"
        );
    }

    /// Frozen bytes (durable-formats-survive-upgrade): a session opened
    /// before the token, under its old `passkey` field, and one after.
    /// Created in 2099 so that expiry, which reads the clock, keeps them.
    const FROZEN_SESSIONS: &str =
        include_str!("../../cli/tests/fixtures/formats/remote-sessions.json");
    const FROZEN_LINKS: &str = include_str!("../../cli/tests/fixtures/formats/remote-links.json");
    const OLD_SESSION: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const NEW_SESSION: &str = "0000000000000000000000000000000000000000000000000000000000000002";

    #[test]
    fn the_frozen_session_store_reads_and_keeps_its_old_session_through_a_write() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        std::fs::write(sessions_path(home.path()), FROZEN_SESSIONS).unwrap();
        let door = door_at(home.path());
        assert!(door.is_live(OLD_SESSION) && door.is_live(NEW_SESSION));

        door.revoke_session(NEW_SESSION).unwrap();
        assert!(door.is_live(OLD_SESSION) && !door.is_live(NEW_SESSION));
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(sessions_path(home.path())).unwrap()).unwrap();
        assert_eq!(
            stored,
            serde_json::json!({"sessions": [{
                "id_hash": OLD_SESSION,
                "how": "passkey",
                "created": "2099-10-01T00:00:00Z",
                "last_seen": "2099-10-02T00:00:00Z"
            }]})
        );
    }

    #[test]
    fn the_frozen_link_store_reads_and_keeps_its_link_through_a_write() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        let path = home.path().join(LINKS_FILE);
        std::fs::write(&path, FROZEN_LINKS).unwrap();
        let door = door_at(home.path());
        assert!(!door.consume_shared(Link::Login, "not-the-token").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FROZEN_LINKS);

        let token = door.mint_shared(Link::Login).unwrap();
        let frozen: serde_json::Value = serde_json::from_str(FROZEN_LINKS).unwrap();
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let links = stored["links"].as_array().unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0], frozen["links"][0], "the frozen link is kept");
        assert!(door.consume_shared(Link::Login, &token).unwrap());
    }

    #[test]
    fn the_door_never_writes_over_a_user_config_it_cannot_read() {
        for body in ["{\"remote\": {", "{\"version\": 2, \"remote\": {}}"] {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".clank")).unwrap();
            std::fs::write(user_config_path(home.path()), body).unwrap();
            let door = door_at(home.path());
            for (what, result) in [("token", door.token()), ("rotate", door.rotate_token())] {
                let err = result.expect_err(what);
                assert!(
                    err.chain().any(|e| e
                        .downcast_ref::<clank_user_config::versioned::Refusal>()
                        .is_some()),
                    "{what}: {err:#}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(user_config_path(home.path())).unwrap(),
                body
            );
        }
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
        let a = door_at(home.path());
        let b = door_at(home.path());
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

    const LEGACY: &str = "PLACEHOLDER-LEGACY-TOKEN";
    const OTHER: &str = "PLACEHOLDER-OTHER-TOKEN";

    /// A token store that can be made to fail, and whose failures
    /// quote the token, as a careless real one might.
    #[derive(Default)]
    struct Fake {
        held: Mutex<Option<String>>,
        writes: std::sync::atomic::AtomicUsize,
        fail_read: std::sync::atomic::AtomicBool,
        fail_write: std::sync::atomic::AtomicBool,
        reads_back_wrong: std::sync::atomic::AtomicBool,
    }

    impl Fake {
        fn holding(token: &str) -> std::sync::Arc<Self> {
            let fake = Self::default();
            *fake.held.lock().unwrap() = Some(token.into());
            std::sync::Arc::new(fake)
        }
        fn held(&self) -> Option<String> {
            self.held.lock().unwrap().clone()
        }
        fn writes(&self) -> usize {
            self.writes.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn set(flag: &std::sync::atomic::AtomicBool, on: bool) {
            flag.store(on, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl crate::secret::Secrets for Fake {
        fn read(&self) -> anyhow::Result<Option<String>> {
            use std::sync::atomic::Ordering::SeqCst;
            anyhow::ensure!(!self.fail_read.load(SeqCst), "the fake store is locked");
            if self.reads_back_wrong.load(SeqCst) && self.writes() > 0 {
                return Ok(Some(OTHER.into()));
            }
            Ok(self.held())
        }
        fn write(&self, token: &str) -> anyhow::Result<()> {
            use std::sync::atomic::Ordering::SeqCst;
            anyhow::ensure!(
                !self.fail_write.load(SeqCst),
                "the fake store refused to keep {token}"
            );
            self.writes.fetch_add(1, SeqCst);
            *self.held.lock().unwrap() = Some(token.into());
            Ok(())
        }
        fn describe(&self) -> String {
            "the fake store".into()
        }
    }

    fn door_with(home: &Path, fake: &std::sync::Arc<Fake>) -> Door {
        Door::with_secrets(home.to_path_buf(), remote_reads, fake.clone())
    }

    /// A user config as every earlier clank left it: the token in it.
    fn legacy_config(home: &Path, token: &str) -> String {
        let body = serde_json::to_string_pretty(&serde_json::json!({
            "auto": "on",
            "remote": {"tunnel": {"provider": "quick"}, "token": token}
        }))
        .unwrap();
        std::fs::create_dir_all(home.join(".clank")).unwrap();
        std::fs::write(user_config_path(home), &body).unwrap();
        body
    }

    fn config_token(home: &Path) -> Option<String> {
        let cfg = read_user_config(home).unwrap();
        cfg.get("remote")?
            .get("token")?
            .as_str()
            .map(str::to_string)
    }

    fn quiet(err: &anyhow::Error) -> String {
        let said = format!("{err:#}");
        for token in [LEGACY, OTHER] {
            assert!(!said.contains(token), "a token in an error: {said}");
        }
        said
    }

    /// The user config around the token, as a newer clank might have
    /// written it: settings this build does not know, beside the token
    /// and inside the tunnel.
    fn config_with_unknowns(token: Option<&str>) -> serde_json::Value {
        let mut remote = serde_json::json!({
            "tunnel": {"provider": "command", "run": ["true"], "future_nested": "keep-me-too"},
            "future_setting": "keep-me"
        });
        if let Some(token) = token {
            remote["token"] = token.into();
        }
        serde_json::json!({"other": "kept", "remote": remote})
    }

    /// Moving the token out removes `remote.token` and nothing else,
    /// whether the store was empty or already held it, and again is a
    /// no-op.
    #[test]
    fn migration_removes_only_the_token() {
        for already_stored in [false, true] {
            let home = tempfile::tempdir().unwrap();
            write_typed_config(
                &user_config_path(home.path()),
                &config_with_unknowns(Some(LEGACY)),
            )
            .unwrap();
            let fake = if already_stored {
                Fake::holding(LEGACY)
            } else {
                std::sync::Arc::new(Fake::default())
            };
            for _ in 0..2 {
                assert_eq!(door_with(home.path(), &fake).token().unwrap(), LEGACY);
                let saved = serde_json::Value::Object(read_user_config(home.path()).unwrap());
                let mut want = config_with_unknowns(None);
                if let Some(version) = saved.get("version") {
                    want["version"] = version.clone();
                }
                assert_eq!(saved, want, "already stored: {already_stored}");
            }
            assert_eq!(fake.writes(), usize::from(!already_stored));
        }
    }

    type Items = std::collections::HashMap<(String, String), Vec<u8>>;

    #[derive(Clone, Default)]
    struct SharedKeychain(std::sync::Arc<Mutex<Items>>);

    impl crate::secret::KeychainApi for SharedKeychain {
        fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned())
        }
        fn set(&self, service: &str, account: &str, secret: &[u8]) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert((service.into(), account.into()), secret.to_vec());
            Ok(())
        }
    }

    /// One home spelled three ways shares one lock and one sessions
    /// file, so it must share one keychain item: a rotation through
    /// one spelling is the token every spelling admits.
    #[test]
    fn one_home_however_spelled_has_one_keychain_token() {
        let home = tempfile::tempdir().unwrap();
        let links = tempfile::tempdir().unwrap();
        let linked = links.path().join("home");
        std::os::unix::fs::symlink(home.path(), &linked).unwrap();
        let keychain = SharedKeychain::default();
        let doors: Vec<Door> = [home.path().to_path_buf(), home.path().join("."), linked]
            .into_iter()
            .map(|spelling| {
                let secrets = crate::secret::Keychain::new(keychain.clone(), &spelling);
                Door::with_secrets(spelling, remote_reads, std::sync::Arc::new(secrets))
            })
            .collect();
        let old = doors[0].token().unwrap();
        for door in &doors {
            assert_eq!(door.token().unwrap(), old);
        }
        assert_eq!(keychain.0.lock().unwrap().len(), 1, "one item");

        let fresh = doors[1].rotate_token().unwrap();
        for door in &doors {
            assert_eq!(door.token().unwrap(), fresh);
            assert!(door.open_session_for_token(&old).unwrap().is_none());
        }
        assert!(doors[2].open_session_for_token(&fresh).unwrap().is_some());
    }

    /// Two processes' doors meet a config still holding the token:
    /// one moves it, the other finds it moved, and the config keeps
    /// everything else.
    #[test]
    fn the_config_token_moves_to_the_store_once_under_concurrency() {
        let home = tempfile::tempdir().unwrap();
        legacy_config(home.path(), LEGACY);
        let fake = std::sync::Arc::new(Fake::default());
        let start = std::sync::Barrier::new(2);
        let seen: Vec<String> = std::thread::scope(|s| {
            let runs: Vec<_> = (0..2)
                .map(|_| {
                    s.spawn(|| {
                        let door = door_with(home.path(), &fake);
                        start.wait();
                        door.token().unwrap()
                    })
                })
                .collect();
            runs.into_iter().map(|r| r.join().unwrap()).collect()
        });
        assert_eq!(
            seen,
            [LEGACY, LEGACY],
            "the token the browser holds is kept"
        );
        assert_eq!(fake.writes(), 1, "one migration");
        assert_eq!(fake.held().as_deref(), Some(LEGACY));
        assert_eq!(config_token(home.path()), None);
        let cfg = read_user_config(home.path()).unwrap();
        assert_eq!(cfg["auto"], "on");
        assert_eq!(cfg["remote"]["tunnel"]["provider"], "quick");

        assert_eq!(door_with(home.path(), &fake).token().unwrap(), LEGACY);
        assert_eq!(fake.writes(), 1, "idempotent");
    }

    /// Both stores holding the same token is a migration that wrote
    /// the store and died before the config: finishing it removes the
    /// config's copy and writes nothing.
    #[test]
    fn an_equal_copy_in_the_config_is_removed() {
        let home = tempfile::tempdir().unwrap();
        legacy_config(home.path(), LEGACY);
        let fake = Fake::holding(LEGACY);
        assert_eq!(door_with(home.path(), &fake).token().unwrap(), LEGACY);
        assert_eq!(config_token(home.path()), None);
        assert_eq!(fake.writes(), 0);
    }

    #[test]
    fn a_failed_move_keeps_the_config_copy_and_refuses_the_remote() {
        let home = tempfile::tempdir().unwrap();
        let before = legacy_config(home.path(), LEGACY);
        let fake = std::sync::Arc::new(Fake::default());
        Fake::set(&fake.fail_write, true);
        let door = door_with(home.path(), &fake);
        let said = quiet(&door.token().expect_err("refused"));
        assert!(said.contains("the fake store"), "{said}");
        assert!(
            said.contains("[redacted]"),
            "the store's own words, masked: {said}"
        );
        quiet(&door.open_session_for_token(LEGACY).expect_err("refused"));
        quiet(&door.rotate_token().expect_err("refused"));
        assert_eq!(
            std::fs::read_to_string(user_config_path(home.path())).unwrap(),
            before,
            "the config keeps its copy"
        );
        assert_eq!(fake.held(), None);

        Fake::set(&fake.fail_write, false);
        assert_eq!(door.token().unwrap(), LEGACY, "resolved, it moves");
        assert_eq!(config_token(home.path()), None);
    }

    #[test]
    fn a_store_that_reads_back_wrong_keeps_the_config_copy() {
        let home = tempfile::tempdir().unwrap();
        legacy_config(home.path(), LEGACY);
        let fake = std::sync::Arc::new(Fake::default());
        Fake::set(&fake.reads_back_wrong, true);
        quiet(&door_with(home.path(), &fake).token().expect_err("refused"));
        assert_eq!(config_token(home.path()).as_deref(), Some(LEGACY));
    }

    /// Different tokens in the two stores: either could be the one a
    /// browser holds, so neither is picked and neither is touched.
    #[test]
    fn a_conflict_is_refused_and_changes_neither_store() {
        let home = tempfile::tempdir().unwrap();
        let fake = std::sync::Arc::new(Fake::default());
        let door = door_with(home.path(), &fake);
        let cookie_value = door.open_session("token").unwrap();
        let before = legacy_config(home.path(), LEGACY);
        *fake.held.lock().unwrap() = Some(OTHER.into());
        let sessions = std::fs::read_to_string(sessions_path(home.path())).unwrap();

        let err = door.token().expect_err("a conflict");
        let conflict = err.downcast_ref::<Conflict>().expect("named as a conflict");
        assert_eq!(conflict.config, user_config_path(home.path()));
        let said = quiet(&err);
        assert!(said.contains("the fake store") && said.contains("remote.token"));
        quiet(&door.rotate_token().expect_err("a conflict"));
        quiet(&door.open_session_for_token(OTHER).expect_err("a conflict"));
        assert!(door.admit(Some(&cookie(&cookie_value))).is_none());

        assert_eq!(
            std::fs::read_to_string(user_config_path(home.path())).unwrap(),
            before
        );
        assert_eq!(fake.held().as_deref(), Some(OTHER));
        assert_eq!(fake.writes(), 0);
        assert_eq!(
            std::fs::read_to_string(sessions_path(home.path())).unwrap(),
            sessions,
            "the rotation emptied nothing"
        );
    }

    /// A store that cannot be read is not an empty one: minting would
    /// replace the token every browser holds, and the config is never
    /// the fallback.
    #[test]
    fn an_unreadable_store_mints_nothing_and_falls_back_to_nothing() {
        let home = tempfile::tempdir().unwrap();
        let fake = std::sync::Arc::new(Fake::default());
        Fake::set(&fake.fail_read, true);
        let door = door_with(home.path(), &fake);
        quiet(&door.token().expect_err("refused"));
        quiet(&door.open_session_for_token(LEGACY).expect_err("refused"));
        quiet(&door.rotate_token().expect_err("refused"));
        let opened = door.open_session("token").unwrap();
        let id_hash = door
            .admit(Some(&cookie(&opened)))
            .expect("admitted")
            .id_hash;
        quiet(&door.token_for_session(&id_hash).expect_err("refused"));
        assert_eq!(fake.writes(), 0, "no token minted");
        assert_eq!(config_token(home.path()), None, "none written in plaintext");

        let before = legacy_config(home.path(), LEGACY);
        quiet(&door.token().expect_err("refused"));
        assert_eq!(
            std::fs::read_to_string(user_config_path(home.path())).unwrap(),
            before
        );
    }

    /// A session the legacy token opened, revoked by a rotation that
    /// also performed the migration, stays revoked: neither its cookie
    /// nor the old token gets back in, and the old token reappearing
    /// in the config is a conflict, not a way in.
    #[test]
    fn a_revoked_session_is_not_readmitted_after_a_migration() {
        let home = tempfile::tempdir().unwrap();
        legacy_config(home.path(), LEGACY);
        let cookie_value = random_token();
        let now = rfc3339(now_utc());
        write_typed_config(
            &sessions_path(home.path()),
            &SessionsFile {
                sessions: vec![SessionRecord {
                    id_hash: hex(&hash_token(&cookie_value)),
                    how: "token".into(),
                    created: now.clone(),
                    last_seen: now,
                }],
            },
        )
        .unwrap();
        let fake = std::sync::Arc::new(Fake::default());
        let a = door_with(home.path(), &fake);
        let b = door_with(home.path(), &fake);
        let id_hash = a
            .admit(Some(&cookie(&cookie_value)))
            .expect("admitted")
            .id_hash;

        let fresh = b.rotate_token().unwrap();
        assert_eq!(fake.held().as_deref(), Some(fresh.as_str()));
        assert_eq!(config_token(home.path()), None);
        assert!(a.admit(Some(&cookie(&cookie_value))).is_none());
        assert_eq!(a.token_for_session(&id_hash).unwrap(), None);
        assert!(a.open_session_for_token(LEGACY).unwrap().is_none());

        legacy_config(home.path(), LEGACY);
        quiet(&a.open_session_for_token(LEGACY).expect_err("a conflict"));
        assert!(a.sessions().is_err());
    }

    /// Ten in a minute per address, then no more until the minute
    /// is up.
    #[tokio::test(start_paused = true)]
    async fn attempts_are_ten_a_minute_per_address() {
        let door = Door::new(None, remote_reads);
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
