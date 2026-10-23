//! `clank user-command`: a person's press on the story site types
//! `! <command>` into the terminal of the agent that asked, and nothing
//! else can.
//!
//! Each site signs its requests with an Ed25519 key only it holds; clank
//! holds the public keys, by name, and verifies, never signs. `submit`
//! admits a request under whichever trusted key signed it and reports;
//! only the monitor dispatches, at most once, into an idle, empty Claude
//! Code prompt, and only after verifying the stored envelope again under
//! that same key. Same-UID limits are in the README ("User commands").

pub(crate) mod dispatch;
pub(crate) mod envelope;
pub(crate) mod live;
pub(crate) mod store;
pub(crate) mod trust;

use clap::{Args, Subcommand};
use envelope::{Envelope, Id, SiteKey, Verified};
use std::io::Read;
use std::path::{Path, PathBuf};
use store::{Record, State, Status, Store};
use trust::{KeyName, TrustSet};

/// `clank user-command`.
#[derive(Args, Debug)]
pub struct UserCommandArgs {
    #[command(subcommand)]
    pub command: UserCommandCmd,
}

#[derive(Subcommand, Debug)]
pub enum UserCommandCmd {
    /// Trust a site's Ed25519 public key, read as base64 from stdin
    ///
    /// Stores it as `--name` and prints its fingerprint. A different key
    /// already trusted as that name is kept unless `--replace` is given; a
    /// key already trusted as another name is refused.
    TrustKey {
        /// The site's name: 1 to 64 of [A-Za-z0-9_-].
        #[arg(long, value_name = "SITE")]
        name: String,
        /// Replace a different key already trusted as that name.
        #[arg(long)]
        replace: bool,
    },
    /// Stop trusting the key named `--name`
    ///
    /// Prints its fingerprint. Requests it admitted and not yet delivered are
    /// refused at their next use.
    UntrustKey {
        #[arg(long, value_name = "SITE")]
        name: String,
    },
    /// The trusted keys' names and fingerprints
    Keys,
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
        UserCommandCmd::TrustKey { name, replace } => {
            let fingerprint = trust_key(&home, &name, &read_stdin(4096)?, replace)?;
            println!("{fingerprint}");
            Ok(())
        }
        UserCommandCmd::UntrustKey { name } => {
            for fingerprint in untrust_key(&home, &name)? {
                println!("{fingerprint}");
            }
            Ok(())
        }
        UserCommandCmd::Keys => {
            for (name, key) in trust_set(&home).keys().map_err(anyhow::Error::msg)? {
                println!("{name}\t{}", key.fingerprint());
            }
            Ok(())
        }
        UserCommandCmd::Submit => {
            let text = read_stdin(envelope::ENVELOPE_MAX)?;
            let admitted = admit(
                &Store::home()?,
                &text,
                &trust_set(&home),
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

/// The trusted keys as `~/.clank/config.json` holds them now.
pub(crate) fn trust_set(home: &Path) -> TrustSet {
    match crate::cli::team::read_user_config(home) {
        Ok(config) => TrustSet::of(config.user_command.as_ref()),
        Err(e) => TrustSet::unusable(format!("{e:#}")),
    }
}

/// Trust the key in `text` as `name` and say its fingerprint.
pub(crate) fn trust_key(
    home: &Path,
    name: &str,
    text: &str,
    replace: bool,
) -> anyhow::Result<String> {
    let name = KeyName::parse(name).map_err(anyhow::Error::msg)?;
    let key = SiteKey::parse(text).map_err(anyhow::Error::msg)?;
    crate::cli::team::mutate_user_config(home, |config| {
        trust::trust(
            config.user_command.get_or_insert_default(),
            &name,
            &key,
            replace,
        )?;
        Ok(key.fingerprint())
    })
}

/// Stop trusting `name`: the fingerprints no longer trusted.
pub(crate) fn untrust_key(home: &Path, name: &str) -> anyhow::Result<Vec<String>> {
    let name = KeyName::parse(name).map_err(anyhow::Error::msg)?;
    crate::cli::team::mutate_user_config(home, |config| {
        let section = config.user_command.get_or_insert_default();
        let gone = trust::untrust(section, &name)?;
        if section.keys.is_empty() {
            config.user_command = None;
        }
        Ok(gone)
    })
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

/// Admit the envelope in `text`: verify it under whichever key in `trust`
/// signed it, at `now`, before anything is parsed from it, check its target,
/// then file it under its id, pinned to that key's entry. The same id with
/// byte-identical signed payload answers with the record as it stands, pin
/// included whichever key signed the repeat, and rewrites a waiting one so
/// the monitor looks again; the same id with any other payload is a
/// collision and changes nothing. Nothing is filed for a request that
/// fails.
pub(crate) fn admit(
    store: &Store,
    text: &str,
    trust: &TrustSet,
    now: u64,
    check: impl Fn(&Verified) -> Result<(), String>,
) -> Result<Status, String> {
    let envelope = Envelope::parse(text).map_err(|e| e.to_string())?;
    let (pin, verified) = trust.admit(&envelope, now).map_err(|e| e.to_string())?;
    check(&verified)?;
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
            let record = Record::admitted(envelope, pin, now);
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

    fn one(key: &SiteKey) -> TrustSet {
        TrustSet::from_keys([("site", key.clone())])
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
        let status = admit(&w.store, &text, &one(&w.site.key()), NOW, ok).unwrap();
        assert_eq!(status.state, State::Admitted);
        assert_eq!(status.id, "r1");
        assert_eq!(status.key, w.site.key().fingerprint());
        assert_eq!(status.key_name, "site");
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
        let cases: Vec<(String, TrustSet, u64)> = vec![
            (String::new(), one(&w.site.key()), NOW),
            ("{\"request\":\"x\"}".into(), one(&w.site.key()), NOW),
            (
                serde_json::to_string(&forger.sign(&payload(&repo()))).unwrap(),
                one(&w.site.key()),
                NOW,
            ),
            (good.clone(), one(&w.site.key()), NOW + 10_000),
            (
                serde_json::to_string(&w.site.sign(&long)).unwrap(),
                one(&w.site.key()),
                NOW,
            ),
            (good.clone(), TrustSet::of(None), NOW),
        ];
        for (text, key, now) in cases {
            assert!(admit(&w.store, &text, &key, now, ok).is_err(), "{text}");
        }
        let elsewhere = admit(&w.store, &good, &one(&w.site.key()), NOW, |_| {
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
        admit(&w.store, &text, &one(&key), NOW, ok).unwrap();
        let tag = crate::cli::monitor::repo_tag(&repo());
        let (label, id) = w.store.list(&tag).remove(0);
        let mut record = w.store.read(&tag, &label, &id).unwrap().unwrap();
        record.moved(State::Delivered, None, NOW + 1);
        w.store
            .lock(&tag, &label, &id)
            .unwrap()
            .save(&record)
            .unwrap();
        let again = admit(&w.store, &text, &one(&key), NOW + 2, ok).unwrap();
        assert_eq!(again.state, State::Delivered);
        let mut other = payload(&repo());
        other["command"] = "git push --force".into();
        let collision = serde_json::to_string(&w.site.sign(&other)).unwrap();
        assert!(
            admit(&w.store, &collision, &one(&key), NOW, ok)
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
        admit(&w.store, &text, &one(&key), NOW, ok).unwrap();
        let tag = crate::cli::monitor::repo_tag(&repo());
        let (label, id) = w.store.list(&tag).remove(0);
        let before = w.store.stamp(&tag, &label, &id);
        admit(&w.store, &text, &one(&key), NOW, ok).unwrap();
        assert_ne!(w.store.stamp(&tag, &label, &id), before);
    }

    /// A repeat of an admitted payload signed by another trusted key
    /// answers with the record as it stands: still pinned to the key that
    /// admitted it, its envelope that key's.
    #[test]
    fn a_repeat_under_another_key_keeps_its_pin() {
        let w = world();
        let other = Signer::generate();
        let trust = TrustSet::from_keys([("site-a", w.site.key()), ("site-b", other.key())]);
        let first = w.site.sign(&payload(&repo()));
        let text = |e: &Envelope| serde_json::to_string(e).unwrap();
        admit(&w.store, &text(&first), &trust, NOW, ok).unwrap();
        let tag = crate::cli::monitor::repo_tag(&repo());
        let (label, id) = w.store.list(&tag).remove(0);
        let before = w.store.read(&tag, &label, &id).unwrap().unwrap();
        let again = admit(
            &w.store,
            &text(&other.sign(&payload(&repo()))),
            &trust,
            NOW,
            ok,
        )
        .unwrap();
        assert_eq!(
            (again.key_name.as_str(), again.key.as_str()),
            ("site-a", w.site.key().fingerprint().as_str())
        );
        assert_eq!(w.store.read(&tag, &label, &id).unwrap().unwrap(), before);
    }

    fn config(home: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(home.join(".clank/config.json")).unwrap()).unwrap()
    }

    /// A config written before keys had names: its one key reads as
    /// `default`, the first trust mutation moves it into the named set and
    /// keeps every unrelated field, and untrusting `default` then leaves no
    /// copy to read back.
    #[test]
    fn the_legacy_key_migrates_as_default_and_is_consumed() {
        let home = tempfile::tempdir().unwrap();
        let (old, b) = (Signer::generate().key(), Signer::generate().key());
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        std::fs::write(
            home.path().join(".clank/config.json"),
            serde_json::to_vec(&serde_json::json!({
                "auto": "on",
                "a_later_field": [1, 2],
                "user_command": {"site_key": old.encoded()},
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            trust_set(home.path()),
            TrustSet::from_keys([("default", old.clone())])
        );
        trust_key(home.path(), "site-b", &b.encoded(), false).unwrap();
        let written = config(home.path());
        assert_eq!(written["auto"], "on");
        assert_eq!(written["a_later_field"], serde_json::json!([1, 2]));
        assert_eq!(
            written["user_command"],
            serde_json::json!({"keys": {"default": old.encoded(), "site-b": b.encoded()}})
        );
        assert_eq!(
            untrust_key(home.path(), "default").unwrap(),
            vec![old.fingerprint()]
        );
        assert_eq!(
            trust_set(home.path()),
            TrustSet::from_keys([("site-b", b.clone())])
        );
        untrust_key(home.path(), "site-b").unwrap();
        assert_eq!(trust_set(home.path()), TrustSet::of(None));
        assert_eq!(config(home.path())["auto"], "on");
    }

    #[test]
    fn trusting_a_key_needs_replace_to_change_it() {
        let home = tempfile::tempdir().unwrap();
        let (first, second, third) = (
            Signer::generate().key(),
            Signer::generate().key(),
            Signer::generate().key(),
        );
        assert_eq!(
            trust_key(home.path(), "a", &first.encoded(), false).unwrap(),
            first.fingerprint()
        );
        trust_key(home.path(), "b", &third.encoded(), false).unwrap();
        assert!(trust_key(home.path(), "a", &first.encoded(), false).is_ok());
        assert!(trust_key(home.path(), "a", &second.encoded(), false).is_err());
        trust_key(home.path(), "a", &second.encoded(), true).unwrap();
        assert_eq!(
            trust_set(home.path()),
            TrustSet::from_keys([("a", second), ("b", third)])
        );
        assert!(trust_key(home.path(), "a", "not a key", true).is_err());
        assert!(trust_key(home.path(), "../a", &first.encoded(), true).is_err());
        assert!(untrust_key(home.path(), "nobody").is_err());
    }

    /// Concurrent trust mutations each land: none reads the config, waits on
    /// another's write, and writes back a set without it.
    #[test]
    fn concurrent_trust_mutations_lose_nothing() {
        let home = tempfile::tempdir().unwrap();
        let keys: Vec<SiteKey> = (0..24).map(|_| Signer::generate().key()).collect();
        let start = std::sync::Barrier::new(keys.len());
        std::thread::scope(|s| {
            for (n, key) in keys.iter().enumerate() {
                let (home, start) = (home.path(), &start);
                s.spawn(move || {
                    start.wait();
                    trust_key(home, &format!("site-{n}"), &key.encoded(), false).unwrap();
                });
            }
        });
        let trusted = trust_set(home.path());
        assert_eq!(trusted.keys().unwrap().len(), keys.len(), "{trusted:?}");
    }
}
