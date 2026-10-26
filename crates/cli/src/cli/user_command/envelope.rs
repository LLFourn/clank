//! The signed request: what the site sends, how clank verifies it, and the
//! only way to obtain a [`Command`] the user-command writer will type.
//!
//! Verify only. Clank holds the site's Ed25519 PUBLIC key; it contains no
//! signing code, never reads a private key and accepts no path to one.

use base64::Engine;
use clank_core::Tool;
use clank_core::ids::{AgentLabel, SessionId};
use std::path::{Path, PathBuf};

/// Prefixed to the payload bytes before signing, so a signature made for
/// anything else under the same key never verifies here.
pub(crate) const DOMAIN: &[u8] = b"clank user-command v1\n";
/// The longest lifetime a request may be signed for, in seconds.
pub(crate) const WINDOW_MAX: u64 = 600;
/// The clock difference allowed against `issued_at` and `expires_at`.
pub(crate) const SKEW: u64 = 30;
/// The longest request id.
pub(crate) const ID_MAX: usize = 64;
/// The longest command admitted at all. The bound a dispatch types is the
/// lower one its evidence observed (`pane_input::USER_COMMAND_EVIDENCE`).
pub(crate) const COMMAND_LIMIT: usize = 4096;
/// The longest envelope read from stdin.
pub(crate) const ENVELOPE_MAX: usize = 64 * 1024;

/// What the site sends: the base64 of the payload bytes, and the base64
/// Ed25519 signature over [`DOMAIN`] followed by exactly those bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub request: String,
    pub sig: String,
}

impl Envelope {
    /// One envelope from `text`, or why it is malformed.
    pub(crate) fn parse(text: &str) -> Result<Self, Refusal> {
        if text.len() > ENVELOPE_MAX {
            return Err(Refusal::Malformed("the envelope is too long".into()));
        }
        serde_json::from_str(text).map_err(|e| Refusal::Malformed(format!("{e}")))
    }
}

/// The signed payload, parsed from the verified bytes only.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    v: u32,
    repo: String,
    agent: String,
    session: String,
    tool: Tool,
    id: String,
    command: String,
    issued_at: u64,
    expires_at: u64,
}

/// A site's trusted Ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SiteKey([u8; 32]);

impl SiteKey {
    /// A key from its standard base64 encoding, surrounding whitespace
    /// ignored.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .map_err(|e| format!("the key is not base64: {e}"))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "an Ed25519 public key is 32 bytes".to_string())?;
        Ok(SiteKey(bytes))
    }

    /// Its standard base64 encoding, as the config stores it.
    pub(crate) fn encoded(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.0)
    }

    /// Its blake3 fingerprint, as `submit`, `status`, `keys` and `trust-key`
    /// print it.
    pub(crate) fn fingerprint(&self) -> String {
        blake3::hash(&self.0).to_hex()[..32].to_string()
    }
}

/// A request id: a bounded `[A-Za-z0-9_-]` token, safe as a filename.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Id(String);

