# claude-launch-does-not-ask-to-trust-the-workspace-again
# Claude launch does not ask to trust the workspace again

## Problem and outcome

The user still sees Claude Code's workspace safety prompt when opening
`/Users/llfourn/src/sp-channel`: “Is this a project you created or one you trust?”
The earlier `a-fresh-clank-repo-does-not-ask-to-be-trusted` plan removed
Clank-created tool pre-approvals and explicitly left the workspace trust dialog
out of scope. This plan addresses that separate launch requirement: Claude
launched by Clank must enter the selected workspace without this interactive
trust question. The current pre-launch trust helper handles Codex only and
incorrectly comments that Claude has no such prompt.

Implement this plan in one implementation commit, without internal named
milestones.

## Investigation and contracts

- Reproduce the reported launch using the actual Claude executable, version,
  effective home/profile, working directory and Clank entry point. Inspect the
  earlier finished trust plan and distinguish its narrower permission-rule fix
  from the missing workspace trust preparation in this case.
  Distinguish the workspace trust gate from tool permission or authentication
  prompts; removing legacy tool allow rules is not proof this gate is handled.
- Research the current supported Claude Code mechanism for recording workspace
  trust or launching into an explicitly trusted workspace. Use official docs
  and actual installed behaviour. Apply the user's trust instruction to the
  concrete workspace being opened, rather than disabling unrelated checks.
- Ensure all real Clank Claude launch paths use the same preparation before
  spawning Claude: fresh starts, resumes, open, provider swaps and forked/linked
  worktrees. Resolve the actual canonical launch cwd, including any child
  worktree, and the effective Claude configuration home/profile. Do not prepare
  one path/profile and launch another.
- Integrate idempotent preparation with the existing automatic setup repair.
  Preserve unrelated Claude configuration and other workspace entries. Handle
  concurrent launches safely and report an actionable failure if required trust
  preparation cannot be persisted. Avoid fragile terminal key injection or an
  agent instruction to click through the safety prompt.
- Dry runs and previews must remain read-only. Normal operation must not require
  the user to run a separate setup command or supply an agent name merely to
  avoid the workspace question. No idle polling or new long-running helper.

## Validation and completion

Verify the concrete sp-channel launch and a previously unseen workspace with the
actual supported Claude harness. Cover fresh launch, resume, a linked worktree,
custom configuration home/profile, repeat preparation, concurrent preparation
and existing user settings. Check that previews make no changes and setup/launch
uses the same effective paths. Keep fixture verification distinct from real
interactive acceptance; demonstrate that the actual trust prompt is absent.

After review, reinstall Clank and any relevant setup assets, apply the correction
through the normal launch/setup path, and report the cause and verified launch
behaviour. Document the supported trust mechanism and remove obsolete guidance
that claims the prompt is already prevented without covering this case.

## Implementation and verification

The earlier migration explicitly excluded workspace trust. The current launcher
had only a Codex trust preparation, and Claude inherited the invoking shell's cwd
even with `--repo`. This implementation gives every composed Claude launch the
selected canonical repo as its actual cwd and uses the same effective HOME and
CLAUDE_CONFIG_DIR for integration repair, trust preparation and execution.
Relative profile paths are resolved there and passed to Claude as absolute paths;
an empty CLAUDE_CONFIG_DIR follows Claude's default-HOME behavior.

The supported trust entry is
`projects["<main checkout>"].hasTrustDialogAccepted = true`. Claude documents the
repository/main-worktree key and manual trust record here:
https://code.claude.com/docs/en/permissions#project-allow-rules-and-workspace-trust
For a normal profile the file is HOME/.claude.json; an explicit CLAUDE_CONFIG_DIR
puts it inside that directory, matching the installed Claude 2.1.280 executable
at ~/.local/share/claude/versions/2.1.280. There is no tool-permission bypass flag,
blanket allow rule, terminal key injection, or idle helper in this change.

Configuration updates preserve unrelated global/project fields, permissions and
symlinks. Clank launches serialize through the existing installation lock and
cooperate with Claude's native `<config>.lock` mkdir lease for the JSON transaction
(verified in 2.1.280's installed configuration writer). They re-read after locking
and atomically publish only the trust decision. This native lock protocol is an
installed-provider implementation dependency, unlike the documented trust field;
it must be checked if Claude changes its configuration storage protocol. Native
contention has bounded launch-time backoff, abandoned leases follow the native
10-second stale rule, and a replaced/expired lease cannot publish. An error keeps
existing configuration and fork intent intact and reports the selected profile.
Already trusted entries neither rewrite the file nor acquire/create locks, even
with read-only configuration. Preview returns before writes.

Automated validation passed: 8 trust tests, 71 setup tests, 72 agent tests, and
5 launch acceptance tests. Coverage includes concurrent projects and a simulated
native writer, stale/replaced locks, symlink and mode preservation, malformed and
read-only JSON, a real Git linked worktree's main-checkout key, custom/default/
empty/relative profile selection, launch from a foreign cwd, fresh/resumed/fork
preparation, retry after failure, and read-only previews. Formatting and diff
checks passed. Launch acceptance uses isolated provider executables; it does not
claim those fixtures prove Claude's interactive behavior.

Separately, real interactive PTY checks used the installed Claude 2.1.280 with
isolated profiles, preapproved dummy credentials, hooks disabled and the API
endpoint routed to a closed loopback port. No external model request or workspace
trust-dialog input was used. In a previously unseen Git repo Claude displayed the
actual safety dialog before preparation. Normal `clank agent start --repo` from a
different cwd reached the actual editor without that dialog afterward. A real
resume with the isolated trust entry reset to false and a real linked-worktree
launch also reached the editor without the dialog; the child shared the main key.
Captures are in /tmp/clank-claude-native-check/ (before.terminal,
after-editor.terminal, resume-after.terminal, child-after.terminal).

For the reported sp-channel cwd, the actual executable/profile default currently
already records trust. An isolated profile reproduced the user's exact safety
dialog there; adding the documented entry to that isolated profile reached the
editor without it (sp-before.terminal and sp-accepted.terminal). A normal duplicate
launch against the existing sp-channel roster was correctly refused because its
Claude pane is still live. That session, roster and user trust configuration were
left untouched; this is not a claim that its live master was restarted. Release
installation and the final installed-binary launch check follow the review gate.
