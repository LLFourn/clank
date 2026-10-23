//! `clank user-command`: a person's press on the story site types
//! `! <command>` into the terminal of the agent that asked, and nothing
//! else can.
//!
//! The site signs each request with an Ed25519 key only it holds; clank
//! holds the public key and verifies, never signs. `submit` admits a
//! request and reports; only the monitor dispatches, at most once, into an
//! idle, empty Claude Code prompt, and only after verifying the stored
//! envelope again. Same-UID limits are in the README ("User commands").

pub(crate) mod dispatch;
pub(crate) mod envelope;
pub(crate) mod live;
pub(crate) mod store;

use clap::{Args, Subcommand};
use envelope::{Envelope, Id, SiteKey, Verified};
use std::io::Read;
use std::path::{Path, PathBuf};
use store::{Record, State, Status, Store};

/// `clank user-command`.
#[derive(Args, Debug)]
pub struct UserCommandArgs {
    #[command(subcommand)]
    pub command: UserCommandCmd,
}

#[derive(Subcommand, Debug)]
pub enum UserCommandCmd {
    /// Trust the site's Ed25519 public key, read as base64 from stdin
    ///
    /// Prints the key's fingerprint. A different key already trusted is
    /// kept unless `--replace` is given.
    TrustKey {
        /// Replace a different key already trusted.
        #[arg(long)]
        replace: bool,
    },
    /// Admit one signed request, read as a JSON envelope from stdin
    ///
    /// Prints the request's state as JSON. Only the repo's monitor
    /// dispatches it.
    Submit,
    /// A request's state, as JSON
    Status {
        /// The request's repo. Defaults to the cwd's git toplevel.
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
        /// The agent it was signed for.
        #[arg(long, value_name = "AGENT")]
        agent: String,
        /// Its request id.
        #[arg(long, value_name = "ID")]
        id: String,
    },
}

pub async fn run(args: UserCommandArgs) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set"))?;
    match args.command {
        UserCommandCmd::TrustKey { replace } => {
            let fingerprint = trust_key(&home, &read_stdin(4096)?, replace)?;
            println!("{fingerprint}");
            Ok(())
        }
        UserCommandCmd::Submit => {
            let text = read_stdin(envelope::ENVELOPE_MAX)?;
            let admitted = admit(
                &Store::home()?,
                &text,
                site_key(&home).as_ref(),
                crate::cli::recovery::unix_now(),
                target_check,
            );
            match admitted {
                Ok(status) => {
                    println!("{}", serde_json::to_string(&status)?);
                    Ok(())
                }
                Err(reason) => {
                    println!(
                        "{}",
                        serde_json::json!({"state": "refused", "reason": reason})
                    );
                    anyhow::bail!("refused: {reason}")
                }
            }
        }
        UserCommandCmd::Status { repo, agent, id } => {
            let repo = crate::cli::resolve_repo(repo.as_deref())?;
            let agent = clank_core::ids::AgentLabel::parse(&agent)?;
            let id = Id::parse(&id).map_err(anyhow::Error::msg)?;
            let tag = crate::cli::monitor::repo_tag(&repo);
            match Store::home()?.read(&tag, &agent, &id)? {
                Some(record) => {
                    println!("{}", serde_json::to_string(&record.status(&id))?);
                    Ok(())
                }
                None => anyhow::bail!(
                    "no request `{}` for `{agent}` in {}",
                    id.as_str(),
                    repo.display()
                ),
            }
        }
    }
}

fn read_stdin(max: usize) -> anyhow::Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .take(max as u64 + 1)
        .read_to_string(&mut text)?;
    anyhow::ensure!(text.len() <= max, "stdin is longer than {max} bytes");
    Ok(text)
}

/// The trusted key as `~/.clank/config.json` holds it now; `None` when
/// there is none, or what is there is not a key.
pub(crate) fn site_key(home: &Path) -> Option<SiteKey> {
    let config = crate::cli::team::read_user_config(home).ok()?;
    SiteKey::parse(&config.user_command?.site_key).ok()
}

/// Trust the key in `text`, through the typed user-config writer, and say
/// its fingerprint. A different key already trusted needs `replace`.
pub(crate) fn trust_key(home: &Path, text: &str, replace: bool) -> anyhow::Result<String> {
    let key = SiteKey::parse(text).map_err(anyhow::Error::msg)?;
    let mut config = crate::cli::team::read_user_config(home)?;
    if let Some(section) = &config.user_command
        && SiteKey::parse(&section.site_key).as_ref() != Ok(&key)
        && !replace
    {
        anyhow::bail!(
            "a different key ({}) is already trusted; pass --replace to replace it, which \
             refuses every request it admitted at their next use",
            SiteKey::parse(&section.site_key)
                .map(|k| k.fingerprint())
                .unwrap_or_else(|_| "unreadable".into())
        );
    }
    config.user_command = Some(crate::cli::teams_config::UserCommandSection {
        site_key: key.encoded(),
    });
    crate::cli::team::write_user_config(home, &config)?;
    Ok(key.fingerprint())
}

