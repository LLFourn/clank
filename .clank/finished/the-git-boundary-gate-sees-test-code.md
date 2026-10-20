# the-git-boundary-gate-sees-test-code
# The git boundary gate sees test code as test code

`cargo test -p clank --test it git_boundary::` is red on master, and the
two sites it flags are test code, not production:

- `crates/cli/src/cli/stop_hook.rs`: `Command::new("git")` at line 4143 is
  inside `#[cfg(test)] mod tests` (from line 3532). The gate reports it at
  4287 of its stripped text, so `strip_test_code`
  (`crates/cli/tests/it/common`) stopped stripping partway through that
  module, probably at a brace inside a string or char literal.
- `crates/cli/src/cli/grok_hook_tests.rs:12`: the whole file is a test
  module, included from `grok_hook.rs:146` with `#[path = "grok_hook_tests.rs"]`
  under `#[cfg(test)]`. Nothing inside the file says so, so the scanner reads
  it as production.

Found while finishing `cli-help-is-beautiful`; that plan does not touch
either file.

## Change

- Find what makes `strip_test_code` lose its place in `stop_hook.rs`'s test
  module and fix the scanner (not the test), with a fixture of that shape
  in `scanner_flags_prod_and_exempts_test_code`.
- Treat a file included as a `#[cfg(test)] #[path = …]` module as test code.
  Derive this from the including `mod` declaration, not from the file name.
- Run the other source-scanning gates that share `strip_test_code`
  (`zellij_ownership_boundary`, `zellij_cost_boundary`,
  `no_json_literal_config_writes`) and confirm none regresses.

## Acceptance

- `git_boundary::` is green, and a raw `git` call added to production code
  in either file still fails it (mutation-checked).

## As built

- The cause in `stop_hook.rs` was `'\u{1b}'`: the lexer took every escaped
  char literal as four bytes, so the `}` of `\u{1b}` counted as code and
  ended the test module at line 6789, before the fixture `git` call at 7671.
  Escaped literals now run to their closing quote, and unescaped ones are
  measured by the char's UTF-8 length.
- `strip_test_code` ends a `#[cfg(test)]` item at a `;` before any brace,
  so `#[cfg(test)] #[path = "…"] mod tests;` no longer swallows the
  production after it, and keeps dropped lines as empty lines so reported
  line numbers are source line numbers.
- `common::Production` classifies whole files from each file's parsed module
  tree (`syn`, test-only dependency): attributes attached to their item,
  inline modules nested, `#[path]` resolved from the file's directory at top
  level and from the inline module's directory inside one. Comments and
  literals cannot create an inclusion. A file is test-only when every
  inclusion is in test code or in a test-only file; if any production
  declaration cannot be resolved, or any file fails to parse, nothing is
  exempt. `git_boundary`, `identity_env_boundary`, `signal_boundary` and
  `clank_dir_boundary` read through it; `zellij_ownership_boundary` keeps its
  own walk, since it scans test code on purpose.
- Fixtures: the escaped-literal shapes, semicolon items, line preservation,
  and file classification: guarded, one-line, unguarded, a production
  inclusion with a comment between attribute and item, a declaration quoted
  in a raw string, no inclusion, an inline module's `#[path]` (loading
  `tests/shared.rs`, not the root `shared.rs`), inheritance both ways, an
  unresolved declaration and an unparseable file. Mutants caught: the old
  escaped-char arm, no semicolon end, dropped lines removed, `cfg(test)`
  unrecognised, inline `#[path]` resolved from the file, uncertainty
  ignored, `any` for `all`, no classification, and a production `git` call
  added before `stop_hook`'s tests.
- `clank_dir_boundary` was already red before this plan, with the same five
  sites; it is queued separately.
