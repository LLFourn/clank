APPROVE

The renderer test now exercises the real `render_amend_dry` output and asserts it includes the program HEAD, every strip path, and the dry-run notice. The typed `AmendProgram` flow and stale-HEAD guard remain in place.

Tests run:
- `cargo test -p clank cli::purge`
- `cargo fmt --check`