/// Whether a verified request's target is this machine's: its repo is a
/// canonical path, its agent is on that repo's roster, and the session and
/// tool it names are the agent's binding now.
fn target_check(v: &Verified) -> Result<(), String> {
    let canonical = dunce::canonicalize(&v.repo).map_err(|e| format!("its repo: {e}"))?;
    if canonical != v.repo {
        return Err("its repo is not a canonical path".into());
    }
    crate::agent_store::resolve_role(&v.repo, &v.agent)
        .map_err(|_| format!("`{}` is not on that repo's roster", v.agent))?;
    let bound = crate::agent_store::load_agent_config(&v.repo, &v.agent)
        .ok()
        .flatten()
        .and_then(|c| c.session)
        .map(|s| (s.tool, s.id));
    if bound != Some((v.tool, v.session.clone())) {
        return Err("the session it names does not hold the label".into());
    }
    Ok(())
}

/// Admit the envelope in `text`: verify it under `key` at `now` before
/// anything is parsed from it, check its target, then file it under its id.
/// The same id with byte-identical signed payload answers with the record
/// as it stands, and rewrites a waiting one so the monitor looks again;
/// the same id with any other payload is a collision and changes nothing.
/// Nothing is filed for a request that fails.
pub(crate) fn admit(
    store: &Store,
    text: &str,
    key: Option<&SiteKey>,
    now: u64,
    check: impl Fn(&Verified) -> Result<(), String>,
) -> Result<Status, String> {
    let envelope = Envelope::parse(text).map_err(|e| e.to_string())?;
    let verified = envelope::verify(&envelope, key, now).map_err(|e| e.to_string())?;
    check(&verified)?;
    let key = key.expect("verified under a key");
    let tag = crate::cli::monitor::repo_tag(&verified.repo);
    let held = store
        .lock(&tag, &verified.agent, &verified.id)
        .map_err(|e| format!("{e:#}"))?;
    match held.load().map_err(|e| format!("{e:#}"))? {
        Some(existing) => {
            if !same_payload(&existing.envelope, &envelope) {
                return Err(format!(
                    "id `{}` already names a different request; nothing was changed",
                    verified.id.as_str()
                ));
            }
            if matches!(existing.state, State::Admitted | State::WaitingForIdle) {
                held.save(&existing).map_err(|e| format!("{e:#}"))?;
            }
            Ok(existing.status(&verified.id))
        }
        None => {
            let record = Record::admitted(envelope, key.fingerprint(), now);
            held.save(&record).map_err(|e| format!("{e:#}"))?;
            Ok(record.status(&verified.id))
        }
    }
}

fn same_payload(a: &Envelope, b: &Envelope) -> bool {
    use base64::Engine;
    let decode = |e: &Envelope| {
        base64::engine::general_purpose::STANDARD
            .decode(&e.request)
            .ok()
    };
    decode(a).is_some() && decode(a) == decode(b)
}

#[cfg(test)]
mod tests {
    use super::envelope::testkey::{NOW, Signer, payload};
    use super::*;

    fn repo() -> PathBuf {
        PathBuf::from("/w/proj")
    }

    fn ok(_: &Verified) -> Result<(), String> {
        Ok(())
    }

    struct World {
        _home: tempfile::TempDir,
        store: Store,
        site: Signer,
    }

    fn world() -> World {
        let home = tempfile::tempdir().unwrap();
        World {
            store: Store::at(home.path().join("uc")),
            _home: home,
            site: Signer::generate(),
        }
    }

    fn filed(store: &Store) -> usize {
        store.list(&crate::cli::monitor::repo_tag(&repo())).len()
    }

    #[test]
    fn a_verified_request_is_filed_as_admitted() {
        let w = world();
        let text = serde_json::to_string(&w.site.sign(&payload(&repo()))).unwrap();
        let status = admit(&w.store, &text, Some(&w.site.key()), NOW, ok).unwrap();
        assert_eq!(status.state, State::Admitted);
        assert_eq!(status.id, "r1");
        assert_eq!(status.key, w.site.key().fingerprint());
        assert_eq!(filed(&w.store), 1);
        let shown = serde_json::to_string(&status).unwrap();
        assert!(
            !shown.contains("git push") && !shown.contains(&w.site.sign(&payload(&repo())).sig)
        );
    }

