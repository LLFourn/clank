//! The web Timeline is a navigation surface. Keep content and actions on
//! their owning pages so a narrow screen never grows a management form or
//! plan editor inside the list.

const PAGE: &str = include_str!("../../../web/assets/page.html");

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    source
        .split_once(start)
        .unwrap_or_else(|| panic!("missing boundary start: {start}"))
        .1
        .split_once(end)
        .unwrap_or_else(|| panic!("missing boundary end: {end}"))
        .0
}

fn forbidden_timeline_markup(markup: &str) -> Vec<&'static str> {
    [
        "<form",
        "<input",
        "<textarea",
        "<select",
        "<details",
        "<button",
        "contenteditable",
        " onclick=",
        " onchange=",
        " onsubmit=",
        "queue-editor",
        "stash-detail",
        "team-rows",
        "team-add",
        "forget",
    ]
    .into_iter()
    .filter(|needle| markup.contains(needle))
    .collect()
}

#[test]
fn web_timeline_boundary() {
    let ledger = section(PAGE, "<aside class=\"ledger\" id=\"ledger\">", "</aside>");
    assert!(
        forbidden_timeline_markup(ledger).is_empty(),
        "Timeline contains content or action markup: {:?}",
        forbidden_timeline_markup(ledger)
    );
    for link in ["id=\"roster\"", "id=\"queue-new\""] {
        assert!(ledger.contains(link), "missing Timeline navigation {link}");
    }

    // Generated plan links open pages. Roster links select an agent;
    // neither navigation surface dispatches a mutation.
    let delegated = section(
        PAGE,
        "ledgerEl.addEventListener('click', (e) => {",
        "// ---- status ----",
    );
    let roster = section(
        PAGE,
        "$('roster').addEventListener('click', (e) => {",
        "function settingsWhy(",
    );
    assert!(roster.contains("show(target())"));
    let create = section(
        PAGE,
        "$('queue-new').addEventListener('click', (e) => {",
        "$('queue-cancel').addEventListener('click', () => {",
    );
    for (name, handler) in [
        ("generated page links", delegated),
        ("new plan link", create),
    ] {
        assert!(
            handler.contains("openSitePage("),
            "{name} no longer opens a page"
        );
        for mutation in [
            "fetch(",
            "askQueue(",
            "askStash(",
            "askAgent(",
            "dispatch(",
            "requestSubmit(",
        ] {
            assert!(!handler.contains(mutation), "{name} dispatches {mutation}");
        }
    }
    for mutation in ["fetch(", "askAgent(", "dispatch(", "requestSubmit("] {
        assert!(
            !roster.contains(mutation),
            "roster navigation dispatches {mutation}"
        );
    }
    for listener in [
        "$('queue').addEventListener",
        "$('stash-list').addEventListener",
        "$('team').addEventListener",
    ] {
        assert!(
            !PAGE.contains(listener),
            "Timeline action listener returned: {listener}"
        );
    }
}

#[test]
fn timeline_markup_gate_rejects_controls() {
    for control in [
        "<form></form>",
        "<input>",
        "<button>Go</button>",
        "<details></details>",
        "<span contenteditable>",
    ] {
        assert!(
            !forbidden_timeline_markup(control).is_empty(),
            "missed {control}"
        );
    }
}
