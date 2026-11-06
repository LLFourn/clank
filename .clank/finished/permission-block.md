# permission-block
# A permission block is resolved only by a nudge: the person's answer is typed into the agent's terminal as their own message

## Why

Agents get refused things they need. Claude Code's auto-mode classifier denied a team in clank-wr
`git push fork …` and `brew install lima`, and this session was refused a queue edit the same way.
The refusal is lifted by the person's own message in that agent's terminal: the classifier trusts
what the user typed, and nothing else. An ordinary block answer can't lift it, because it reaches
the agent as a hook's system reminder, not as the user's turn. So today LLFourn has to go to that
team's terminal and type the approval himself.

`clank nudge` already types text into an agent's pane. What's missing is a block that knows it
needs that, and an unblock that sends it.

## Build

### 1. A permission block

`clank block ask <name> --permission -m "<what was refused, and why it's needed>"`
[`--suggest <text>` | `--suggest-file <path>`].

- `--permission` marks the block's kind. The kind is stored with the block, and `clank status -j`
  reports it.
- `--suggest` is optional. It's the agent's guess at the message that would lift the refusal, for
  example "yes: push repro-ci-experiment to the fork". It's stored and shown, and never sent unless a
  person sends it.
- A block without `--permission` is unchanged.

### 2. A permission block can only be resolved by a nudge

`clank block answer <agent> <name> --nudge -m "<text>"` types the text into the agent's pane as the
person's message, through the same pane discovery, input path and nudge history as `clank nudge`. It
records the answer only when the nudge was submitted.

`-m` is required. The person writes or accepts the text; the suggestion is a pre-fill, never a
default. `--nudge` on an ordinary block means the same: answer it by typing the answer.

#### One resolution operation

All answerers go through one operation in `block.rs`, beside `answer_block` and sharing its
occurrence stamp, stale-question check and `blocks.lock`. Those answerers are:

- the CLI;
- the TUI (`status_tui/block_page.rs`);
- the browser (`web/repo_session.rs`).

The kind guard lives in that operation, not in argument dispatch. A plain answer to a permission
block is refused for every caller, the browser included, with an error that names `--nudge`.

A nudge is slow and can end in an unknown state, so the lock isn't held across it. An attempt record
serializes answerers instead:

1. **Claim.** Under the lock:
   - Check that the block is open and that the occurrence is the one the answerer saw (stale or
     re-asked → refuse, before anything is typed).
   - Check it isn't already answered, and that no attempt record exists for this occurrence.
   - Publish `<name>.attempt`, holding the occurrence, a fresh attempt id (a random token minted
     per claim), the text and the start time.

   A second answerer, concurrent or later, sees the record and is refused. This includes a plain
   answer to an ordinary block.
2. **Deliver.** Without the lock, run the nudge. `nudge.rs` distinguishes three outcomes:
   - `Refused` and `Empty`: definitely not typed.
   - `Interrupt` and `Submit`: indeterminate. `pane_input::confirm` can time out after the text
     was submitted.
   - Success: submitted.
3. **Settle.** Under the lock again:
   - **Not typed:** remove the attempt record. The block stays open and the reason is reported.
   - **Indeterminate:** keep the attempt record and add the diagnostic to it. The block stays
     visibly unresolved in `status`, the TUI and `status -j`, as "nudge outcome unknown: <reason>".
     Nothing is resent automatically, and nothing claims the text didn't arrive.
   - **Submitted:** if the question still carries the claimed occurrence, publish the answer in one
     write. It's stamped with that occurrence and with the disposition `nudged`. The wake and
     `owed_answers` paths treat a `nudged` answer as already received, so no answer is ever exposed
     as unread, and nothing marks it received afterwards. The attempt record is then removed. If
     the block was re-asked meanwhile, no answer is recorded. The new ask stays open, and the
     report says the text was delivered for the old question.

#### Ownership

The attempt filename is reused, so each settle is fenced to the attempt it claimed. Every
settlement, diagnostic update and removal re-reads `<name>.attempt` under the lock, and acts only
if it still holds the claimed occurrence and attempt id.

An older delivery that returns after a `--resend` or a re-ask replaced its claim owns nothing:

- it records no answer and removes no record;
- it only reports what happened to its own text, and adds that to the nudge history.

A `nudged` answer is stamped with its attempt id as well as its occurrence. That's how recovery
tells whose answer it is.

Obsolete records resolve without touching the current ask. A record whose occurrence isn't the
question's current one belongs to an older ask, so every reader ignores it and the next claim
removes it. Neither reopens or answers the current ask.

#### Recovery

When an attempt record survives but a `nudged` answer carrying its occurrence and attempt id
already exists, the work finished and only the cleanup was interrupted. The answer wins:

- the block is answered;
- every reader ignores the record, and the next write under the lock removes it;
- nothing is reopened, recorded twice or sent.

