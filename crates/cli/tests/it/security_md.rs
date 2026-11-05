//! SECURITY.md's exposure table stays true: every exposure is fixed or
//! accepted, a fixed one names tests, and each named test exists.

use crate::common::crates_dir;

fn rust_sources(dir: &std::path::Path, out: &mut String) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                rust_sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push_str(&std::fs::read_to_string(&path).unwrap());
        }
    }
}

#[test]
fn every_exposure_names_its_test_or_its_reason() {
    let page = std::fs::read_to_string(crates_dir().join("../SECURITY.md")).unwrap();
    let mut sources = String::new();
    rust_sources(&crates_dir(), &mut sources);
    let table = page
        .split("## Exposures")
        .nth(1)
        .expect("an exposures section");
    let rows: Vec<Vec<&str>> = table
        .lines()
        .skip_while(|l| !l.starts_with("|---"))
        .skip(1)
        .take_while(|l| l.starts_with('|'))
        .map(|l| l.trim_matches('|').split('|').map(str::trim).collect())
        .collect();
    assert!(rows.len() >= 10, "the table was not read: {rows:?}");
    for row in &rows {
        let [exposure, status, why] = row[..] else {
            panic!("three columns: {row:?}");
        };
        match status {
            "Fixed" => {
                let tests: Vec<&str> = why.split('`').skip(1).step_by(2).collect();
                assert!(!tests.is_empty(), "`{exposure}` names no test");
                for test in tests {
                    assert!(
                        sources.contains(&format!("fn {test}(")),
                        "`{exposure}` names `{test}`, which is no test"
                    );
                }
            }
            "Accepted" => assert!(why.len() > 20, "`{exposure}` gives no reason"),
            other => panic!("`{exposure}` is neither fixed nor accepted: {other}"),
        }
    }
}
