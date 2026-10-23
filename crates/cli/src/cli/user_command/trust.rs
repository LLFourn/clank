//! Which sites clank trusts: each one's Ed25519 public key, under a name.
//!
//! The set is searched only to admit a request ([`TrustSet::admit`]). The
//! record then pins the name and fingerprint that verified it, and every
//! later use verifies under exactly that entry ([`TrustSet::pinned`]). An
//! entry removed or replaced since is `key changed`: searching again could
//! find the same key trusted under another name and relabel the request.
//!
//! One key has one name. Trusting a key already trusted under another name
//! is refused, and a hand-edited set holding the same key twice is unusable
//! until it is fixed, so an admitting name never depends on search order.
//!
//! Names are labels, not isolation: every site's private key and this
//! config are reachable by the same Unix user (README, "User commands").

use super::envelope::{self, Envelope, Refusal, SiteKey, Verified};
use crate::cli::teams_config::UserCommandSection;
use anyhow::bail;
use std::collections::BTreeMap;

/// The longest key name.
pub(crate) const NAME_MAX: usize = 64;

/// A trusted key's name: a bounded `[A-Za-z0-9_-]` token.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct KeyName(String);

impl KeyName {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let ok = !text.is_empty()
            && text.len() <= NAME_MAX
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        ok.then(|| KeyName(text.to_string()))
            .ok_or_else(|| format!("a key name is 1 to {NAME_MAX} of [A-Za-z0-9_-]"))
    }

    /// `default`: the name of the single key trusted before keys had names,
    /// and so of every record admitted under it.
    pub(crate) fn legacy() -> Self {
        KeyName("default".into())
    }
}

impl TryFrom<String> for KeyName {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        KeyName::parse(&text)
    }
}

impl From<KeyName> for String {
    fn from(name: KeyName) -> String {
        name.0
    }
}

impl std::fmt::Display for KeyName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The entry a request was admitted under, which every later use of it
/// must find unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pin {
    pub(crate) name: KeyName,
    pub(crate) fingerprint: String,
}

/// The trusted keys as the config holds them now, or why they are unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustSet(Result<BTreeMap<KeyName, SiteKey>, String>);

impl TrustSet {
    /// The set `section` holds, its legacy key read as `default`.
    pub(crate) fn of(section: Option<&UserCommandSection>) -> Self {
        TrustSet(section.map_or(Ok(BTreeMap::new()), |s| {
            entries(s, false).map(|(keys, _)| keys)
        }))
    }

    /// A set no request verifies under, for `why`.
    pub(crate) fn unusable(why: String) -> Self {
        TrustSet(Err(why))
    }

    #[cfg(test)]
    pub(crate) fn from_keys<'a>(keys: impl IntoIterator<Item = (&'a str, SiteKey)>) -> Self {
        TrustSet(Ok(keys
            .into_iter()
            .map(|(n, k)| (KeyName::parse(n).unwrap(), k))
            .collect()))
    }

    pub(crate) fn keys(&self) -> Result<&BTreeMap<KeyName, SiteKey>, Refusal> {
        self.0
            .as_ref()
            .map_err(|why| Refusal::Untrusted(why.clone()))
    }

    /// Verify `envelope` at `now` under whichever trusted key signed it, and
    /// the entry that did.
    pub(crate) fn admit(&self, envelope: &Envelope, now: u64) -> Result<(Pin, Verified), Refusal> {
        let keys = self.keys()?;
        if keys.is_empty() {
            return Err(Refusal::NoTrustedKey);
        }
        for (name, key) in keys {
            match envelope::verify(envelope, key, now) {
                Err(Refusal::BadSignature) => continue,
                Err(other) => return Err(other),
                Ok(verified) => {
                    let pin = Pin {
                        name: name.clone(),
                        fingerprint: key.fingerprint(),
                    };
                    return Ok((pin, verified));
                }
            }
        }
        Err(Refusal::BadSignature)
    }

    /// Verify `envelope` at `now` under `pin`'s entry and no other.
    pub(crate) fn pinned(
        &self,
        pin: &Pin,
        envelope: &Envelope,
        now: u64,
    ) -> Result<Verified, Refusal> {
        match self.keys()?.get(&pin.name) {
            Some(key) if key.fingerprint() == pin.fingerprint => {
                envelope::verify(envelope, key, now)
            }
            _ => Err(Refusal::KeyChanged),
        }
    }
}