Any other attempt record that outlives its process means one of two things:

- the process was interrupted after claiming;
- the text was confirmed delivered and the answer write failed.

Either way it shows like the indeterminate case: unresolved, with the attempt's text and time. The
person settles it explicitly:

- **`clank block answer … --delivered`:** they saw the text arrive. The answer is recorded as
  `nudged` from the attempt's text, and nothing is typed.
- **`clank block answer … --nudge --resend -m …`:** they know it didn't arrive, or accept a
  duplicate. The attempt is replaced, and the claim, deliver and settle steps run again.

**Promise:** at most one automatic delivery per occurrence. A second delivery happens only on an
explicit `--resend`. The plan claims no crash-proof exactly-once delivery, because nothing here can
provide it.

### 3. TUI

- **Showing it:** a permission block is shown as one, with its suggestion if it has one.
- **Answering it:** the TUI's answer action on a permission block always nudges. Its input is
  pre-filled with the suggestion, and the person edits or confirms it.
- **Ordinary blocks:** answering them is unchanged.

### 4. Surface for clank-wr's site

`status -j` carries the kind and the suggestion on each block. The site resolves a block by running
the same `clank block answer … --nudge -m …`. How the site gets the person's authority to run it is
the site's business, not this plan's.

## Out of scope

- A signed `prompt` kind on `clank user-command` (the clank-wr draft `block-prompt`). The site can
  call the unblock command.
- Sending the suggestion automatically. A person always sends the text.

## Tests

Every test drives the nudge through a seam. No test spawns zellij or an agent, and no test needs a
real pane.

1. **Kind and suggestion:** `block ask --permission --suggest` stores both, and `status -j` reports
   them.
2. **Plain answers are refused:** a plain answer to a permission block is refused by the shared
   operation, called as the CLI, TUI and browser callers do. The block stays open.
3. **Submitted:** the answer is recorded once, `nudged`, in one write. A wake or `owed_answers` read
   observed at any point after that write presents no answer.
4. **Concurrent answerers:** two answerers race on one block. Exactly one reaches the seam, and the
   other is refused by the attempt record.
5. **Stale or re-asked occurrence:** refused before the seam is called. A re-ask while a nudge is in
   flight records no answer for the new ask.
6. **Definitely not sent:** the attempt record is removed, nothing is recorded, and the block stays
   open.
7. **Submit, then confirmation timeout (indeterminate):** the attempt record remains with its
   diagnostic, `status -j` shows the block unresolved, and nothing is resent.
8. **Confirmed delivery, then the answer write fails:** the attempt record remains, and
   `--delivered` settles it without calling the seam.
9. **`--resend`:** calls the seam again for an outstanding attempt. Without it, a new `--nudge` is
   refused.
10. **A resend completes before an older delivery returns:** the late settle records nothing,
    leaves the resend's answer and record alone, and only reports.
11. **Stale cleanup after a re-ask:** an old attempt's settle or cleanup after a re-ask doesn't touch
    the new ask, or a new claim on it.
12. **Interrupted between the answer write and removing the record:** the block reads as answered,
    and recovery removes the record without sending, reopening or recording again.
13. **Ordinary blocks:** `--nudge` answers and types. A plain answer is unchanged, except while an
    attempt is outstanding.

## Done when

- **Code:** the CLI and TUI resolve a permission block only by a nudge.
- **Docs:** the README and the `clank block` docs describe `--permission`, `--suggest` and `--nudge`.
- **Report to clank-wr:**
  - the `status -j` shape of a permission block;
  - the exact unblock command.

## As landed

- Answering a permission block from the CLI also needs `--override-permission-classifier`, at
  LLFourn's request. An agent's own commands go through the auto-mode classifier, which refuses a
  flag that announces it overrides the classifier, so an agent can't approve its own refusal or
  another agent's. It's a hint to the classifier, not authentication, and the README says so. The
  TUI is a person's keypress and passes it implicitly. The browser can't, so it keeps getting the
  plain-answer refusal.
- Stored state that can't be read (kind, disposition, attempt record, or a header cut short) is
  `Stored::Unreadable`. It refuses every answer and retry, stays visible in `status`, the TUI and
  `status -j` (`"unreadable"`), is never owed, and is never deleted by `clean`. It's repaired by
  hand.
- In `status -j`, every block carries `kind` (`plain`, `permission` or `unreadable`), `suggestion`,
  `nudge` (`null`, or the unknown outcome with its attempt) and `unreadable`.
- The unblock command for clank-wr's site:
  `clank block answer <agent> <name> --nudge --override-permission-classifier -m "<text>"`.
- Not marked yet: permission blocks in the HTML status page (`html.rs`), and an unreadable
  `.received` marker, which is still read as "not received" (that behaviour predates this plan).
