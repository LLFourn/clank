//! Reads a script the way a browser will, for the tests that stand
//! between this binary and a page that cannot run.
//!
//! Two surfaces ship JavaScript from this binary: the web session
//! (`clank-web` and its default host) and the generated site (`html.rs`). A SyntaxError in
//! an inline script is the whole page, and it is silent — the build
//! succeeds, the file still contains every string a grep looks for,
//! and nothing works. One guard for that fault, not one per surface.

/// How a browser will read a script: the pages' own blocks are
/// classic, a `type="module"` block is a module, and the two are not
/// interchangeable — module syntax does not run in a classic script,
/// and a classic script's top-level `return` is not a module's
/// (codex on b5170be).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Classic,
    Module,
}

/// A script as a JavaScript engine reads it: parsed, then its scopes
/// built, so the early errors a browser reports before running a line
/// are reported here too. A redeclaration is one of those, and it is
/// not a parse error — it is a fact about a SCOPE, which is why the
/// check has to know about scopes and a grep over the file never
/// could.
///
/// Nothing is executed: there is no DOM here to execute against.
pub(crate) fn parse_as(script: &str, kind: Kind) -> Result<(), String> {
    let allocator = oxc_allocator::Allocator::default();
    // Spelled out, because this type's Default is a MODULE: as one,
    // `export`, a top-level `await` and `import.meta` all pass, and
    // none of them runs in what is shipped.
    let source = match kind {
        Kind::Classic => oxc_span::SourceType::script(),
        Kind::Module => oxc_span::SourceType::mjs(),
    };
    let parsed = oxc_parser::Parser::new(&allocator, script, source).parse();
    if let Some(e) = parsed.diagnostics.first() {
        return Err(format!("{e:?}"));
    }
    // The checks that catch `break` outside a loop or a private field
    // declared twice are off unless asked for. A guard that accepts
    // what the browser rejects is the fault this exists to catch
    // (codex on 79fc29f).
    let semantic = oxc_semantic::SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&parsed.program);
    match semantic.diagnostics.first() {
        Some(e) => Err(format!("{e:?}")),
        None => Ok(()),
    }
}

pub(crate) fn parse_js(script: &str) -> Result<(), String> {
    parse_as(script, Kind::Classic)
}

/// One `<script>` tag as the page carries it: what stood between
/// `<script` and `>`, and the source up to the closing tag.
pub(crate) struct Tag<'a> {
    pub attrs: &'a str,
    pub body: &'a str,
}

/// Every `<script` in a page, by its OPEN TAG rather than by a
/// literal `<script>` that an attribute slips past: a page with one
/// plain block and one attributed module would otherwise pass while
/// the module went unread — a hole of exactly the kind this guard
/// exists to close (ruthless on 9473655).
///
/// Callers decide what each tag means, and are expected to refuse a
/// shape nobody decided about rather than skip it.
pub(crate) fn script_tags(html: &str) -> Vec<Tag<'_>> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("<script") {
        let after = &rest[at + "<script".len()..];
        let shut = after.find('>').expect("an open tag that ends");
        let attrs = after[..shut].trim();
        let body = &after[shut + 1..];
        let end = body.find("</script>").expect("a script that is closed");
        out.push(Tag {
            attrs,
            body: &body[..end],
        });
        rest = &body[end..];
    }
    out
}