/// `section`'s keys with its legacy key folded in as `default`, and the
/// legacy key when `resolving` dropped it. A legacy key that differs from a
/// named `default` is refused, unless `resolving`: the caller is about to
/// set or remove `default` itself.
fn entries(
    section: &UserCommandSection,
    resolving: bool,
) -> Result<(BTreeMap<KeyName, SiteKey>, Option<SiteKey>), String> {
    let mut keys = BTreeMap::new();
    for (name, key) in &section.keys {
        let name = KeyName::parse(name).map_err(|e| format!("key name `{name}`: {e}"))?;
        let key = SiteKey::parse(key).map_err(|e| format!("key `{name}`: {e}"))?;
        keys.insert(name, key);
    }
    let mut dropped = None;
    if let Some(legacy) = &section.site_key {
        let legacy = SiteKey::parse(legacy).map_err(|e| format!("the legacy site_key: {e}"))?;
        match keys.get(&KeyName::legacy()) {
            None => {
                keys.insert(KeyName::legacy(), legacy);
            }
            Some(named) if *named == legacy => {}
            Some(_) if resolving => dropped = Some(legacy),
            Some(named) => {
                return Err(format!(
                    "the legacy site_key ({}) differs from the key named `default` ({}); \
                     `clank user-command trust-key --name default --replace` or `untrust-key \
                     --name default` decides",
                    legacy.fingerprint(),
                    named.fingerprint()
                ));
            }
        }
    }
    let mut seen: BTreeMap<&SiteKey, &KeyName> = BTreeMap::new();
    for (name, key) in &keys {
        if let Some(first) = seen.insert(key, name) {
            return Err(format!(
                "the key {} is trusted as both `{first}` and `{name}`; one key has one name",
                key.fingerprint()
            ));
        }
    }
    Ok((keys, dropped))
}

fn written(keys: BTreeMap<KeyName, SiteKey>) -> UserCommandSection {
    UserCommandSection {
        keys: keys.into_iter().map(|(n, k)| (n.0, k.encoded())).collect(),
        site_key: None,
    }
}

/// Trust `key` as `name` in `section`, writing any legacy key into the named
/// set. A different key already named so needs `replace`; a key trusted
/// under another name is refused.
pub(crate) fn trust(
    section: &mut UserCommandSection,
    name: &KeyName,
    key: &SiteKey,
    replace: bool,
) -> anyhow::Result<()> {
    let (mut keys, _) =
        entries(section, replace && *name == KeyName::legacy()).map_err(anyhow::Error::msg)?;
    if let Some((other, _)) = keys.iter().find(|(n, k)| *n != name && *k == key) {
        bail!("this key is already trusted as `{other}`; one key has one name");
    }
    match keys.get(name) {
        Some(old) if old == key => {}
        Some(old) if !replace => bail!(
            "`{name}` already trusts a different key ({}); pass --replace to replace it, which \
             refuses every request it admitted at their next use",
            old.fingerprint()
        ),
        _ => {
            keys.insert(name.clone(), key.clone());
        }
    }
    *section = written(keys);
    Ok(())
}

