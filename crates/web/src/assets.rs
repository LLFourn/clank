//! Session page and vendored browser assets shared by every host.

pub const PAGE: &str = include_str!("../assets/page.html");
pub const COMPOSER_JS: &str = include_str!("../assets/composer.js");
pub const JSQR: &str = include_str!("../assets/vendor/jsQR.js");
pub const XTERM_JS: &str = include_str!("../assets/vendor/xterm.js");
pub const XTERM_CSS: &str = include_str!("../assets/vendor/xterm.css");
pub const PLATE_FONT: &[u8] = include_bytes!("../assets/vendor/plate.woff2");
pub const VENDOR_NOTICE: &str = include_str!("../assets/vendor/README.md");

const SESSION_ANCHOR: &str = "window.CLANK_SESSION || 'clank'";
const CONFIG_ANCHOR: &str =
    "window.CLANK_WEB_CONFIG || {base:'/',hosted:false,login:'/login',signout:null}";
const DOOR_ANCHOR: &str = "/*__CLANK_DOOR__*/";
const COMPOSER_ANCHOR: &str = "/*__CLANK_COMPOSER__*/";
const BASE_ANCHOR: &str = "__CLANK_BASE_PATH__";

/// The built-in host supplies its credential script; another host supplies
/// its own login and optional sign-out routes. Host mode never provisions or
/// stores Clank's token in the browser.
pub enum BrowserAuth<'a> {
    ClankDoor {
        script: &'a str,
    },
    Host {
        login_path: &'a str,
        sign_out_path: Option<&'a str>,
    },
}

pub struct PageOptions<'a> {
    pub session: &'a str,
    /// An absolute, trailing-slash path such as `/teams/alpha/`.
    pub base_path: &'a str,
    pub auth: BrowserAuth<'a>,
}

/// Render the shared session page for an explicit mount and browser policy.
/// Paths are restricted before entering HTML or JavaScript; the session name
/// is encoded as a JavaScript string without an executable `</script>`.
pub fn render_page(options: PageOptions<'_>) -> anyhow::Result<String> {
    anyhow::ensure!(valid_path(options.base_path, true), "invalid mount path");
    for anchor in [
        SESSION_ANCHOR,
        CONFIG_ANCHOR,
        DOOR_ANCHOR,
        COMPOSER_ANCHOR,
        BASE_ANCHOR,
    ] {
        anyhow::ensure!(
            PAGE.contains(anchor),
            "session page anchor missing: {anchor}"
        );
    }
    let (hosted, login, signout, door) = match options.auth {
        BrowserAuth::ClankDoor { script } => {
            (false, format!("{}login", options.base_path), None, script)
        }
        BrowserAuth::Host {
            login_path,
            sign_out_path,
        } => {
            anyhow::ensure!(valid_path(login_path, false), "invalid login path");
            if let Some(path) = sign_out_path {
                anyhow::ensure!(valid_path(path, false), "invalid sign-out path");
            }
            (true, login_path.to_owned(), sign_out_path, "")
        }
    };
    let config = serde_json::json!({
        "base": options.base_path,
        "hosted": hosted,
        "login": login,
        "signout": signout,
    });
    let page = PAGE
        .replace(BASE_ANCHOR, options.base_path)
        .replace(SESSION_ANCHOR, &inline_json(&options.session)?)
        .replace(CONFIG_ANCHOR, &inline_json(&config)?)
        .replace(DOOR_ANCHOR, door)
        .replace(COMPOSER_ANCHOR, COMPOSER_JS);
    Ok(page)
}

fn inline_json(value: &impl serde::Serialize) -> anyhow::Result<String> {
    Ok(serde_json::to_string(value)?
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026"))
}

fn valid_path(path: &str, trailing_slash: bool) -> bool {
    if path == "/" {
        return true;
    }
    if !path.starts_with('/') || path.starts_with("//") || path.len() > 512 {
        return false;
    }
    if trailing_slash != path.ends_with('/') {
        return false;
    }
    let body = if trailing_slash {
        &path[1..path.len() - 1]
    } else {
        &path[1..]
    };
    body.split('/').all(|segment| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            && segment != "."
            && segment != ".."
    })
}

#[cfg(test)]
mod tests {
    use super::{BrowserAuth, PageOptions, render_page};

    #[test]
    fn host_page_uses_its_mount_and_does_not_invoke_the_clank_door() {
        let page = render_page(PageOptions {
            session: "team",
            base_path: "/teams/alpha/",
            auth: BrowserAuth::Host {
                login_path: "/auth/login",
                sign_out_path: Some("/auth/logout"),
            },
        })
        .unwrap();
        assert!(page.contains("href=\"/teams/alpha/xterm.css\""));
        assert!(page.contains("src=\"/teams/alpha/xterm.js\""));
        assert!(page.contains("url(\"/teams/alpha/plate.woff2\")"));
        assert!(page.contains("\"base\":\"/teams/alpha/\""));
        assert!(page.contains("\"login\":\"/auth/login\""));
        assert!(page.contains("\"signout\":\"/auth/logout\""));
        assert!(page.contains("if (!web.hosted) provision()"));
        assert!(page.contains("${route('/events')}"));
        assert!(page.contains("url.pathname.startsWith(route('/html/'))"));
        assert!(!page.contains("/*__CLANK_DOOR__*/"));
        assert!(!page.contains("function doorStart"));
    }

    #[test]
    fn rejects_unsafe_paths_and_escapes_session_name() {
        for bad in [
            "//other/",
            "/team/../",
            "/team//",
            "/team?q=1/",
            "/team%2f/",
        ] {
            assert!(
                render_page(PageOptions {
                    session: "team",
                    base_path: bad,
                    auth: BrowserAuth::Host {
                        login_path: "/login",
                        sign_out_path: None
                    },
                })
                .is_err(),
                "{bad}"
            );
        }
        let page = render_page(PageOptions {
            session: "</script><script>alert(1)</script>",
            base_path: "/",
            auth: BrowserAuth::Host {
                login_path: "/login",
                sign_out_path: None,
            },
        })
        .unwrap();
        assert!(!page.contains("</script><script>alert(1)</script>"));
        assert!(page.contains("\\u003c/script\\u003e"));
    }

    #[test]
    fn built_in_page_keeps_the_root_routes_and_default_door() {
        let page = render_page(PageOptions {
            session: "default",
            base_path: "/",
            auth: BrowserAuth::ClankDoor {
                script: "function doorStart() {}",
            },
        })
        .unwrap();
        assert!(page.contains("href=\"/xterm.css\""));
        assert!(page.contains("\"hosted\":false"));
        assert!(page.contains("\"login\":\"/login\""));
        assert!(page.contains("function doorStart() {}"));
    }
}
