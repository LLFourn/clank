//! Belt-and-suspenders guardrail: no production code path under `daemon::{http,ui}`
//! or `tools::{master,reviewer}` reaches for `events.payload.get("text")` to
//! render feedback. Feedback body must come from `FeedbackRecord`.
//!
//! The single tolerated payload accessor is the `feedback_updated_prior_body`
//! helper in `ui.rs`, which reads `prior_body` (different key). This grep
//! only checks for `.get("text")`, so that helper is fine.

const SOURCES: &[(&str, &str)] = &[
    ("src/daemon/http.rs", include_str!("../src/daemon/http.rs")),
    ("src/daemon/ui.rs", include_str!("../src/daemon/ui.rs")),
    (
        "src/tools/master.rs",
        include_str!("../src/tools/master.rs"),
    ),
    (
        "src/tools/reviewer.rs",
        include_str!("../src/tools/reviewer.rs"),
    ),
];

#[test]
fn no_payload_text_get_in_production_paths() {
    let mut offenders = Vec::new();
    for (name, src) in SOURCES {
        if src.contains(".get(\"text\")") {
            offenders.push(*name);
        }
    }
    assert!(
        offenders.is_empty(),
        "the following files still parse feedback body from events.payload via .get(\"text\"): {offenders:?}"
    );
}