impl Id {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let ok = !text.is_empty()
            && text.len() <= ID_MAX
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        ok.then(|| Id(text.to_string()))
            .ok_or_else(|| format!("an id is 1 to {ID_MAX} of [A-Za-z0-9_-]"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A command line a verified request carries. Only [`verify`] makes one,
/// so the user-command writer cannot be handed ordinary text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command(String);

impl Command {
    pub(crate) fn line(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(line: &str) -> Self {
        Command(line.to_string())
    }
}

/// A request whose signature verified under a trusted key and whose
/// payload passed every check, with every field taken from the verified
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verified {
    pub(crate) repo: PathBuf,
    pub(crate) agent: AgentLabel,
    pub(crate) session: SessionId,
    pub(crate) tool: Tool,
    pub(crate) id: Id,
    pub(crate) command: Command,
    pub(crate) issued_at: u64,
    pub(crate) expires_at: u64,
}

/// Why a request is refused. Each one's words are the reason a record or
/// an answer carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    NoTrustedKey,
    /// The entry it was admitted under is gone or holds another key.
    KeyChanged,
    /// The configured keys cannot be read, and why.
    Untrusted(String),
    Malformed(String),
    BadSignature,
    BadPayload(String),
    NotYetValid,
    Expired,
    Window,
    NotClaude,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NoTrustedKey => write!(f, "no trusted key"),
            Refusal::KeyChanged => write!(f, "key changed"),
            Refusal::Untrusted(why) => write!(f, "the trusted keys are unusable: {why}"),
            Refusal::Malformed(why) => write!(f, "malformed envelope: {why}"),
            Refusal::BadSignature => write!(f, "the signature does not verify"),
            Refusal::BadPayload(why) => write!(f, "invalid payload: {why}"),
            Refusal::NotYetValid => write!(f, "not yet valid"),
            Refusal::Expired => write!(f, "expired"),
            Refusal::Window => write!(f, "signed for longer than {WINDOW_MAX} seconds"),
            Refusal::NotClaude => write!(
                f,
                "only Claude Code runs user commands; Codex, Grok and OpenCode have no verified \
                 rendering of their shell mode"
            ),
        }
    }
}

/// Verify `envelope` under `key` at `now` (unix seconds): the signature over
/// the transmitted bytes first, then the payload parsed from exactly those
/// bytes, then every field and the validity window.
pub(crate) fn verify(envelope: &Envelope, key: &SiteKey, now: u64) -> Result<Verified, Refusal> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let bytes = b64
        .decode(&envelope.request)
        .map_err(|e| Refusal::Malformed(format!("request is not base64: {e}")))?;
    let sig = b64
        .decode(&envelope.sig)
        .map_err(|e| Refusal::Malformed(format!("sig is not base64: {e}")))?;
    let mut signed = Vec::with_capacity(DOMAIN.len() + bytes.len());
    signed.extend_from_slice(DOMAIN);
    signed.extend_from_slice(&bytes);
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key.0)
        .verify(&signed, &sig)
        .map_err(|_| Refusal::BadSignature)?;
    let payload: Payload =
        serde_json::from_slice(&bytes).map_err(|e| Refusal::BadPayload(format!("{e}")))?;
    checked(payload, now)
}

fn checked(p: Payload, now: u64) -> Result<Verified, Refusal> {
    let bad = |why: &str| Refusal::BadPayload(why.to_string());
    if p.v != 1 {
        return Err(bad("unsupported version"));
    }
    let repo = PathBuf::from(&p.repo);
    if !repo.is_absolute() {
        return Err(bad("repo is not an absolute path"));
    }
    let agent = AgentLabel::parse(&p.agent).map_err(|e| bad(&format!("agent: {e}")))?;
    let session = SessionId::parse(&p.session).map_err(|e| bad(&format!("session: {e}")))?;
    let id = Id::parse(&p.id).map_err(|e| bad(&e))?;
    command_problem(&p.command).map_or(Ok(()), |why| Err(bad(why)))?;
    if p.expires_at < p.issued_at || p.expires_at - p.issued_at > WINDOW_MAX {
        return Err(Refusal::Window);
    }
    if now.saturating_add(SKEW) < p.issued_at {
        return Err(Refusal::NotYetValid);
    }
    if now > p.expires_at.saturating_add(SKEW) {
        return Err(Refusal::Expired);
    }
    if p.tool != Tool::Claude {
        return Err(Refusal::NotClaude);
    }
    Ok(Verified {
        repo,
        agent,
        session,
        tool: p.tool,
        id,
        command: Command(p.command),
        issued_at: p.issued_at,
        expires_at: p.expires_at,
    })
}

/// What disqualifies `command` from being typed as one shell line, if
/// anything: empty, too long, or holding a control character.
fn command_problem(command: &str) -> Option<&'static str> {
    if command.trim().is_empty() {
        Some("the command is empty")
    } else if command.chars().count() > COMMAND_LIMIT {
        Some("the command is too long")
    } else if command.chars().any(char::is_control) {
        Some("the command is not one line without control characters")
    } else {
        None
    }
}