    /// No proof, a malformed envelope, a forged signature, an expired
    /// request, one signed for over ten minutes, no trusted key, and a
    /// target that is not this machine's each file nothing.
    #[test]
    fn a_request_that_fails_files_nothing() {
        let w = world();
        let forger = Signer::generate();
        let mut long = payload(&repo());
        long["expires_at"] = (NOW + 601).into();
        let good = serde_json::to_string(&w.site.sign(&payload(&repo()))).unwrap();
        let cases: Vec<(String, Option<SiteKey>, u64)> = vec![
            (String::new(), Some(w.site.key()), NOW),
            ("{\"request\":\"x\"}".into(), Some(w.site.key()), NOW),
            (
                serde_json::to_string(&forger.sign(&payload(&repo()))).unwrap(),
                Some(w.site.key()),
                NOW,
            ),
            (good.clone(), Some(w.site.key()), NOW + 10_000),
            (
                serde_json::to_string(&w.site.sign(&long)).unwrap(),
                Some(w.site.key()),
                NOW,
            ),
            (good.clone(), None, NOW),
        ];
        for (text, key, now) in cases {
            assert!(
                admit(&w.store, &text, key.as_ref(), now, ok).is_err(),
                "{text}"
            );
        }
        let elsewhere = admit(&w.store, &good, Some(&w.site.key()), NOW, |_| {
            Err("the session it names does not hold the label".into())
        });
        assert!(elsewhere.is_err());
        assert_eq!(filed(&w.store), 0);
    }

    /// The same id with the same signed payload answers with the record as
    /// it stands; with any other payload it is a collision and the record
    /// is untouched.
    #[test]
    fn an_id_is_bound_to_its_payload() {
        let w = world();
        let key = w.site.key();
        let text = serde_json::to_string(&w.site.sign(&payload(&repo()))).unwrap();
        admit(&w.store, &text, Some(&key), NOW, ok).unwrap();
        let tag = crate::cli::monitor::repo_tag(&repo());
        let (label, id) = w.store.list(&tag).remove(0);
        let mut record = w.store.read(&tag, &label, &id).unwrap().unwrap();
        record.moved(State::Delivered, None, NOW + 1);
        w.store
            .lock(&tag, &label, &id)
            .unwrap()
            .save(&record)
            .unwrap();
        let again = admit(&w.store, &text, Some(&key), NOW + 2, ok).unwrap();
        assert_eq!(again.state, State::Delivered);
        let mut other = payload(&repo());
        other["command"] = "git push --force".into();
        let collision = serde_json::to_string(&w.site.sign(&other)).unwrap();
        assert!(
            admit(&w.store, &collision, Some(&key), NOW, ok)
                .unwrap_err()
                .contains("already names a different request")
        );
        assert_eq!(w.store.read(&tag, &label, &id).unwrap().unwrap(), record);
    }

    /// A repeat submit of a waiting request rewrites it, so the monitor's
    /// watch sees it and looks again.
    #[test]
    fn a_repeat_submit_pokes_a_waiting_request() {
        let w = world();
        let key = w.site.key();
        let text = serde_json::to_string(&w.site.sign(&payload(&repo()))).unwrap();
        admit(&w.store, &text, Some(&key), NOW, ok).unwrap();
        let tag = crate::cli::monitor::repo_tag(&repo());
        let (label, id) = w.store.list(&tag).remove(0);
        let before = w.store.stamp(&tag, &label, &id);
        admit(&w.store, &text, Some(&key), NOW, ok).unwrap();
        assert_ne!(w.store.stamp(&tag, &label, &id), before);
    }

    #[test]
    fn trusting_a_key_needs_replace_to_change_it() {
        let home = tempfile::tempdir().unwrap();
        let first = Signer::generate().key();
        let second = Signer::generate().key();
        assert_eq!(
            trust_key(home.path(), &first.encoded(), false).unwrap(),
            first.fingerprint()
        );
        assert_eq!(site_key(home.path()), Some(first.clone()));
        assert!(trust_key(home.path(), &first.encoded(), false).is_ok());
        assert!(trust_key(home.path(), &second.encoded(), false).is_err());
        assert_eq!(site_key(home.path()), Some(first));
        trust_key(home.path(), &second.encoded(), true).unwrap();
        assert_eq!(site_key(home.path()), Some(second));
        assert!(trust_key(home.path(), "not a key", true).is_err());
    }
}