/// Stop trusting `name` in `section`, writing any legacy key into the named
/// set first: the fingerprints of the keys no longer trusted.
pub(crate) fn untrust(
    section: &mut UserCommandSection,
    name: &KeyName,
) -> anyhow::Result<Vec<String>> {
    let (mut keys, dropped) =
        entries(section, *name == KeyName::legacy()).map_err(anyhow::Error::msg)?;
    let Some(gone) = keys.remove(name) else {
        bail!("no key is trusted as `{name}`");
    };
    *section = written(keys);
    Ok(std::iter::once(gone)
        .chain(dropped)
        .map(|k| k.fingerprint())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::super::envelope::testkey::{NOW, Signer, payload};
    use super::*;
    use std::path::PathBuf;

    fn repo() -> PathBuf {
        PathBuf::from("/w/proj")
    }

    fn name(s: &str) -> KeyName {
        KeyName::parse(s).unwrap()
    }

    fn legacy(key: &SiteKey) -> UserCommandSection {
        UserCommandSection {
            keys: BTreeMap::new(),
            site_key: Some(key.encoded()),
        }
    }

    /// Each of two trusted sites' envelopes is admitted under its own name;
    /// a third key's is refused, and so is anything with no key trusted.
    #[test]
    fn each_trusted_key_admits_its_own_envelopes_and_no_other() {
        let (a, b, c) = (Signer::generate(), Signer::generate(), Signer::generate());
        let set = TrustSet::from_keys([("site-a", a.key()), ("site-b", b.key())]);
        for (signer, site) in [(&a, "site-a"), (&b, "site-b")] {
            let (pin, v) = set.admit(&signer.sign(&payload(&repo())), NOW).unwrap();
            assert_eq!(pin.name, name(site));
            assert_eq!(pin.fingerprint, signer.key().fingerprint());
            assert_eq!(v.id.as_str(), "r1");
        }
        let third = c.sign(&payload(&repo()));
        assert_eq!(set.admit(&third, NOW).unwrap_err(), Refusal::BadSignature);
        assert_eq!(
            TrustSet::of(None).admit(&third, NOW).unwrap_err(),
            Refusal::NoTrustedKey
        );
    }

    /// A pinned use verifies under its own entry only: that entry gone,
    /// replaced, or its key moved to another name is `key changed`.
    #[test]
    fn a_pin_verifies_under_its_entry_and_never_another() {
        let (a, b) = (Signer::generate(), Signer::generate());
        let envelope = a.sign(&payload(&repo()));
        let set = TrustSet::from_keys([("site-a", a.key()), ("site-b", b.key())]);
        let (pin, _) = set.admit(&envelope, NOW).unwrap();
        assert!(set.pinned(&pin, &envelope, NOW).is_ok());
        for changed in [
            TrustSet::from_keys([("site-b", b.key())]),
            TrustSet::from_keys([("site-a", b.key())]),
            TrustSet::from_keys([("moved", a.key()), ("site-b", b.key())]),
            TrustSet::of(None),
        ] {
            assert_eq!(
                changed.pinned(&pin, &envelope, NOW),
                Err(Refusal::KeyChanged),
                "{changed:?}"
            );
        }
    }

    #[test]
    fn the_legacy_key_reads_as_default() {
        let a = Signer::generate();
        let set = TrustSet::of(Some(&legacy(&a.key())));
        let (pin, _) = set.admit(&a.sign(&payload(&repo())), NOW).unwrap();
        assert_eq!(pin.name, KeyName::legacy());
    }

    /// The first trust mutation writes the legacy key into the named set and
    /// drops it, so untrusting `default` afterwards leaves nothing to read
    /// back.
    #[test]
    fn a_mutation_consumes_the_legacy_key() {
        let (a, b) = (Signer::generate(), Signer::generate());
        let mut section = legacy(&a.key());
        trust(&mut section, &name("site-b"), &b.key(), false).unwrap();
        assert_eq!(section.site_key, None);
        assert_eq!(
            section.keys.get("default"),
            Some(&a.key().encoded()),
            "{section:?}"
        );
        let gone = untrust(&mut section, &KeyName::legacy()).unwrap();
        assert_eq!(gone, vec![a.key().fingerprint()]);
        assert_eq!(
            TrustSet::of(Some(&section)),
            TrustSet::from_keys([("site-b", b.key())])
        );
        let mut section = legacy(&a.key());
        untrust(&mut section, &KeyName::legacy()).unwrap();
        assert_eq!(TrustSet::of(Some(&section)), TrustSet::of(None));
    }

    /// A legacy key beside a different named `default` is a collision: the
    /// set is unusable and only a mutation that decides `default` resolves
    /// it. The same key in both is no collision.
    #[test]
    fn a_legacy_key_never_silently_replaces_a_named_default() {
        let (old, named, other) = (
            Signer::generate().key(),
            Signer::generate().key(),
            Signer::generate().key(),
        );
        let both = |legacy_key: &SiteKey| UserCommandSection {
            keys: [("default".to_string(), named.encoded())].into(),
            site_key: Some(legacy_key.encoded()),
        };
        assert_eq!(
            TrustSet::of(Some(&both(&named))),
            TrustSet::from_keys([("default", named.clone())])
        );
        assert!(TrustSet::of(Some(&both(&old))).keys().is_err());
        let mut section = both(&old);
        assert!(trust(&mut section, &name("site-b"), &other, false).is_err());
        assert!(trust(&mut section, &KeyName::legacy(), &other, false).is_err());
        assert_eq!(section, both(&old), "nothing changed");
        trust(&mut section, &KeyName::legacy(), &other, true).unwrap();
        assert_eq!(
            TrustSet::of(Some(&section)),
            TrustSet::from_keys([("default", other.clone())])
        );
        let mut section = both(&old);
        let mut gone = untrust(&mut section, &KeyName::legacy()).unwrap();
        gone.sort();
        let mut want = vec![named.fingerprint(), old.fingerprint()];
        want.sort();
        assert_eq!(gone, want);
        assert_eq!(TrustSet::of(Some(&section)), TrustSet::of(None));
    }

    /// One key, one name: trusting a key under a second name is refused,
    /// with or without `--replace`, and a hand-written alias makes the set
    /// unusable rather than admit by search order.
    #[test]
    fn a_key_is_never_trusted_under_two_names() {
        let a = Signer::generate().key();
        let mut section = UserCommandSection::default();
        trust(&mut section, &name("site-a"), &a, false).unwrap();
        for replace in [false, true] {
            assert!(trust(&mut section, &name("site-b"), &a, replace).is_err());
        }
        assert_eq!(
            TrustSet::of(Some(&section)),
            TrustSet::from_keys([("site-a", a.clone())])
        );
        section
            .keys
            .insert("site-b".into(), section.keys["site-a"].clone());
        assert!(matches!(
            TrustSet::of(Some(&section)).keys(),
            Err(Refusal::Untrusted(_))
        ));
    }

    #[test]
    fn replacing_a_name_needs_replace_and_keeps_the_others() {
        let (a, b, c) = (
            Signer::generate().key(),
            Signer::generate().key(),
            Signer::generate().key(),
        );
        let mut section = UserCommandSection::default();
        trust(&mut section, &name("site-a"), &a, false).unwrap();
        trust(&mut section, &name("site-b"), &b, false).unwrap();
        trust(&mut section, &name("site-a"), &a, false).unwrap();
        assert!(trust(&mut section, &name("site-a"), &c, false).is_err());
        trust(&mut section, &name("site-a"), &c, true).unwrap();
        assert_eq!(
            TrustSet::of(Some(&section)),
            TrustSet::from_keys([("site-a", c), ("site-b", b)])
        );
        assert!(untrust(&mut section, &name("nobody")).is_err());
        assert!(KeyName::parse("../x").is_err());
        assert!(KeyName::parse(&"x".repeat(NAME_MAX + 1)).is_err());
    }
}