impl Verified {
    /// Whether this request names `repo`, `agent` and `id`: where it was
    /// found or is being filed.
    pub(crate) fn names(&self, repo: &Path, agent: &AgentLabel, id: &Id) -> bool {
        self.repo == repo && &self.agent == agent && &self.id == id
    }
}

#[cfg(test)]
pub(crate) mod testkey {
    //! Test-only signing with a key generated in the test: production has
    //! no signer.
    use super::*;
    use ring::signature::KeyPair;

    pub(crate) struct Signer(pub(crate) ring::signature::Ed25519KeyPair);

    impl Signer {
        pub(crate) fn generate() -> Self {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
            Signer(ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap())
        }

        pub(crate) fn key(&self) -> SiteKey {
            SiteKey(self.0.public_key().as_ref().try_into().unwrap())
        }

        /// An envelope over exactly `bytes`.
        pub(crate) fn seal(&self, bytes: &[u8]) -> Envelope {
            let mut signed = DOMAIN.to_vec();
            signed.extend_from_slice(bytes);
            let b64 = base64::engine::general_purpose::STANDARD;
            Envelope {
                request: b64.encode(bytes),
                sig: b64.encode(self.0.sign(&signed).as_ref()),
            }
        }

        pub(crate) fn sign(&self, payload: &serde_json::Value) -> Envelope {
            self.seal(&serde_json::to_vec(payload).unwrap())
        }
    }

    pub(crate) const NOW: u64 = 1_800_000_000;

