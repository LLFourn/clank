//! The eMudhra incident (RUSTSEC-2026-0382) is fixed in two places, and
//! a lockfile change can undo either one without failing anything else.
//!
//! The bundled roots behind reqwest and tungstenite drop emSign ECC
//! Root CA - G3 from webpki-roots 1.0.10. The quick tunnel's QUIC client
//! trusts the system store plus Cloudflare's roots instead, so only the
//! verifier, rustls-webpki 0.103.16, rejects the distrusted
//! intermediates there. Hickory's DoT fallback runs rustls-webpki
//! 0.101 with no roots at this feature resolution, so it trusts nothing.

fn real_lock() -> String {
    std::fs::read_to_string(
        crate::common::crates_dir()
            .parent()
            .unwrap()
            .join("Cargo.lock"),
    )
    .expect("Cargo.lock at the workspace root")
}

/// Each package in `lock`: its name, version, and the dependency names
/// it resolved (optional ones appear only when a feature enables them).
fn packages(lock: &str) -> Vec<(String, Vec<u64>, Vec<String>)> {
    lock.split("[[package]]")
        .skip(1)
        .filter_map(|p| {
            let field = |key: &str| {
                p.lines()
                    .find_map(|l| l.strip_prefix(&format!("{key} = \"")))
                    .map(|v| v.trim_end_matches('"').to_string())
            };
            let version = field("version")?
                .split(['.', '-', '+'])
                .take(3)
                .map(|n| n.parse().unwrap_or(0))
                .collect();
            let deps = p
                .split("dependencies = [")
                .nth(1)
                .map(|l| {
                    l.split(']')
                        .next()
                        .unwrap_or_default()
                        .lines()
                        .map(|d| d.trim().trim_end_matches(',').trim_matches('"'))
                        .filter(|d| !d.is_empty())
                        .map(|d| d.split(' ').next().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default();
            Some((field("name")?, version, deps))
        })
        .collect()
}

/// Why `lock` misses an incident fix: a 1.x webpki-roots below 1.0.10,
/// or a 0.103 rustls-webpki below 0.103.16.
fn missing_fixes(lock: &str) -> Vec<String> {
    let pkgs = packages(lock);
    let mut why = Vec::new();
    for (name, v, _) in &pkgs {
        if name == "webpki-roots" && v[0] == 1 && *v < vec![1, 0, 10] {
            why.push(format!(
                "webpki-roots {v:?} still bundles emSign ECC Root CA - G3"
            ));
        }
        if name == "rustls-webpki" && v[..2] == [0, 103] && *v < vec![0, 103, 16] {
            why.push(format!(
                "rustls-webpki {v:?} accepts the distrusted intermediates"
            ));
        }
    }
    if !pkgs
        .iter()
        .any(|(n, v, _)| n == "rustls-webpki" && v[..2] == [0, 103])
    {
        why.push("no rustls-webpki 0.103: the live verifier changed, review this gate".into());
    }
    why
}

/// Why the older verifier is no longer the rootless DoT path: another
/// non-0.103 rustls-webpki line, or Hickory taking a root source.
fn older_verifier_trusts(lock: &str) -> Vec<String> {
    let pkgs = packages(lock);
    let mut why = Vec::new();
    let older: Vec<_> = pkgs
        .iter()
        .filter(|(n, v, _)| n == "rustls-webpki" && v[..2] != [0, 103])
        .map(|(_, v, _)| v.clone())
        .collect();
    if older != vec![vec![0, 101, 7]] {
        why.push(format!(
            "older rustls-webpki lines {older:?}: review their roots"
        ));
    }
    for (name, _, deps) in pkgs.iter().filter(|(n, _, _)| n.starts_with("hickory-")) {
        for d in deps
            .iter()
            .filter(|d| *d == "rustls-native-certs" || *d == "webpki-roots")
        {
            why.push(format!("{name} now takes DoT roots from {d}"));
        }
    }
    why
}

/// Whether a trust anchor's subject is one of the incident's roots:
/// eMudhra's emSign G3 roots (TLS, code signing, ECC) or Comodo's AAA
/// Certificate Services, which the incident's impacted chains route
/// through. The AAA root is not an emSign name, so it stands alone.
fn is_incident_root(subject: &[u8]) -> bool {
    let s = String::from_utf8_lossy(subject);
    (s.contains("emSign") && s.contains("G3")) || s.contains("AAA Certificate Services")
}

#[test]
fn no_incident_root_is_bundled() {
    let incident: Vec<String> = webpki_roots::TLS_SERVER_ROOTS
        .iter()
        .filter(|a| is_incident_root(a.subject.as_ref()))
        .map(|a| String::from_utf8_lossy(a.subject.as_ref()).into_owned())
        .collect();
    assert!(
        incident.is_empty(),
        "incident roots still trusted: {incident:?}"
    );
}

/// The predicate on real subjects: webpki-roots 0.25.4's AAA root and
/// 1.0.9's emSign ECC G3 root are incident roots; an emSign root and a
/// G3 root outside the incident are not.
#[test]
fn the_incident_predicate_rejects_the_incident_roots_and_nothing_else() {
    const AAA: &[u8] = b"1\x0b0\t\x06\x03U\x04\x06\x13\x02GB1\x1b0\x19\x06\x03U\x04\x08\x0c\x12Greater Manchester1\x100\x0e\x06\x03U\x04\x07\x0c\x07Salford1\x1a0\x18\x06\x03U\x04\n\x0c\x11Comodo CA Limited1!0\x1f\x06\x03U\x04\x03\x0c\x18AAA Certificate Services";
    const EMSIGN_ECC_G3: &[u8] = b"1\x0b0\t\x06\x03U\x04\x06\x13\x02IN1\x130\x11\x06\x03U\x04\x0b\x13\nemSign PKI1%0#\x06\x03U\x04\n\x13\x1ceMudhra Technologies Limited1 0\x1e\x06\x03U\x04\x03\x13\x17emSign ECC Root CA - G3";
    const EMSIGN_C1: &[u8] = b"1\x0b0\t\x06\x03U\x04\x06\x13\x02US1\x130\x11\x06\x03U\x04\x0b\x13\nemSign PKI1\x140\x12\x06\x03U\x04\n\x13\x0beMudhra Inc1\x1c0\x1a\x06\x03U\x04\x03\x13\x13emSign Root CA - C1";
    assert!(is_incident_root(AAA), "AAA Certificate Services");
    assert!(is_incident_root(EMSIGN_ECC_G3), "emSign ECC Root CA - G3");
    assert!(
        !is_incident_root(EMSIGN_C1),
        "emSign Root CA - C1 is not the incident's"
    );
    let digicert_g3 = webpki_roots::TLS_SERVER_ROOTS
        .iter()
        .find(|a| String::from_utf8_lossy(a.subject.as_ref()).contains("DigiCert Global Root G3"))
        .expect("DigiCert Global Root G3 is bundled");
    assert!(
        !is_incident_root(digicert_g3.subject.as_ref()),
        "a G3 root outside the incident"
    );
}

#[test]
fn the_lock_resolves_both_incident_fixes() {
    let why = missing_fixes(&real_lock());
    assert!(why.is_empty(), "{why:?}");
}

#[test]
fn the_only_older_verifier_is_the_rootless_dot_path() {
    let why = older_verifier_trusts(&real_lock());
    assert!(why.is_empty(), "{why:?}");
}

/// Replace `old` once in the real lock, failing loudly if it has moved.
fn reverted(old: &str, new: &str) -> String {
    let lock = real_lock();
    assert_eq!(lock.matches(old).count(), 1, "lock no longer holds {old:?}");
    lock.replacen(old, new, 1)
}

/// Each check fails on the lock that undoes what it guards.
#[test]
fn each_check_fails_on_the_lock_it_guards_against() {
    let roots = reverted(
        "name = \"webpki-roots\"\nversion = \"1.0.10\"",
        "name = \"webpki-roots\"\nversion = \"1.0.9\"",
    );
    assert_eq!(
        missing_fixes(&roots).len(),
        1,
        "{:?}",
        missing_fixes(&roots)
    );
    let verifier = reverted(
        "name = \"rustls-webpki\"\nversion = \"0.103.16\"",
        "name = \"rustls-webpki\"\nversion = \"0.103.15\"",
    );
    assert_eq!(
        missing_fixes(&verifier).len(),
        1,
        "{:?}",
        missing_fixes(&verifier)
    );
    let hickory = packages(&real_lock())
        .into_iter()
        .find(|(n, _, d)| n == "hickory-proto" && d.iter().any(|d| d == "tinyvec"))
        .expect("hickory-proto with its dependency list");
    assert!(!hickory.2.is_empty());
    let native = reverted(
        " \"tinyvec\",\n \"tokio\",\n \"tokio-rustls 0.24.1\",",
        " \"rustls-native-certs\",\n \"tinyvec\",\n \"tokio\",\n \"tokio-rustls 0.24.1\",",
    );
    assert_eq!(
        older_verifier_trusts(&native).len(),
        1,
        "{:?}",
        older_verifier_trusts(&native)
    );
}
