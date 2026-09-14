//! The door: who the remote lets in. A session is a cookie the
//! browser holds and a hashed record here; it opens from a passkey
//! ceremony or from a one-time link the TUI minted, and closes when
//! it is revoked from the TUI, or after [`SESSION_DAYS`]. Nothing
//! secret is kept: the file holds hashes, the tokens live only until
//! used or expired, and the passkeys are public keys
//! (the-tui-mints-the-way-in).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use webauthn_rs::prelude::*;

pub(crate) const SESSION_DAYS: i64 = 30;
/// A minted link is good for this long, and for one use.
pub(crate) const LINK_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// A ceremony the browser never finishes is forgotten after this.
const CEREMONY_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// Login attempts one address may make in a minute before it is
/// refused for the rest of it.
pub(crate) const ATTEMPTS_PER_MINUTE: u32 = 10;
pub(crate) const COOKIE: &str = "clank_session";
const SESSIONS_FILE: &str = ".clank/remote-sessions.json";

/// What the two doors admit: a login link opens a session for the
/// holder; a registration link admits one passkey registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    Login,
    Register,
}

/// One open session as the file keeps it: the hash of the cookie,
/// never the cookie.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SessionRecord {
    pub(crate) id_hash: String,
    pub(crate) passkey: String,
    pub(crate) created: String,
    pub(crate) last_seen: String,
}

#[derive(Default, Serialize, Deserialize)]
struct SessionsFile {
    #[serde(default)]
    sessions: Vec<SessionRecord>,
}

/// A registered passkey as `~/.clank/config.json#/remote/passkeys`
/// keeps it: the credential (id, COSE key, sign count) and how the
/// operator named it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPasskey {
    pub(crate) name: String,
    pub(crate) added: String,
    pub(crate) key: Passkey,
}

/// `~/.clank/config.json#/remote`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteSection {
    /// The tunnel that gives this machine a public URL, when there
    /// is one: its fixed URL is the relying party's origin, the host
    /// a `Secure` cookie is for, and where the phone's link points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tunnel: Option<super::tunnel::TunnelSection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) passkeys: Vec<StoredPasskey>,
}

/// A session the door admitted: what the routes carry.
#[derive(Debug, Clone)]
pub(crate) struct Admitted {
    pub(crate) id_hash: String,
    /// When its term ends: a stream holding it ends then.
    pub(crate) expires: tokio::time::Instant,
}

/// A passkey as the TUI lists it: the credential id is what a
/// removal names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PasskeyRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) added: String,
}

struct Token {
    kind: Link,
    hash: [u8; 32],
    expires: tokio::time::Instant,
}

enum Ceremony {
    Register {
        name: String,
        state: PasskeyRegistration,
    },
    Login {
        state: PasskeyAuthentication,
    },
}

struct Pending {
    started: tokio::time::Instant,
    ceremony: Ceremony,
}

/// What this instance keeps in memory: the links it minted, the
/// ceremonies in flight, the attempts it has seen, the live set as
/// last read — and, without a home, the sessions and passkeys.
#[derive(Default)]
struct Inner {
    tokens: Vec<Token>,
    ceremonies: HashMap<String, Pending>,
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
    passkeys: Vec<StoredPasskey>,
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
}

/// What the browser posts to finish a ceremony it started.
#[derive(Deserialize)]
pub(crate) struct Finish<C> {
    pub(crate) id: String,
    pub(crate) credential: C,
}

/// Why a ceremony did not finish, in words for the browser.
#[derive(Debug)]
pub(crate) enum Refused {
    /// The link is spent, expired, or was never minted.
    Link,
    /// No ceremony by that id: expired, finished, or invented.
    Ceremony,
    /// The authenticator's answer did not verify, or is a credential
    /// this door never registered.
    Credential(String),
    /// Nothing to authenticate against.
    NoPasskeys,
    /// The store could not be read or written; nothing was changed.
    Store(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Link => write!(
                f,
                "this link is spent or expired — mint another from the TUI"
            ),
            Refused::Ceremony => write!(f, "no such ceremony — start again"),
            Refused::Credential(why) => write!(f, "the credential did not verify: {why}"),
            Refused::NoPasskeys => write!(
                f,
                "no passkey is registered — mint a registration link from the TUI"
            ),
            Refused::Store(why) => write!(f, "the store refused: {why}"),
        }
    }
}

