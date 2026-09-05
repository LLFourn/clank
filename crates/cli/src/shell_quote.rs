//! One shell quoter for every command line clank prints for a human
//! or an agent to paste. Three copies used to disagree on whether
//! `claude` needed quotes; a recipe built from a user-supplied string
//! is where the difference stops being cosmetic.

/// POSIX single-quote escaping. A word made only of characters no
/// shell interprets is left bare, so common lines stay readable;
/// anything else — spaces, `$`, backticks, quotes, an empty string —
/// is single-quoted, inside which nothing expands, with embedded
/// single quotes closed out by the `'\''` dance.
pub fn shell_quote(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '='));
    if bare {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_when_no_shell_would_touch_it() {
        assert_eq!(shell_quote("simple/path.md"), "simple/path.md");
        assert_eq!(shell_quote("claude"), "claude");
        assert_eq!(shell_quote("--expect=5m"), "--expect=5m");
    }

    /// Everything a pasted line could otherwise execute or lose: each
    /// comes back as one literal word.
    #[test]
    fn quoted_when_a_shell_would_interpret_it() {
        assert_eq!(shell_quote("a b.md"), "'a b.md'");
        assert_eq!(shell_quote("isn't.md"), r"'isn'\''t.md'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("`id`"), "'`id`'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote(r#"say "hi""#), r#"'say "hi"'"#);
        assert_eq!(shell_quote(r"back\slash"), r"'back\slash'");
        assert_eq!(shell_quote(""), "''", "an empty word is still a word");
    }
}
