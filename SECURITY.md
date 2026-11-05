# Security

clank runs coding agents on your machine, as you, against your
repositories. This page says what it protects, from whom, and what it
does not.

## Reporting a vulnerability

Report it privately through a GitHub security advisory on
[LLFourn/clank](https://github.com/LLFourn/clank/security/advisories/new),
not in a public issue. Include what you ran, what you saw and what you
expected. You will get an answer there.

## What is protected

- **The remote access token.** It opens a browser session that drives
  the TUI through the tunnel, so it is control of your agents.
- **GitHub credentials.** clank takes them from `GH_TOKEN`,
  `GITHUB_TOKEN` or one `gh auth token` call and stores none of them.
- **Your code and plans.**
- **The operator's authority over agents.** What an agent is told to do
  should come from you, not from whoever wrote a pull request title.

## Who it is protected from

1. **Someone on the network.** Every listener binds to `127.0.0.1`.
   The remote is off until you turn it on in the TUI, and the default
   tunnel is Cloudflare's, which terminates TLS. A tunnel you configure
   yourself carries what you point it at.
2. **Third parties who write GitHub text** — titles, bodies, comments,
   branch and author names — that reaches an agent's context. This is
   prompt injection. clank marks that text as someone else's wherever
   it shows it (below), but **no label or delimiter makes model input
   safe**: a model can still follow instructions it reads inside the
   mark. This is a residual risk. Watch only repositories whose
   writers you would let suggest work to your agents, and keep the
   operator prompt on a watch narrow.
3. **Accidental disclosure** — the token pasted into a transcript, a
   log, a diagnostic, a subprocess's argv or an exported page. The
   token is kept out of `~/.clank/config.json` and out of every one of
   those paths.
4. **Agents and other processes running as you: not defended.**
   Anything with your user id can read a mode-0600 file, query your
   login keychain, or run `clank` itself. The keychain and file
   permissions below guard against accidents, not against your own
   agents. If an agent must not have the remote token, it must not run
   as you.

## Where the remote token lives

- **macOS:** the login keychain, as the generic password
  `clank-remote` (account: your home directory), written and read
  through the Security framework API — never through a command line.
  A door started with `HOME` pointing anywhere but your account's own
  home (a test, a sandbox) uses the file below instead: the keychain is
  per account, and two homes sharing one item would share one token.
  The keychain may ask you to allow a newly built clank to read the
  item.
- **Elsewhere:** `~/.clank/remote-token`. Every write creates a new
  file with `O_CREAT|O_EXCL` and mode 0600 and renames it into place,
  so the token is never readable by others and is never chmodded after
  it is written. A token file anyone else can read is refused until
  you `chmod 600` it.

A token an older clank left in `~/.clank/config.json` under
`remote.token` is moved on first use, under the same lock that guards
rotation and sessions: written to the store, read back and compared,
and only then removed from the config. If the store cannot be written
or read, the config keeps its copy and the remote stays off; clank
never falls back to the config and never mints a replacement for a
token it could not read. If the config and the store hold different
tokens, the remote stays off and the error names both, until you
remove one.

The token is shown deliberately in two places, as before: the TUI's
remote page (its QR code and link) and an already signed-in browser
session. No command prints it.

## Exposures

Each is fixed, with the test that holds it, or accepted, with the
reason.

| Exposure | Status | Test or reason |
|---|---|---|
| The remote token was stored in plaintext in `~/.clank/config.json`, which agents read | Fixed | `the_config_token_moves_to_the_store_once_under_concurrency`, `an_equal_copy_in_the_config_is_removed`, `editing_remote_preserves_the_rest_of_user_config` |
| A failed or partial move of the token could lose it, run the remote from plaintext, or mint a replacement | Fixed | `a_failed_move_keeps_the_config_copy_and_refuses_the_remote`, `a_store_that_reads_back_wrong_keeps_the_config_copy`, `an_unreadable_store_mints_nothing_and_falls_back_to_nothing` |
| Two different tokens, one in the config and one in the store, could be resolved silently | Fixed | `a_conflict_is_refused_and_changes_neither_store` |
| One home spelled two ways (`/Users/x/.`, a symlink) could select two keychain items, so a rotation through one left the old token valid through the other | Fixed | `one_home_however_spelled_has_one_keychain_token` |
| Moving the token out of the config could drop other `remote` settings it did not know | Fixed | `migration_removes_only_the_token` |
| Moving the token could reopen a session a rotation revoked | Fixed | `a_revoked_session_is_not_readmitted_after_a_migration`, `a_revoked_session_is_handed_no_token`, `a_login_cannot_straddle_a_rotation` |
| The token file could be readable by others | Fixed | `the_token_file_is_created_0600_and_a_looser_one_is_refused_until_fixed`, `a_symlinked_token_file_is_not_followed` |
| The token could appear in a subprocess argv | Fixed | `the_keychain_gets_the_token_as_item_data_only` |
| The token could appear in an error a store reports | Fixed | `a_failed_move_keeps_the_config_copy_and_refuses_the_remote` |
| Remote sessions and links on disk could be replayed | Fixed | `a_session_is_hashed_on_disk_and_ends_when_revoked_or_old`, `a_shared_link_crosses_door_processes_and_is_spent_once` |
| A command source's diagnostic could carry a bearer token | Fixed | `secrets_are_masked_wherever_they_appear`, `a_quoted_value_is_masked_whole` |
| A GitHub title, author or body reaches an agent's wake, `clank events read`, or the TUI's event page as if the operator wrote it | Fixed | `a_malicious_github_title_stays_fenced_in_the_wake_text_and_json`, `external_wake_hints_render_their_payload`, `events_read_keeps_a_malicious_title_inside_its_fence`, `the_event_page_fences_a_malicious_title_and_body`, `a_body_cannot_close_the_fence_with_a_nonce_it_saw_before` |
| A model follows instructions inside the untrusted fence anyway | Accepted | Marking is not a boundary; no delimiter makes model input safe. Narrow watches and operator prompts are the mitigation. |
| `clank html` output could carry configuration, the environment or `~/.clank` stores | Fixed | `html_exports_no_config_environment_or_home_store_secrets` |
| `clank html` exports diffs, plans and feedback as they are, including a secret someone committed | Accepted | The page shows what git already shows to anyone with the repository; scrubbing would be a guess that hides real content. |
| Agents and processes running as the user can read the token, the keychain item and every file clank writes | Accepted | Same-user isolation is the operating system's job; clank cannot provide it to processes that share its user id. |
| A door run under a substituted `HOME` on macOS keeps its token in a 0600 file, not the keychain | Accepted | The keychain is per account; sharing one item across homes would hand one home's token to another. |
| Someone on the network reaches a listener | Accepted | Listeners bind to `127.0.0.1` only (`web-cloudflare/src/host.rs`), the remote is opt-in, and the default tunnel's TLS is Cloudflare's. |
| `hooks.blocked` runs through `sh -c` | Accepted | It comes only from untracked configuration you wrote; it runs what you would run. |
| GitHub credentials | Accepted | clank stores none; it reads `GH_TOKEN`, `GITHUB_TOKEN` or `gh auth token` each time. |

## What `clank html` reads

The exported site is built from git history and diffs, active and
finished plans, the queue and stash, review feedback, and the repo's
open questions with their agent labels. GitHub events are not part of
the site.

Private configuration is excluded from the html OUTPUT. Building the
site still builds the same status snapshot the TUI does, and that
reads the repo, user and agent configuration; the page renderer is
then handed only a fixed projection of it — repo name, branch, head,
the active plan's gate, and open questions — so configured sources,
agent settings, team resolution, configuration contents, environment
variables and the `~/.clank` stores (the remote token, sessions,
links, the user-command ledger) never reach a page. The projection is
an output boundary, not a guarantee that those stores are never read.