impl Door {
    /// The door for this user: passkeys in `~/.clank/config.json`,
    /// open sessions in `~/.clank/remote-sessions.json`, read on
    /// every use. Without a home nothing persists — a session lasts
    /// as long as the process.
    pub(crate) fn new(home: Option<PathBuf>) -> Self {
        let door = Self {
            home,
            inner: Mutex::new(Inner::default()),
            revoked: tokio::sync::watch::channel(0).0,
        };
        door.refresh();
        door
    }

    /// The relying party for a remote on `port`: the tunnel's URL
    /// when a start has one, else `localhost` — which is why the
    /// remote's own URL says `localhost` and not `127.0.0.1`: the
    /// browser's origin must be the RP's (codex on 375f30c). The
    /// public URL is the start's, resolved once and handed here, so
    /// the relying party, the cookie and the links are one
    /// configuration for the life of the instance (codex on 6f60efa).
    pub(crate) fn relying_party(
        &self,
        port: u16,
        public: Option<&Url>,
    ) -> anyhow::Result<Webauthn> {
        let local = Url::parse(&format!("http://localhost:{port}"))?;
        let (rp_id, origin) = match public {
            Some(url) => (
                url.host_str()
                    .ok_or_else(|| anyhow::anyhow!("the tunnel's URL `{url}` has no host"))?
                    .to_string(),
                url.clone(),
            ),
            None => ("localhost".to_string(), local.clone()),
        };
        let mut builder = WebauthnBuilder::new(&rp_id, &origin)?.rp_name("clank");
        if public.is_some() {
            builder = builder.append_allowed_origin(&local);
        }
        Ok(builder.build()?)
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
        let Some(home) = &self.home else {
            let mut inner = self.lock();
            return Ok(edit(&mut inner.memory.sessions).0);
        };
        let _lock = store_lock(home)?;
        let path = sessions_path(home);
        let mut sessions = match std::fs::read_to_string(&path) {
            Ok(s) => {
                serde_json::from_str::<SessionsFile>(&s)
                    .map_err(|e| anyhow::anyhow!("parsing `{}`: {e}", path.display()))?
                    .sessions
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => anyhow::bail!("reading `{}`: {e}", path.display()),
        };
        let (r, changed) = edit(&mut sessions);
        if changed {
            crate::agent_store::write_typed_config(&path, &SessionsFile { sessions })?;
        }
        Ok(r)
    }

    /// The passkeys, the same way, in the user config.
    fn with_passkeys<R>(
        &self,
        edit: impl FnOnce(&mut Vec<StoredPasskey>) -> (R, bool),
    ) -> anyhow::Result<R> {
        let Some(home) = &self.home else {
            let mut inner = self.lock();
            return Ok(edit(&mut inner.memory.passkeys).0);
        };
        let _lock = store_lock(home)?;
        let mut cfg = crate::cli::team::read_user_config(home)?;
        let remote = cfg.remote.get_or_insert_with(Default::default);
        let (r, changed) = edit(&mut remote.passkeys);
        if changed {
            crate::cli::team::write_user_config(home, &cfg)?;
        }
        Ok(r)
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

    /// Open a session for the holder of `passkey`; the cookie value,
    /// handed out once. Not opened at all if it cannot be written.
    pub(crate) fn open_session(&self, passkey: &str) -> anyhow::Result<String> {
        let token = random_token();
        let now = rfc3339(now_utc());
        let record = SessionRecord {
            id_hash: hex(&hash_token(&token)),
            passkey: passkey.to_string(),
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
        inner.tokens.retain(|t| t.expires > now);
        inner.tokens.push(Token {
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
        inner.tokens.retain(|t| t.expires > now);
        let at = inner
            .tokens
            .iter()
            .position(|t| t.kind == kind && bool::from(t.hash.ct_eq(&hash)));
        match at {
            Some(i) => {
                inner.tokens.remove(i);
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

    // ---- passkeys ----

    pub(crate) fn passkeys(&self) -> anyhow::Result<Vec<PasskeyRow>> {
        self.with_passkeys(|passkeys| {
            (
                passkeys
                    .iter()
                    .map(|p| PasskeyRow {
                        id: cred_id_str(&p.key),
                        name: p.name.clone(),
                        added: p.added.clone(),
                    })
                    .collect(),
                false,
            )
        })
    }

    /// Forget the passkey with credential id `id`; one another TUI
    /// already removed is nothing, never its neighbour. The sessions
    /// it opened stay open until they are revoked or expire. An
    /// error means it is still there.
    pub(crate) fn remove_passkey(&self, id: &str) -> anyhow::Result<()> {
        self.with_passkeys(|passkeys| {
            let before = passkeys.len();
            passkeys.retain(|p| cred_id_str(&p.key) != id);
            ((), passkeys.len() != before)
        })
    }

    // ---- ceremonies ----

    /// Begin registering a passkey named `name` for the holder of a
    /// registration link, which is spent here: the link admits one
    /// attempt. The id names the ceremony to `finish_registration`.
    pub(crate) fn start_registration(
        &self,
        rp: &Webauthn,
        token: &str,
        name: &str,
    ) -> Result<(String, CreationChallengeResponse), Refused> {
        if !self.consume(Link::Register, token) {
            return Err(Refused::Link);
        }
        let exclude: Vec<CredentialID> = self
            .with_passkeys(|passkeys| {
                (
                    passkeys.iter().map(|p| p.key.cred_id().clone()).collect(),
                    false,
                )
            })
            .map_err(|e| Refused::Store(format!("{e:#}")))?;
        let mut user = [0u8; 16];
        fill_random(&mut user);
        let (challenge, state) = rp
            .start_passkey_registration(Uuid::from_bytes(user), name, name, Some(exclude))
            .map_err(|e| Refused::Credential(e.to_string()))?;
        let id = self.remember(Ceremony::Register {
            name: name.to_string(),
            state,
        });
        Ok((id, challenge))
    }

    /// Verify the browser's answer, keep the passkey, and open a
    /// session for it: the cookie value. The passkey is written
    /// before anything is admitted; if it cannot be, nothing is.
    pub(crate) fn finish_registration(
        &self,
        rp: &Webauthn,
        id: &str,
        credential: &RegisterPublicKeyCredential,
    ) -> Result<String, Refused> {
        let (name, state) = match self.take(id)? {
            Ceremony::Register { name, state } => (name, state),
            Ceremony::Login { .. } => return Err(Refused::Ceremony),
        };
        let key = rp
            .finish_passkey_registration(credential, &state)
            .map_err(|e| Refused::Credential(e.to_string()))?;
        let added = rfc3339(now_utc());
        let stored = self
            .with_passkeys(|passkeys| {
                if passkeys.iter().any(|p| p.key.cred_id() == key.cred_id()) {
                    return (false, false);
                }
                passkeys.push(StoredPasskey {
                    name: name.clone(),
                    added,
                    key,
                });
                (true, true)
            })
            .map_err(|e| Refused::Store(format!("{e:#}")))?;
        if !stored {
            return Err(Refused::Credential("already registered".into()));
        }
        self.open_session(&name)
            .map_err(|e| Refused::Store(format!("{e:#}")))
    }

    /// Begin a login against every registered passkey.
    pub(crate) fn start_login(
        &self,
        rp: &Webauthn,
    ) -> Result<(String, RequestChallengeResponse), Refused> {
        let keys: Vec<Passkey> = self
            .with_passkeys(|passkeys| (passkeys.iter().map(|p| p.key.clone()).collect(), false))
            .map_err(|e| Refused::Store(format!("{e:#}")))?;
        if keys.is_empty() {
            return Err(Refused::NoPasskeys);
        }
        let (challenge, state) = rp
            .start_passkey_authentication(&keys)
            .map_err(|e| Refused::Credential(e.to_string()))?;
        let id = self.remember(Ceremony::Login { state });
        Ok((id, challenge))
    }

    /// Verify the assertion, note the sign count, and open a session
    /// for the passkey that signed: the cookie value.
    pub(crate) fn finish_login(
        &self,
        rp: &Webauthn,
        id: &str,
        credential: &PublicKeyCredential,
    ) -> Result<String, Refused> {
        let state = match self.take(id)? {
            Ceremony::Login { state } => state,
            Ceremony::Register { .. } => return Err(Refused::Ceremony),
        };
        let result = rp
            .finish_passkey_authentication(credential, &state)
            .map_err(|e| Refused::Credential(e.to_string()))?;
        let name = self
            .with_passkeys(|passkeys| {
                let Some(p) = passkeys
                    .iter_mut()
                    .find(|p| p.key.cred_id() == result.cred_id())
                else {
                    return (None, false);
                };
                let counted = p.key.update_credential(&result).is_some();
                (Some(p.name.clone()), counted)
            })
            .map_err(|e| Refused::Store(format!("{e:#}")))?;
        let Some(name) = name else {
            return Err(Refused::Credential("not a passkey of this door".into()));
        };
        self.open_session(&name)
            .map_err(|e| Refused::Store(format!("{e:#}")))
    }

    fn remember(&self, ceremony: Ceremony) -> String {
        let id = random_token();
        let mut inner = self.lock();
        let now = tokio::time::Instant::now();
        inner
            .ceremonies
            .retain(|_, p| now - p.started < CEREMONY_TTL);
        inner.ceremonies.insert(
            id.clone(),
            Pending {
                started: now,
                ceremony,
            },
        );
        id
    }

    fn take(&self, id: &str) -> Result<Ceremony, Refused> {
        let mut inner = self.lock();
        let now = tokio::time::Instant::now();
        inner
            .ceremonies
            .retain(|_, p| now - p.started < CEREMONY_TTL);
        inner
            .ceremonies
            .remove(id)
            .map(|p| p.ceremony)
            .ok_or(Refused::Ceremony)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
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

fn cred_id_str(key: &Passkey) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.cred_id().as_ref())
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
/// of the sessions file or the passkeys. BLOCKING: the sections are
/// one small file each.
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
    use webauthn_authenticator_rs::WebauthnAuthenticator;
    use webauthn_authenticator_rs::softpasskey::SoftPasskey;

    fn cookie(token: &str) -> String {
        format!("other=1; {COOKIE}={token}")
    }

    /// A link is for one door, one use, five minutes.
    #[tokio::test(start_paused = true)]
    async fn a_link_admits_once_and_not_after_five_minutes() {
        let door = Door::new(None);
        let t = door.mint(Link::Login);
        assert!(!door.consume(Link::Register, &t), "the other door");
        assert!(door.consume(Link::Login, &t));
        assert!(!door.consume(Link::Login, &t), "spent");
        let late = door.mint(Link::Register);
        tokio::time::advance(LINK_TTL + std::time::Duration::from_secs(1)).await;
        assert!(!door.consume(Link::Register, &late), "expired");
        assert!(!door.consume(Link::Login, "not-a-token"));
    }

    /// The file keeps the hash and never the cookie; a revoked
    /// session admits nothing and its watchers are told; a record
    /// older than the term is gone when the door opens.
    #[tokio::test]
    async fn a_session_is_hashed_on_disk_and_ends_when_revoked_or_old() {
        let home = tempfile::tempdir().unwrap();
        let door = Door::new(Some(home.path().to_path_buf()));
        assert!(door.admit(Some("clank_session=nothing")).is_none());
        let token = door.open_session("phone").unwrap();
        let file = std::fs::read_to_string(sessions_path(home.path())).unwrap();
        assert!(!file.contains(&token), "the cookie is not on disk");
        let admitted = door.admit(Some(&cookie(&token))).expect("admitted");
        assert!(file.contains(&admitted.id_hash));
        assert_eq!(door.sessions().unwrap().len(), 1);
        assert_eq!(door.sessions().unwrap()[0].passkey, "phone");
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
        let token = fresh.open_session("phone").unwrap();
        let old = time::OffsetDateTime::now_utc() - time::Duration::days(SESSION_DAYS + 1);
        let mut records = fresh.sessions().unwrap();
        records.push(SessionRecord {
            id_hash: "00".repeat(32),
            passkey: "stale".into(),
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
        let token = a.open_session("phone").unwrap();
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
        let other = a.open_session("laptop").unwrap();
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
        let hash = b.admit(Some(&cookie(&other))).map(|s| s.id_hash);
        assert!(hash.is_none(), "an unreadable store admits nobody");
        assert!(a.revoke_session("anything").is_err());
        assert!(a.open_session("x").is_err());
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

    /// A passkey registers from a link, is kept in the user config,
    /// and logs in; a credential registered to another relying party
    /// does not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_passkey_registers_from_a_link_and_logs_in() {
        let home = tempfile::tempdir().unwrap();
        let door = Door::new(Some(home.path().to_path_buf()));
        let rp = door.relying_party(4321, None).unwrap();
        let origin = Url::parse("http://localhost:4321").unwrap();
        let mut phone = WebauthnAuthenticator::new(SoftPasskey::new(true));

        let link = door.mint(Link::Register);
        assert!(matches!(
            door.start_registration(&rp, "bogus", "phone"),
            Err(Refused::Link)
        ));
        let (id, options) = door.start_registration(&rp, &link, "phone").unwrap();
        assert!(
            matches!(
                door.start_registration(&rp, &link, "again"),
                Err(Refused::Link)
            ),
            "the link admits one registration"
        );
        let credential = phone.do_registration(origin.clone(), options).unwrap();
        assert!(matches!(
            door.finish_registration(&rp, "no-such-ceremony", &credential),
            Err(Refused::Ceremony)
        ));
        let session = door.finish_registration(&rp, &id, &credential).unwrap();
        assert!(door.admit(Some(&cookie(&session))).is_some());
        assert_eq!(door.passkeys().unwrap()[0].name, "phone");
        let cfg = crate::cli::team::read_user_config(home.path()).unwrap();
        assert_eq!(
            cfg.remote.unwrap().passkeys.len(),
            1,
            "kept in the user config"
        );

        let (id, options) = door.start_login(&rp).unwrap();
        let assertion = phone.do_authentication(origin.clone(), options).unwrap();
        let session = door.finish_login(&rp, &id, &assertion).unwrap();
        let admitted = door.admit(Some(&cookie(&session))).unwrap();
        assert_eq!(
            door.sessions()
                .unwrap()
                .iter()
                .find(|s| s.id_hash == admitted.id_hash)
                .unwrap()
                .passkey,
            "phone"
        );
        assert!(
            matches!(
                door.finish_login(&rp, &id, &assertion),
                Err(Refused::Ceremony)
            ),
            "a ceremony finishes once"
        );

        // A key registered to example.com, imported as if it were
        // ours: its assertion names the wrong relying party.
        let elsewhere =
            WebauthnBuilder::new("example.com", &Url::parse("https://example.com").unwrap())
                .unwrap()
                .build()
                .unwrap();
        let other_door = Door::new(None);
        let mut other = WebauthnAuthenticator::new(SoftPasskey::new(true));
        let link = other_door.mint(Link::Register);
        let (id, options) = other_door
            .start_registration(&elsewhere, &link, "theirs")
            .unwrap();
        let cred = other
            .do_registration(Url::parse("https://example.com").unwrap(), options)
            .unwrap();
        other_door
            .finish_registration(&elsewhere, &id, &cred)
            .unwrap();
        let theirs = other_door.lock().memory.passkeys[0].clone();
        {
            let mut cfg = crate::cli::team::read_user_config(home.path()).unwrap();
            cfg.remote.as_mut().unwrap().passkeys.push(theirs);
            crate::cli::team::write_user_config(home.path(), &cfg).unwrap();
        }
        let door = Door::new(Some(home.path().to_path_buf()));
        assert_eq!(door.passkeys().unwrap().len(), 2);
        let (id, _ours) = door.start_login(&rp).unwrap();
        let (_, their_options) = other_door.start_login(&elsewhere).unwrap();
        let forged = other
            .do_authentication(Url::parse("https://example.com").unwrap(), their_options)
            .unwrap();
        assert!(
            matches!(
                door.finish_login(&rp, &id, &forged),
                Err(Refused::Credential(_))
            ),
            "another relying party's assertion is refused"
        );
        // Two TUIs looking at [phone, theirs]: one removes `phone`;
        // the other, still showing both, removes `phone` too — by
        // its id, so nothing happens, and `theirs` is not taken in
        // its place (codex on f657b4d).
        let shown = door.passkeys().unwrap();
        let sibling = Door::new(Some(home.path().to_path_buf()));
        sibling.remove_passkey(&shown[0].id).unwrap();
        door.remove_passkey(&shown[0].id).unwrap();
        let left = door.passkeys().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, shown[1].id, "the neighbour is untouched");
        let cfg = crate::cli::team::read_user_config(home.path()).unwrap();
        assert_eq!(cfg.remote.unwrap().passkeys.len(), 1);
        door.remove_passkey(&shown[1].id).unwrap();
        assert!(door.passkeys().unwrap().is_empty());
        assert!(matches!(door.start_login(&rp), Err(Refused::NoPasskeys)));

        let none = Door::new(None);
        assert!(matches!(none.start_login(&rp), Err(Refused::NoPasskeys)));
    }

    #[test]
    fn the_cookie_is_read_out_of_the_header_and_set_for_the_tunnel_only() {
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