/// The faults this guard is FOR, each one a program a browser refuses
/// to run. PRIVATE on purpose: its only reader is this module's own
/// test, so a surface cannot grow a second copy of the assertion — the
/// duplication is unwritable rather than merely absent (ruthless on
/// 8fe222d).
///
/// The first is the shape that actually shipped: a name declared
/// twice inside a closure, which is where a page's script lives. A
/// global-only check reads it as fine.
const BROWSER_REFUSES: &[&str] = &[
    "(() => { const a = 1; async function a() {} })();",
    "const a = ;",
    "return;",
    "(() => { break; })();",
    "(() => { continue; })();",
    "class C { #x; #x; }",
    "'use strict'; with (x) {}",
    // Module syntax: valid somewhere, not in a classic script, which
    // is what both surfaces' inline blocks load as.
    "export {};",
    "await 0;",
    "import.meta;",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard is shown to fail on the faults it stands for. Both
    /// surfaces lean on this, so it is checked once here rather than
    /// re-asserted in each of them.
    #[test]
    fn the_guard_refuses_what_a_browser_refuses() {
        for src in BROWSER_REFUSES {
            assert!(
                parse_js(src).is_err(),
                "a browser refuses `{src}`, and so must this"
            );
        }
        assert!(parse_js("(() => { const a = 1; async function b() {} })();").is_ok());
        // The two kinds mean different things, which is the whole
        // reason a tag's own attributes decide which it is read as.
        assert!(parse_as("export {};", Kind::Module).is_ok());
        assert!(parse_as("export {};", Kind::Classic).is_err());
    }

    #[test]
    fn a_tag_is_found_by_its_open_tag_not_by_a_bare_literal() {
        let tags = script_tags(
            "<script>a()</script><script type=\"module\">export {};</script>\
             <script nonce=\"x\">b()</script>",
        );
        assert_eq!(tags.len(), 3, "an attributed tag is not skipped");
        assert_eq!(tags[0].attrs, "");
        assert_eq!(tags[0].body, "a()");
        assert_eq!(tags[1].attrs, "type=\"module\"");
        assert_eq!(tags[2].attrs, "nonce=\"x\"");
    }

    #[test]
    fn the_shipped_web_pages_and_vendor_scripts_parse() {
        let session = clank_web::assets::render_page(clank_web::assets::PageOptions {
            session: "team",
            base_path: "/",
            auth: clank_web::assets::BrowserAuth::ClankDoor {
                script: clank_web_cloudflare::DOOR_JS,
            },
        })
        .unwrap();
        let login = clank_web_cloudflare::LOGIN
            .replace("/*__CLANK_DOOR__*/", clank_web_cloudflare::DOOR_JS);
        for (name, page) in [("session", session), ("login", login)] {
            assert!(
                !page.contains("/*__CLANK_DOOR__*/"),
                "{name} embeds Door decisions"
            );
            assert!(
                !page.contains("/*__CLANK_COMPOSER__*/"),
                "{name} embeds the composer"
            );
            let mut inline = 0;
            for tag in script_tags(&page) {
                if tag.attrs.contains("src=") {
                    assert_eq!(
                        tag.attrs, "src=\"/xterm.js\"",
                        "{name} loads only an inventoried local script"
                    );
                    continue;
                }
                let kind = if tag.attrs.contains("type=\"module\"") {
                    Kind::Module
                } else {
                    assert!(
                        tag.attrs.contains("nonce="),
                        "{name} inline script has a nonce"
                    );
                    Kind::Classic
                };
                parse_as(tag.body, kind)
                    .unwrap_or_else(|error| panic!("{name} inline script does not parse: {error}"));
                inline += 1;
            }
            assert!(inline > 0, "{name} shipped no checked inline script");
            for (at, _) in page.match_indices("//") {
                let before = &page[..at];
                if before.ends_with("http:") || before.ends_with("https:") {
                    panic!(
                        "{name} names an external host: {}",
                        &page[at..(at + 50).min(page.len())]
                    );
                }
            }
        }
        parse_js(clank_web::assets::XTERM_JS).expect("shipped terminal script parses");
        parse_js(clank_web::assets::JSQR).expect("shipped QR decoder parses");
    }

    #[test]
    fn shipped_vendor_assets_match_their_notice() {
        for (name, bytes, digest, license) in [
            (
                "jsQR.js",
                clank_web::assets::JSQR.as_bytes(),
                "bc40c8a15196236b2314db0856f72ca0b49980cd5413b8c852a7349f5fee0859",
                "Apache-2.0",
            ),
            (
                "xterm.js",
                clank_web::assets::XTERM_JS.as_bytes(),
                "4196e242ef1cf4c2adead8d97f4a772a69576076f70b095e004b4abbb049e7bf",
                "MIT",
            ),
            (
                "xterm.css",
                clank_web::assets::XTERM_CSS.as_bytes(),
                "f7f724aea2bb620a6482bfb8e4bdecfae1152b0c7facef55fbda61f3b6cfedb2",
                "MIT",
            ),
            (
                "plate.woff2",
                clank_web::assets::PLATE_FONT,
                "723569a183da9c0bb9fb829bd3a3fb3c6e044e0ad405a6feb6fce8a45480fb52",
                "OFL-1.1",
            ),
        ] {
            use sha2::Digest;
            let actual: String = sha2::Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(actual, digest, "{name} changed from the documented build");
            assert!(
                clank_web::assets::VENDOR_NOTICE.contains(digest)
                    && clank_web::assets::VENDOR_NOTICE.contains(license),
                "{name} and its license are documented"
            );
        }
    }
}