    /// A valid payload for `repo`, agent `claude`, session `s1`, id `r1`.
    pub(crate) fn payload(repo: &Path) -> serde_json::Value {
        serde_json::json!({
            "v": 1,
            "repo": repo.to_string_lossy(),
            "agent": "claude",
            "session": "session-one",
            "tool": "claude",
            "id": "r1",
            "command": "git push origin feature",
            "issued_at": NOW,
            "expires_at": NOW + 300,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testkey::*;
    use super::*;

    fn repo() -> PathBuf {
        PathBuf::from("/w/proj")
    }

    #[test]
    fn a_signed_request_verifies_and_carries_its_fields() {
        let site = Signer::generate();
        let v = verify(&site.sign(&payload(&repo())), &site.key(), NOW).unwrap();
        assert_eq!(v.repo, repo());
        assert_eq!(v.agent.as_str(), "claude");
        assert_eq!(v.session.as_str(), "session-one");
        assert_eq!(v.id.as_str(), "r1");
        assert_eq!(v.command.line(), "git push origin feature");
        assert!(v.names(&repo(), &v.agent, &Id::parse("r1").unwrap()));
    }

    /// A forged signature, another key's signature and one over the bytes
    /// without the domain are each refused.
    #[test]
    fn a_forged_request_is_refused() {
        let site = Signer::generate();
        let other = Signer::generate();
        let good = site.sign(&payload(&repo()));
        let mut flipped = good.clone();
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut sig = b64.decode(&flipped.sig).unwrap();
        sig[0] ^= 1;
        flipped.sig = b64.encode(sig);
        assert_eq!(
            verify(&flipped, &site.key(), NOW),
            Err(Refusal::BadSignature)
        );
        assert_eq!(
            verify(&other.sign(&payload(&repo())), &site.key(), NOW),
            Err(Refusal::BadSignature)
        );
        let bare = serde_json::to_vec(&payload(&repo())).unwrap();
        let undomained = Envelope {
            request: b64.encode(&bare),
            sig: b64.encode(other.0.sign(&bare).as_ref()),
        };
        assert_eq!(
            verify(&undomained, &other.key(), NOW),
            Err(Refusal::BadSignature)
        );
    }

    /// Changing any one signed field after signing is a forgery: the bytes
    /// no longer match the signature.
    #[test]
    fn each_signed_field_changed_is_refused() {
        let site = Signer::generate();
        let good = site.sign(&payload(&repo()));
        let b64 = base64::engine::general_purpose::STANDARD;
        let changes: [(&str, serde_json::Value); 9] = [
            ("v", 2.into()),
            ("repo", "/w/other".into()),
            ("agent", "codex".into()),
            ("session", "session-two".into()),
            ("tool", "codex".into()),
            ("id", "r2".into()),
            ("command", "git push --force".into()),
            ("issued_at", (NOW + 1).into()),
            ("expires_at", (NOW + 301).into()),
        ];
        for (field, value) in changes {
            let mut p = payload(&repo());
            p[field] = value;
            let tampered = Envelope {
                request: b64.encode(serde_json::to_vec(&p).unwrap()),
                sig: good.sig.clone(),
            };
            assert_eq!(
                verify(&tampered, &site.key(), NOW),
                Err(Refusal::BadSignature),
                "{field}"
            );
        }
    }

    #[test]
    fn expiry_and_the_window_are_enforced() {
        let site = Signer::generate();
        let key = site.key();
        let e = site.sign(&payload(&repo()));
        assert!(verify(&e, &key, NOW + 300 + SKEW).is_ok());
        assert_eq!(verify(&e, &key, NOW + 301 + SKEW), Err(Refusal::Expired));
        assert_eq!(verify(&e, &key, NOW - SKEW - 1), Err(Refusal::NotYetValid));
        let mut long = payload(&repo());
        long["expires_at"] = (NOW + WINDOW_MAX + 1).into();
        assert_eq!(verify(&site.sign(&long), &key, NOW), Err(Refusal::Window));
        let mut backwards = payload(&repo());
        backwards["expires_at"] = (NOW - 1).into();
        assert_eq!(
            verify(&site.sign(&backwards), &key, NOW),
            Err(Refusal::Window)
        );
    }

    /// A signed payload is still checked: malformed envelopes, unknown
    /// fields, bad ids, multi-line or empty commands, a relative repo, and
    /// any tool but Claude are refused.
    #[test]
    fn a_signed_but_invalid_payload_is_refused() {
        let site = Signer::generate();
        let key = site.key();
        assert!(matches!(Envelope::parse("{}"), Err(Refusal::Malformed(_))));
        assert!(matches!(
            Envelope::parse(r#"{"request":"","sig":"","extra":1}"#),
            Err(Refusal::Malformed(_))
        ));
        assert!(matches!(
            verify(
                &Envelope {
                    request: "%%".into(),
                    sig: "AA==".into()
                },
                &key,
                NOW
            ),
            Err(Refusal::Malformed(_))
        ));
        let cases: [(&str, serde_json::Value); 7] = [
            ("id", "../x".into()),
            ("id", "x".repeat(ID_MAX + 1).into()),
            ("command", "ls\nrm -rf ~".into()),
            ("command", "ls\u{1b}[A".into()),
            ("command", "   ".into()),
            ("repo", "relative/repo".into()),
            ("extra", 1.into()),
        ];
        for (field, value) in cases {
            let mut p = payload(&repo());
            p[field] = value;
            assert!(
                matches!(
                    verify(&site.sign(&p), &key, NOW),
                    Err(Refusal::BadPayload(_))
                ),
                "{field}"
            );
        }
        for tool in ["codex", "grok", "opencode"] {
            let mut p = payload(&repo());
            p["tool"] = tool.into();
            assert_eq!(verify(&site.sign(&p), &key, NOW), Err(Refusal::NotClaude));
        }
    }

    #[test]
    fn a_key_round_trips_and_has_a_stable_fingerprint() {
        let key = Signer::generate().key();
        let again = SiteKey::parse(&format!("  {}\n", key.encoded())).unwrap();
        assert_eq!(again, key);
        assert_eq!(again.fingerprint(), key.fingerprint());
        assert_eq!(key.fingerprint().len(), 32);
        assert!(SiteKey::parse("AAAA").is_err());
        assert!(SiteKey::parse("not base64!").is_err());
    }
}
