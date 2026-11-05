//! `clank licenses`: the licence notices a standalone binary has to
//! carry, embedded so they travel with it.

const LICENSE: &str = include_str!("../../LICENSE");
const VENDORED: &str = include_str!("../../THIRD-PARTY-NOTICES.md");
/// Written by `scripts/notices/generate.sh`; CI fails when it is stale.
const RUST_DEPENDENCIES: &str = include_str!("../../notices/rust-dependencies.txt");

/// Clank's own licence, then the vendored assets' notices, then every
/// Rust dependency's.
pub fn notice() -> String {
    format!("{LICENSE}\n\n{VENDORED}\n\n{RUST_DEPENDENCIES}")
}

pub fn run() -> anyhow::Result<()> {
    print!("{}", notice());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const VENDOR_DIRS: &[&str] = &[
        "crates/web/assets/vendor",
        "book/fonts",
        "book/vendor/asciinema-player",
    ];

    fn is_licence(name: &str) -> bool {
        name == "LICENSE" || name.ends_with(".LICENSE") || name.starts_with("OFL-")
    }

    fn is_readme(name: &str) -> bool {
        name == "README" || name == "README.md"
    }

    #[test]
    fn every_vendored_file_has_its_notice() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let printed = notice();
        let mut assets = 0;
        for dir in VENDOR_DIRS {
            for entry in std::fs::read_dir(root.join(dir)).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_str().unwrap();
                if is_readme(name) {
                    continue;
                }
                if is_licence(name) {
                    let text = std::fs::read_to_string(&path).unwrap();
                    assert!(
                        printed.contains(text.trim_end()),
                        "{dir}/{name} is not copied into THIRD-PARTY-NOTICES.md"
                    );
                } else {
                    assets += 1;
                    assert!(
                        VENDORED.contains(&format!("`{name}`")),
                        "{dir}/{name} is not named in THIRD-PARTY-NOTICES.md"
                    );
                }
            }
        }
        assert!(assets >= 10, "found only {assets} vendored assets");
    }

    #[test]
    fn clanks_own_licence_is_printed() {
        assert!(
            notice().starts_with("BSD Zero Clause License\n\nCopyright (c) 2026 Lloyd Fourn\n")
        );
    }

    struct Entry<'a> {
        lines: Vec<&'a str>,
    }

    impl<'a> Entry<'a> {
        fn field(&self, label: &str) -> &'a str {
            let prefix = format!("  {label}:");
            let line = self
                .lines
                .iter()
                .find(|l| l.starts_with(&prefix))
                .unwrap_or_else(|| panic!("no {label} in {:?}", self.lines));
            line[prefix.len()..].trim()
        }

        fn texts(&self) -> Vec<&'a str> {
            let start = self
                .lines
                .iter()
                .position(|l| l.starts_with("  texts:"))
                .unwrap();
            let mut refs = self.lines[start]["  texts:".len()..].to_string();
            for l in &self.lines[start + 1..] {
                if !l.starts_with(&" ".repeat(14)) {
                    break;
                }
                refs.push_str(l);
            }
            refs.split(',')
                .map(|r| {
                    let n = r.trim().split(']').next().unwrap().trim_start_matches('[');
                    licence_text(n)
                })
                .collect()
        }
    }

    fn entry(name: &str) -> Entry<'static> {
        let heading = format!("\n{name} ");
        let at = RUST_DEPENDENCIES
            .find(&heading)
            .unwrap_or_else(|| panic!("{name} is not in rust-dependencies.txt"));
        let lines = RUST_DEPENDENCIES[at + 1..]
            .lines()
            .take_while(|l| !l.is_empty())
            .collect();
        Entry { lines }
    }

    fn licence_text(number: &str) -> &'static str {
        let heading = format!("\n[{number}] ");
        let at = RUST_DEPENDENCIES
            .find(&heading)
            .unwrap_or_else(|| panic!("no licence text [{number}]"));
        let rest = &RUST_DEPENDENCIES[at + 1..];
        &rest[..rest.find(&"-".repeat(78)).unwrap()]
    }

    #[test]
    fn tokio_carries_its_mit_text_and_copyright() {
        let tokio = entry("tokio");
        assert_eq!(tokio.field("used under"), "MIT");
        assert!(
            tokio
                .texts()
                .iter()
                .any(|t| t.contains("MIT License\n\nCopyright (c) Tokio Contributors\n"))
        );
    }

    #[test]
    fn unicode_ident_names_its_chosen_branch_and_every_and_part() {
        let ident = entry("unicode-ident");
        assert_eq!(
            ident.field("declared"),
            "(MIT OR Apache-2.0) AND Unicode-3.0"
        );
        assert_eq!(ident.field("used under"), "MIT AND Unicode-3.0");
        let texts = ident.texts();
        assert!(texts.iter().any(|t| t.contains(" MIT: ")));
        assert!(
            texts
                .iter()
                .any(|t| t.contains(" Unicode-3.0: ") && t.contains("UNICODE LICENSE V3"))
        );
    }

    #[test]
    fn uluru_carries_the_mpl_and_where_its_source_is() {
        let uluru = entry("uluru");
        assert_eq!(uluru.field("used under"), "MPL-2.0");
        assert!(
            uluru
                .texts()
                .iter()
                .any(|t| t.contains("Mozilla Public License Version 2.0"))
        );
        let version = uluru.lines[0].strip_prefix("uluru ").unwrap();
        assert_eq!(
            uluru.field("Source Code Form (MPL-2.0 section 3.2)"),
            format!("https://crates.io/api/v1/crates/uluru/{version}/download")
        );
    }
}
