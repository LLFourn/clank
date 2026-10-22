//! How owed work is worded when it wakes an agent: one minimal line per
//! item, what discharges it, and the escalation once the same items
//! keep coming back. The monitor renders every presentation from here,
//! so the hook only frames text it is handed.

use std::path::{Path, PathBuf};

use crate::lifecycle::AgentLabel;
use clank_core::Role;

/// A wire discriminant, normalized AT THE PARSE BOUNDARY.
///
/// Holds the typed value when this build knows the tag, and always
/// the raw text so an unknown one can still be shown. Converting in
/// `Deserialize` rather than in a normalize step is what makes
/// "converted once" true: there is no path that produces a row with
/// the string un-parsed, including the tests' own `from_value`
/// (codex 9cdf153).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tag<T> {
    known: Option<T>,
    raw: String,
}

impl<T> Tag<T> {
    fn known(&self) -> Option<&T> {
        self.known.as_ref()
    }

    /// What to print when nothing typed matched.
    fn raw(&self) -> &str {
        &self.raw
    }
}

/// A discriminant this build may or may not know. `None` is the
/// compatibility fallback and nothing else.
pub(crate) trait FromTag: Sized {
    fn from_tag(s: &str) -> Option<Self>;
}

impl FromTag for clank_core::wait::WaitKind {
    fn from_tag(s: &str) -> Option<Self> {
        clank_core::wait::WaitKind::parse(s)
    }
}

impl FromTag for clank_core::vocab::WaitingReason {
    fn from_tag(s: &str) -> Option<Self> {
        clank_core::vocab::WaitingReason::parse(s)
    }
}

impl<'de, T: FromTag> serde::Deserialize<'de> for Tag<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(Tag {
            known: T::from_tag(&raw),
            raw,
        })
    }
}

/// One owed item in the shape `clank wait --json` gives it, which is
/// what every line of a wake is phrased from. `kind` is the
/// `#[serde(tag = "kind")]` discriminant `wait` emits; anything `wait`
/// adds is ignored, and anything absent stays `None`, so a future kind
/// or field never breaks the rendering.
#[derive(serde::Deserialize)]
pub(crate) struct WaitItem {
    #[serde(skip)]
    source_cwd: Option<std::sync::Arc<PathBuf>>,
    presentation: Option<clank_core::wait::EventPresentation>,
    pub(crate) kind: Option<Tag<clank_core::wait::WaitKind>>,
    pub(crate) plan: Option<String>,
    pub(crate) sha: Option<String>,
    finalized_at: Option<String>,
    next: Option<String>,
    pub(crate) reason: Option<Tag<clank_core::vocab::WaitingReason>>,
    prompt: Option<String>,
    name: Option<String>,
    id: Option<String>,
    priority: Option<u64>,
    agent: Option<String>,
    answer: Option<String>,
    carries: Option<clank_core::wait::AnswerRef>,
    // External wake payloads (external-wake-hints-carry-payload):
    // github_event / command_event fields, all optional like the rest.
    repo: Option<String>,
    event: Option<String>,
    detail: Option<String>,
    number: Option<u64>,
    title: Option<String>,
    actor: Option<String>,
    url: Option<String>,
    /// The watch's operator prompt (github-watch-prompts).
    instructions: Option<String>,
    /// A command event's payload
    /// (a-command-is-an-event-source-like-any-other). `event` and
    /// `title` above are shared with the github item.
    content: Option<String>,
    due_at: Option<u64>,
    recurrence: Option<clank_core::agent_config::TimerRecurrence>,
    failure: Option<bool>,
    requires_reply: Option<bool>,
    /// What `clank events ack` takes for this one.
    selector: Option<String>,
    // An inbox message's payload.
    handle: Option<String>,
    from: Option<String>,
    subject: Option<String>,
}

impl WaitItem {
    /// What makes this the same item on another delivery: an event's own
    /// durable key, or a workflow item's typed fields. Never its rendering.
    pub(crate) fn identity(&self) -> String {
        if let Some(p) = &self.presentation {
            return format!("event {}@{} {}", p.source, p.seq, p.ident);
        }
        let opt = |v: &Option<String>| v.clone().unwrap_or_default();
        let mut identity = format!(
            "{} plan={} sha={} next={} reason={} name={} handle={}",
            self.raw_kind(),
            opt(&self.plan),
            opt(&self.sha),
            opt(&self.next),
            self.reason.as_ref().map_or("", Tag::raw),
            opt(&self.name),
            opt(&self.handle),
        );
        if let Some(c) = &self.carries {
            identity.push_str(&format!(
                " occurrence={} question={} answer={}",
                c.occurrence.as_deref().unwrap_or(""),
                c.question_digest,
                c.answer_digest,
            ));
        }
        identity
    }

    /// The wake's kind, already typed by the parse. `None` for a tag
    /// this build has no arm for.
    fn wake_kind(&self) -> Option<clank_core::wait::WaitKind> {
        self.kind.as_ref()?.known().copied()
    }

    /// The wake's reason, already typed by the parse. `None` when
    /// absent OR unknown; `discharge_of` treats both as "no condition
    /// to state" rather than borrowing one.
    fn wake_reason(&self) -> Option<clank_core::vocab::WaitingReason> {
        self.reason.as_ref()?.known().copied()
    }

    /// The kind's raw text, for rendering one this build cannot type.
    fn raw_kind(&self) -> &str {
        self.kind.as_ref().map_or("?", Tag::raw)
    }

    /// The reason's raw text, shown verbatim so an unrecognised one
    /// is still legible rather than becoming `?`.
    fn raw_reason(&self) -> &str {
        self.reason.as_ref().map_or("?", Tag::raw)
    }
}

/// Each owed item as its wake line reads it. A command event's source
/// runs in `source_cwd`, which its receipt callback runs in too.
pub(crate) fn mirror(
    items: &[clank_core::wait::WaitItem],
    source_cwd: Option<&Path>,
) -> Vec<WaitItem> {
    let cwd = source_cwd.map(|cwd| std::sync::Arc::new(cwd.to_path_buf()));
    items
        .iter()
        .filter_map(|item| {
            let json = serde_json::to_value(crate::cli::wait::render_json(item)).ok()?;
            let mut item: WaitItem = serde_json::from_value(json).ok()?;
            if item.wake_kind() == Some(clank_core::wait::WaitKind::CommandEvent) {
                item.source_cwd = cwd.clone();
            }
            Some(item)
        })
        .collect()
}

// ── discharge conditions (wakes-state-their-discharge-condition) ──
//
// A wake says what to do. Without saying what STOPS it, an agent that
// believes it already acted has no way to tell it is looping — every
// wake looks like a fresh instruction. The condition is a pure
// function of the item, so no state is stored and level-triggering is
// untouched.

/// Why a wake will or will not fire again.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Discharge {
    /// Fires while a condition holds; the text says what ends it.
    Until(&'static str),
    /// Watcher delivery, not stop-hook wake delivery.
    ObserverOnly,
    /// This kind, with this reason, has no condition to state.
    ///
    /// A DECISION, not an omission — which is the difference from the
    /// `Option<Discharge>` this replaces. `None` there meant both
    /// "deliberately silent" and "nobody classified it", and the
    /// second is how `command_event`'s text went stale under a guard
    /// that was never called (the-turn-end-is-one-decision).
    Unconditioned,
}

/// Classify one wake: what ends it, and what the agent owes it.
/// `reason` matters for `master`, whose several `WaitingReason`s
/// discharge differently.
///
/// TOTAL, and ONE decision per kind: every kind lands on both answers in
/// the same arm, so adding a `WaitKind` fails to compile here rather
/// than falling into a catchall, and the wake's condition and the
/// escalation's duty cannot classify a kind differently. A catchall is
/// how `fix_commit_tag` went unclassified, and the hand-rolled
/// `wire_tag_of` stand-in for this exhaustiveness was never called
/// (the-turn-end-is-one-decision).
pub(crate) fn classify(
    kind: clank_core::wait::WaitKind,
    reason: Option<clank_core::vocab::WaitingReason>,
) -> (Discharge, Duty) {
    use Discharge::*;
    use clank_core::vocab::WaitingReason;
    use clank_core::wait::WaitKind as K;
    match kind {
        K::Master => match reason {
            // Says only what ENDS it. The `clank attending` recipe is
            // the hint's job (attending-suppresses-standing-wakes),
            // which fires only when a background task is actually
            // live — advertising it unconditionally here would tell an
            // agent with nothing running to record nothing.
            Some(WaitingReason::GateContinue) => {
                (Until("fires until a commit moves the gate"), Duty::Step)
            }
            Some(WaitingReason::AddressCommitChanges) => (
                Until("fires until you commit work addressing the feedback on that sha"),
                Duty::Step,
            ),
            Some(WaitingReason::ReadyToFinalize) => {
                (Until("fires until `clank finish` runs"), Duty::Step)
            }
            Some(WaitingReason::CommitPlanRevision) => {
                (Until("fires until the plan edit is committed"), Duty::Step)
            }
            // Master never carries these, and a borrowed condition
            // reads as authoritative — worse than none.
            Some(WaitingReason::SessionFinished | WaitingReason::CommitNeedsReview) | None => {
                (Unconditioned, Duty::Nothing)
            }
        },
        K::Reviewer => (
            Until("fires until you write feedback for that sha"),
            Duty::Step,
        ),
        K::FixCommitTag => (
            Until("fires until the commit's plan tag is corrected"),
            Duty::Step,
        ),
        K::PromoteFromQueue => (
            Until("fires until the item is promoted or removed from the queue"),
            Duty::Step,
        ),
        K::Message => (
            Until("fires until the message is acked — `clank events ack`"),
            Duty::Event,
        ),
        K::GithubEvent => (
            Until("fires until the event is acked — `clank events ack`"),
            Duty::Event,
        ),
        K::AdHocReview => (
            Until("fires until feedback exists for that sha"),
            Duty::Step,
        ),
        K::AdHocRevise => (Until("fires until you commit the revision"), Duty::Step),
        K::Unblocked => (
            Until("this wake delivers it; there is nothing to acknowledge"),
            Duty::Nothing,
        ),
        K::Blocked => (
            Until(
                "you CANNOT clear this — only the human can answer it. Do not act on it, and \
                 do not answer it yourself",
            ),
            Duty::HumanOnly,
        ),
        K::Idle => (
            Until("fires while there is nothing else to do"),
            Duty::Nothing,
        ),
        // Same discharge as a github event, because it is the same
        // WAL: a command source's event is durable, re-presented on
        // every arm, and finished by an ack
        // (a-command-is-an-event-source-like-any-other). It used to
        // say "change or remove the command source" — true when an
        // exiting producer WAS the event, and advice to break the
        // watch now that one isn't.
        K::CommandEvent | K::TimerEvent => (
            Until("fires until the event is acked — `clank events ack`"),
            Duty::Event,
        ),
        K::ForCommit | K::ForFinished | K::ForBlocked => (ObserverOnly, Duty::Nothing),
    }
}

/// What ends one wake: [`classify`]'s condition.
pub(crate) fn discharge_of(
    kind: clank_core::wait::WaitKind,
    reason: Option<clank_core::vocab::WaitingReason>,
) -> Discharge {
    classify(kind, reason).0
}

/// What an agent owes an item, for an escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Duty {
    /// An event: handled and acked by its selector, before the turn ends,
    /// whatever it waits on.
    Event,
    /// A workflow step, named by its condition.
    Step,
    /// Only the human can clear it.
    HumanOnly,
    /// Nothing the agent can do discharges it.
    Nothing,
}

impl WaitItem {
    /// What `clank events read` and `clank events ack` take for this
    /// item: its own selector, or its presentation's.
    fn event_selector(&self) -> Option<String> {
        self.selector
            .clone()
            .or_else(|| self.presentation.as_ref().map(|p| p.selector()))
    }

    /// Which item this is, by every identifying field it carries.
    fn identified(&self) -> String {
        let mut out = self.raw_kind().to_string();
        if let Some(next) = &self.next {
            out.push_str(&format!(" {next}"));
        }
        if let Some(plan) = &self.plan {
            out.push_str(&format!(" {plan}"));
        }
        if let Some(name) = &self.name {
            out.push_str(&format!(" {name}"));
        }
        if let Some(sha) = &self.sha {
            out.push_str(&format!(" @ {}", short_sha(sha)));
        }
        if let Some(selector) = self.event_selector() {
            out.push_str(&format!(" {selector}"));
        }
        out
    }
}

/// One escalation line: what discharges `item`, or nothing when nothing
/// the agent does would.
pub(crate) fn duty_line(item: &WaitItem) -> Option<String> {
    let kind = item.wake_kind()?;
    let (discharge, duty) = classify(kind, item.wake_reason());
    let what = item.identified();
    let line = match duty {
        Duty::Event => {
            let Some(s) = item.event_selector() else {
                let Discharge::Until(until) = discharge else {
                    return None;
                };
                return Some(format!("- {what}: {until}"));
            };
            let finish = if item.requires_reply.unwrap_or(false) {
                format!(
                    "read it (`clank events read {s}`), then `clank events ack {s} --reply <text>`. \
                     A reply is required, and waiting on anyone or anything does not waive it: \
                     say in the reply what you are waiting on, and follow up when it arrives"
                )
            } else {
                format!(
                    "read it (`clank events read {s}`), reply if its source asks for one \
                     (`clank events reply {s} -m <text>`), then `clank events ack {s}`. If it \
                     waits on anyone or anything, answer or ack it now saying so, and follow up \
                     when that arrives"
                )
            };
            format!("- {what}: you MUST handle this before ending your turn: {finish}.")
        }
        Duty::Step => {
            let Discharge::Until(until) = discharge else {
                return None;
            };
            let block = if item.plan.is_some() {
                " If this step genuinely depends on a person, record that with `clank block \
                 ask <name> -m \"<question>\"`: it pauses this plan's review work and \
                 discharges nothing else."
            } else {
                ""
            };
            format!("- {what}: {until}.{block}")
        }
        Duty::HumanOnly => format!(
            "- {what}: only the human can clear this. Do not act on it, and do not answer it \
             yourself; it does not excuse the other items."
        ),
        Duty::Nothing => return None,
    };
    Some(line)
}

/// The condition line for one item, or nothing.
///
/// `None` for `kind` is the ONE fail-soft: a tag from a newer binary
/// this build has no arm for renders without a condition rather than
/// breaking the read. Every other absence is now a `Discharge`
/// variant, so "unclassified" cannot be reached by forgetting
/// (the-turn-end-is-one-decision).
fn discharge_suffix(
    kind: Option<clank_core::wait::WaitKind>,
    reason: Option<clank_core::vocab::WaitingReason>,
) -> String {
    let Some(kind) = kind else {
        return String::new();
    };
    match discharge_of(kind, reason) {
        // NOT `↳` — that already marks a watch's standing
        // instructions (`github-watch-prompts`), and two meanings on
        // one glyph is how a reader stops trusting either.
        Discharge::Until(t) => format!("      · {t}\n"),
        Discharge::ObserverOnly | Discharge::Unconditioned => String::new(),
    }
}

/// What READS the feedback a wake is about, on the wake that is about
/// it. The skill has always said `clank feedback read`, and a reader
/// who does not remember it reaches for the files instead — which are
/// right there, and wrong in the same way every time the layout moves
/// (the-nudge-names-the-command).
///
/// Only wakes whose work IS the feedback on a sha, and only their own
/// sha.
fn read_hint(item: &WaitItem) -> String {
    use clank_core::vocab::WaitingReason as R;
    use clank_core::wait::WaitKind as K;
    // TYPED, like every other discriminant branch: a kind spelled
    // here and never emitted would silently stop naming the command
    // (codex bf22b5b).
    let about_feedback = match (item.wake_kind(), item.wake_reason()) {
        (Some(K::Master), Some(R::AddressCommitChanges | R::ReadyToFinalize)) => true,
        // The same need on a commit that belongs to no plan.
        (Some(K::AdHocRevise), _) => true,
        _ => false,
    };
    match (about_feedback, item.sha.as_deref()) {
        (true, Some(sha)) => format!(
            "      · read it: clank feedback read --commit {}\n",
            short_sha(sha)
        ),
        _ => String::new(),
    }
}

#[cfg(test)]
fn render_wait_items(items: &[WaitItem], label: &AgentLabel, role: Role) -> String {
    let refs: Vec<&WaitItem> = items.iter().collect();
    render_wait_items_ref(&refs, label, role)
}

/// An event's body, then what finishes it, for one line of a wake.
///
/// The body is the WHOLE message — it used to be capped at 300
/// characters for timers and arrived pre-flattened for commands, so a
/// second paragraph never reached the agent at all. A single-line body
/// keeps the shape this line has always had. A multi-line one is
/// indented under its item, so its later lines cannot read as the next
/// item, and the ack moves to a line of its own: appended to the body's
/// LAST line it read as part of the message rather than as the
/// instruction for answering it.
fn body_and_ack(content: Option<&str>, ack: &str) -> String {
    const INDENT: &str = "      ";
    let Some(content) = content.filter(|c| !c.is_empty()) else {
        return ack.to_string();
    };
    let body = crate::cli::status_tui::text::indented_body(content, INDENT);
    if body.contains('\n') && !ack.is_empty() {
        format!(": {body}\n{INDENT}{}", ack.trim_start())
    } else {
        format!(": {body}{ack}")
    }
}

/// The continuation text for `items`: one MINIMAL line per item —
/// who/verb + plan + 12-char sha (`wait-output-is-a-minimal-hint`). The
/// HOW (feedback-write form, verdicts, promote evaluation, unblock) lives
/// in the agent's skill doc, not re-taught per wake.
pub(crate) fn render_wait_items_ref(items: &[&WaitItem], label: &AgentLabel, role: Role) -> String {
    use clank_core::wait::WaitKind as K;
    let mut out = String::new();
    out.push_str(&format!(
        "Clank wait returned work for `{label}` ({role}). Items:\n",
        label = label.as_str(),
        role = role.as_str(),
    ));
    for item in items {
        let plan = item.plan.as_deref().unwrap_or("?");
        let full = item
            .sha
            .as_deref()
            .or(item.finalized_at.as_deref())
            .unwrap_or("");
        let short = short_sha(full);
        // The typed kind DISPATCHES; the raw tag is kept only to
        // render one this build has no arm for. Branching on the
        // string let a new core kind fall into the catchall and print
        // a generic `? @` hint, which is the stale-renderer failure
        // this milestone exists to remove (codex bf22b5b).
        let Some(kind) = item.wake_kind() else {
            out.push_str(&format!("  - {}: {plan} @ {short}\n", item.raw_kind()));
            out.push_str(&read_hint(item));
            continue;
        };
        // Minimal hints (`wait-output-is-a-minimal-hint`): one line
        // per item — WHO/verb + plan + short sha. The HOW (the
        // `feedback write` form, verdict meanings, promote
        // evaluation, unblock command) lives in the agent's skill
        // doc, not re-taught per wake. The 12-char sha is what the
        // agent composes `feedback write --commit` from; `clank
        // status` has the rest.
        match kind {
            K::Reviewer => out.push_str(&format!("  - reviewer: review {plan} @ {short}\n")),
            K::Master => {
                let next = item.next.as_deref().unwrap_or("?");
                let reason = item.raw_reason();
                out.push_str(&format!("  - master: {next} {plan} @ {short} ({reason})\n"));
            }
            K::Idle => {
                let prompt = item.prompt.as_deref().unwrap_or("");
                out.push_str(&format!("  - idle: {prompt}\n"));
            }
            K::AdHocReview => out.push_str(&format!("  - adhoc-review: {short}\n")),
            K::AdHocRevise => out.push_str(&format!("  - adhoc-revise: {short}\n")),
            K::Message => {
                let handle = item.handle.as_deref().unwrap_or("?");
                let from = crate::inbox::sender_words(item.from.as_deref(), item.repo.as_deref());
                let subject = item.subject.as_deref().unwrap_or("");
                // The SELECTOR, not the handle: the handle names the
                // message inside its recipient, and `clank events ack`
                // takes neither it nor nothing.
                let ack = item
                    .selector
                    .as_deref()
                    .map(|s| {
                        if item.requires_reply.unwrap_or(false) {
                            format!(" — reply required; ack `clank events ack {s} --reply <text>`")
                        } else {
                            format!(" — ack `clank events ack {s}`")
                        }
                    })
                    .unwrap_or_default();
                out.push_str(&format!("  - message: {handle} {from}: {subject}{ack}\n"));
            }
            K::PromoteFromQueue => {
                let name = item.name.as_deref().unwrap_or("?");
                let priority = item.priority.unwrap_or(0);
                out.push_str(&format!("  - promote: {name} (priority {priority:03})\n"));
            }
            K::Blocked => {
                let agent = item.agent.as_deref().unwrap_or("?");
                let name = item.name.as_deref().unwrap_or("?");
                let scope = item.plan.as_deref().unwrap_or("repo");
                out.push_str(&format!(
                    "  - blocked: {agent}/{name} on {scope} (awaiting human)\n"
                ));
            }
            K::Unblocked => {
                let name = item.name.as_deref().unwrap_or("?");
                // The answer is the action payload (the human's
                // instruction) — signal, not tutorial.
                let answer = item.answer.as_deref().unwrap_or("");
                out.push_str(&format!("  - unblocked: {name}: {answer}\n"));
            }
            // External wakes carry their payload — a bare `? @ ?`
            // fallback hint is unactionable
            // (external-wake-hints-carry-payload).
            K::GithubEvent => {
                let repo = item.repo.as_deref().unwrap_or("?");
                let event = item.event.as_deref().unwrap_or("?");
                let detail = item
                    .detail
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default();
                let num = item.number.map(|n| format!(" #{n}")).unwrap_or_default();
                let title = item
                    .title
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .map(|t| format!(" \"{t}\""))
                    .unwrap_or_default();
                let actor = item
                    .actor
                    .as_deref()
                    .map(|a| format!(" by {a}"))
                    .unwrap_or_default();
                let url = item
                    .url
                    .as_deref()
                    .map(|u| format!(" {u}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  - github: {event}{detail} {repo}{num}{title}{actor}{url}\n"
                ));
                // The watch's standing instructions, one sanitized
                // indented follow-on line (github-watch-prompts) —
                // same collapse as the wait's own hint surface.
                if let Some(i) = item
                    .instructions
                    .as_deref()
                    .filter(|i| !i.trim().is_empty())
                {
                    out.push_str(&format!("    ↳ {}\n", crate::cli::wait::one_line(i, 300)));
                }
            }
            K::TimerEvent => {
                let name = crate::cli::wait::one_line(item.name.as_deref().unwrap_or("?"), 80);
                let due = item
                    .due_at
                    .map(|at| at.to_string())
                    .unwrap_or_else(|| "?".into());
                let ack = item
                    .selector
                    .as_deref()
                    .map(|s| format!(" — ack `clank events ack {s}`"))
                    .unwrap_or_default();
                let recurrence = item
                    .recurrence
                    .as_ref()
                    .map(|r| format!(" ({})", crate::cli::events::recurrence_summary(r)))
                    .unwrap_or_default();
                let body = body_and_ack(item.content.as_deref(), &ack);
                out.push_str(&format!(
                    "  - timer: {name} due at {due}{recurrence}{body}\n"
                ));
            }
            K::CommandEvent => {
                let name = item.name.as_deref().unwrap_or("?");
                let event = item.event.as_deref().unwrap_or("?");
                // A source reporting itself broken is not news about
                // the world, and the remedy is not to act on it.
                let lead = if item.failure.unwrap_or(false) {
                    "command source BROKEN"
                } else {
                    "command"
                };
                // Named here, not left to a `clank events list`: the
                // line that says an event is owed should say how to
                // finish it.
                let ack = item
                    .selector
                    .as_deref()
                    .map(|s| {
                        if item.requires_reply.unwrap_or(false) {
                            format!(" — reply required; use `clank events ack {s} --reply <text>`")
                        } else {
                            format!(" — ack `clank events ack {s}`")
                        }
                    })
                    .unwrap_or_default();
                let body = body_and_ack(item.content.as_deref(), &ack);
                out.push_str(&format!("  - {lead}: {name} {event}{body}\n"));
            }
            // Its OWN variant, not a `master` reason — `work_for`
            // emits it directly. It rendered through the catchall
            // until now, which is exactly how a recurring wake ends up
            // with no explanation.
            K::FixCommitTag => out.push_str(&format!("  - fix-commit-tag: {plan} @ {short}\n")),
            // Watcher delivery: these reach an observer, not this
            // hint surface. Naming them keeps the match EXHAUSTIVE,
            // which is the point — a new core kind now fails to
            // compile here instead of printing `? @`.
            K::ForCommit | K::ForFinished | K::ForBlocked => {
                out.push_str(&format!("  - {}: {plan} @ {short}\n", kind.as_str()))
            }
        }
        out.push_str(&discharge_suffix(item.wake_kind(), item.wake_reason()));
        out.push_str(&read_hint(item));
    }
    out.push_str("\nAct on these items now.");
    out
}

/// 12 chars, matching wait's hint width: agents compose
/// `feedback write --commit <sha>` from this, and 12 hex chars
/// can't realistically be ambiguous (ruthless 201e498 concern 2).
fn short_sha(s: &str) -> &str {
    &s[..s.len().min(12)]
}

/// The event rows `items` present, as their delivery evidence records.
pub(crate) fn selected_presentations<'a>(
    items: impl IntoIterator<Item = &'a WaitItem>,
) -> Vec<clank_core::wait::EventPresentation> {
    items
        .into_iter()
        .filter(|item| {
            matches!(
                item.wake_kind(),
                Some(
                    clank_core::wait::WaitKind::Message
                        | clank_core::wait::WaitKind::GithubEvent
                        | clank_core::wait::WaitKind::CommandEvent
                        | clank_core::wait::WaitKind::TimerEvent
                )
            )
        })
        .filter_map(|item| item.presentation.clone())
        .collect()
}

/// Everything a presentation of `items` carries to the model.
pub(crate) fn carried<'a>(
    items: impl IntoIterator<Item = &'a WaitItem> + Clone,
) -> crate::cli::presentation::Carried {
    crate::cli::presentation::Carried {
        events: selected_presentations(items.clone()),
        answers: items
            .into_iter()
            .filter(|item| item.wake_kind() == Some(clank_core::wait::WaitKind::Unblocked))
            .filter_map(|item| item.carries.clone())
            .collect(),
    }
}

/// The command events in `items` whose source may be told they were
/// presented: a real event, known by id and selector, with the cwd its
/// source runs in.
pub(crate) fn receipt_candidates<'a>(
    items: impl IntoIterator<Item = &'a WaitItem>,
) -> impl Iterator<Item = crate::cli::wake_receipt::Candidate<'a>> {
    items.into_iter().filter_map(|item| {
        if item.wake_kind() != Some(clank_core::wait::WaitKind::CommandEvent)
            || item.failure == Some(true)
        {
            return None;
        }
        Some(crate::cli::wake_receipt::Candidate {
            source: item.name.as_deref()?,
            id: item.id.as_deref()?,
            selector: item.selector.as_deref()?,
            cwd: item.source_cwd.as_deref().map(|cwd| cwd.as_path()),
        })
    })
}

/// The heartbeat a parked hook wakes with at its deadline. It must not
/// read as work: an agent woken with no items and no explanation will
/// look for a reason and invent one.
pub(crate) const REARM_REASON: &str = "clank: no work pending — the wait reached its ceiling and \
is re-arming. This is a heartbeat, not a task. End your turn; the next hook parks a fresh wait.";

/// What an agent is told when no monitor can present its work.
pub(crate) const MONITOR_UNAVAILABLE: &str =
    "clank's monitor is unavailable; work is owed — run `clank status`.";

/// How to attend background work instead of ending a turn to poll it.
const ATTEND_RECIPE: &str = "a subagent by its name — `clank attend \"<name>\"` — and shell work \
by launching it through clank — `clank run --desc \"two words\" -- <command>` — which records \
what it is attending and then becomes the command, so clank knows exactly when the work ends";

/// The paragraph a wake adds while the Stop listed live background work:
/// the shape that produced a dozen identical wakes.
pub(crate) fn live_work_recipe(tasks: &clank_core::turn_policy::TaskSnapshot) -> Option<String> {
    let live: Vec<String> = tasks
        .running()
        .into_iter()
        .map(|t| match (&t.id, &t.description) {
            (Some(id), Some(desc)) => format!("{id} \"{desc}\""),
            (Some(id), None) => id.clone(),
            (None, Some(desc)) => format!("\"{desc}\""),
            (None, None) => "an unnamed task".into(),
        })
        .collect();
    (!live.is_empty()).then(|| {
        format!(
            "\n\nYou have live background work ({}). To wait on it without being woken again \
             for what you have been shown, attend it and end your turn: {ATTEND_RECIPE}. Either takes `--expect 30m` when the work will \
             outlast the five-minute default; past that you are checked in on once, and nothing \
             is cancelled. Do not end your turn to poll it.",
            live.join(", ")
        )
    })
}

/// A task as a resolution notice lists it.
fn task_line(t: &clank_core::turn_policy::TaskSummary) -> String {
    use clank_core::turn_policy::TaskKind;
    let kind = match &t.kind {
        TaskKind::Subagent => "subagent".to_string(),
        TaskKind::Shell => "shell".to_string(),
        TaskKind::Unknown(Some(k)) => k.clone(),
        TaskKind::Unknown(None) => "task".to_string(),
    };
    let mut line = format!("  - {kind}");
    if let Some(desc) = &t.description {
        line.push_str(&format!(" \"{desc}\""));
    }
    if let Some(id) = &t.id {
        line.push_str(&format!(" ({id})"));
    }
    line
}

/// What an attendance notice tells the agent, so it can attend the right
/// thing next time instead of being woken again.
pub(crate) fn notice_text(notice: &clank_core::turn_policy::Notice) -> String {
    use clank_core::turn_policy::Notice;
    let attend = |selector: &clank_core::turn_policy::Selector| {
        crate::cli::turn_custody::selector_words(selector)
    };
    match notice {
        Notice::NoMatch { selector, live } => {
            let listed = if live.is_empty() {
                "No background work is live.".to_string()
            } else {
                format!(
                    "Live now:\n{}",
                    live.iter().map(task_line).collect::<Vec<_>>().join("\n")
                )
            };
            format!(
                "clank: attending {} matched no live background work, so it is dropped and this \
                 turn-end was not quieted. {listed}\nAttend {ATTEND_RECIPE}.",
                attend(selector)
            )
        }
        Notice::Ambiguous { selector, matches } => format!(
            "clank: attending {} matched {} live tasks, so it attends none of them:\n{}\nAttend \
             one by a name only it has.",
            attend(selector),
            matches.len(),
            matches.iter().map(task_line).collect::<Vec<_>>().join("\n")
        ),
        Notice::ShellNeedsRun { task_id } => format!(
            "clank: `{task_id}` is shell work clank cannot prove is still running, so its \
             turn-ends are not quieted. Launch shell work through `clank run --desc \"two words\" \
             -- <command>`, or attend it with the `--pid` of its process."
        ),
        Notice::MonitorUnavailable => MONITOR_UNAVAILABLE.to_string(),
        Notice::ArmWait => ARM_WAIT.to_string(),
    }
}

/// Grok's native handoff: the background wait whose completion is the
/// agent's next wake.
pub(crate) const ARM_WAIT: &str = "Run `clank wait` as one background terminal task \
     (block_until_ms: 0, with no positive timeout), then finish the turn. Its completion wakes \
     you. Do not poll it or launch a second wait.";

/// The one check-in, written for an agent with no context. It must not
/// read as an error or as work, and every value in its recipe is filled
/// in, so it can be pasted. One line: it is typed into the agent's pane.
pub(crate) fn check_in_text(
    task_id: &str,
    desc: Option<&str>,
    kind: &clank_core::turn_policy::TaskKind,
    pid: Option<i32>,
    elapsed_secs: u64,
    expect_secs: u64,
) -> String {
    use crate::shell_quote::shell_quote;
    let subject = desc
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or(task_id);
    let again = crate::cli::attend::duration_arg(expect_secs.saturating_mul(2));
    let recipe = match (kind, pid) {
        (clank_core::turn_policy::TaskKind::Shell, Some(pid)) => format!(
            "clank attend {} --desc {} --pid {pid} --expect {again}",
            shell_quote(task_id),
            shell_quote(subject)
        ),
        _ => format!("clank attend {} --expect {again}", shell_quote(subject)),
    };
    format!(
        "clank check-in: \"{subject}\" has been running for {} — you expected {}. It is still \
         running; nothing has been cancelled and nothing has failed. This is a check-in, not a \
         task: look at its output so far, and if it just needs longer run `{recipe}` and end your \
         turn; otherwise end your turn — its completion still wakes you.",
        crate::age::short(elapsed_secs as i64),
        crate::cli::attend::duration_arg(expect_secs),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `wait --json`'s envelope, as the rendering reads it: each item,
    /// and a command event's source cwd.
    fn parse_wait_json(raw: &[u8]) -> Result<Vec<WaitItem>, String> {
        #[derive(serde::Deserialize)]
        struct Envelope {
            items: Vec<WaitItem>,
            #[serde(default)]
            source_cwd: Option<PathBuf>,
        }
        let mut envelope: Envelope =
            serde_json::from_slice(raw).map_err(|e| format!("not valid JSON: {e}"))?;
        if let Some(cwd) = envelope.source_cwd {
            let cwd = std::sync::Arc::new(cwd);
            for item in &mut envelope.items {
                if item.wake_kind() == Some(clank_core::wait::WaitKind::CommandEvent) {
                    item.source_cwd = Some(cwd.clone());
                }
            }
        }
        Ok(envelope.items)
    }

    /// The live-work paragraph names what is running and both ways to
    /// attend it, and appears only while a listing shows work running.
    #[test]
    fn live_work_carries_the_attend_and_run_recipes() {
        use clank_core::turn_policy::TaskSnapshot;
        let listed: Vec<clank_core::hook_io::BackgroundTask> = vec![
            serde_json::from_value(serde_json::json!({
                "id": "task-42", "type": "subagent", "status": "running",
                "description": "sleeper"
            }))
            .unwrap(),
        ];
        let recipe = live_work_recipe(&TaskSnapshot::Known(listed)).expect("live work");
        assert!(recipe.contains("task-42"), "names the live task: {recipe}");
        assert!(recipe.contains("`clank attend \"<name>\"`"), "{recipe}");
        assert!(recipe.contains("`clank run --desc"), "{recipe}");
        assert!(!recipe.contains("clank attending"), "{recipe}");
        assert_eq!(live_work_recipe(&TaskSnapshot::Known(Vec::new())), None);
        assert_eq!(live_work_recipe(&TaskSnapshot::Unavailable), None);
    }

    /// The check-in's recipe is one shell-safe line however the
    /// description or id reads: user text must arrive as the same
    /// letters, not run. Shell work re-attends by its id and process; a
    /// subagent by its name.
    #[test]
    fn the_check_in_recipe_is_shell_safe_and_names_its_own_kind_of_work() {
        use clank_core::turn_policy::TaskKind;
        let shell = check_in_text(
            "b$1",
            Some("say $(rm -rf /) `id` \"hi\" it's"),
            &TaskKind::Shell,
            Some(4242),
            720,
            300,
        );
        assert!(
            shell.contains(
                "clank attend 'b$1' --desc 'say $(rm -rf /) `id` \"hi\" it'\\''s' --pid 4242 \
                 --expect 10m"
            ),
            "{shell}"
        );
        assert!(!shell.contains('\n'), "it is typed into a pane: one line");
        let subagent = check_in_text(
            "a25eee2",
            Some("sleeper"),
            &TaskKind::Subagent,
            None,
            720,
            300,
        );
        assert!(
            subagent.contains("clank attend sleeper --expect 10m"),
            "{subagent}"
        );
        for text in [&shell, &subagent] {
            assert!(text.contains("check-in, not a"), "{text}");
            assert!(text.contains("12m"), "elapsed: {text}");
        }
    }

    /// The heartbeat a parked hook wakes with at its deadline cannot be
    /// mistaken for work: an agent woken with no items and no explanation
    /// will look for a reason and usually invent one.
    #[test]
    fn the_re_arm_reason_cannot_be_mistaken_for_work() {
        let r = REARM_REASON.to_lowercase();
        assert!(r.contains("no work"), "must say nothing is pending");
        assert!(r.contains("re-arm"), "must name itself a re-arm");
        assert!(r.contains("end your turn"), "and the correct response");
    }

    /// Each resolution notice says what it matched, lists the live work,
    /// and names how to attend it next time.
    #[test]
    fn a_resolution_notice_lists_the_live_work_and_the_recipe() {
        use clank_core::turn_policy::{Notice, Selector, TaskKind, TaskSummary};
        let live = vec![TaskSummary {
            id: Some("a25eee2".into()),
            kind: TaskKind::Subagent,
            description: Some("sleeper".into()),
        }];
        let none = notice_text(&Notice::NoMatch {
            selector: Selector::Name("sleper".into()),
            live: live.clone(),
        });
        assert!(none.contains("\"sleper\" matched no live"), "{none}");
        assert!(none.contains("subagent \"sleeper\" (a25eee2)"), "{none}");
        assert!(none.contains("`clank attend"), "{none}");
        let two = notice_text(&Notice::Ambiguous {
            selector: Selector::RunDesc("build".into()),
            matches: vec![live[0].clone(), live[0].clone()],
        });
        assert!(two.contains("matched 2 live tasks"), "{two}");
        let shell = notice_text(&Notice::ShellNeedsRun {
            task_id: "b1".into(),
        });
        assert!(shell.contains("`clank run --desc"), "{shell}");
        assert!(shell.contains("--pid"), "{shell}");
        assert_eq!(
            notice_text(&Notice::MonitorUnavailable),
            MONITOR_UNAVAILABLE
        );
    }

    /// Every kind's discharge says the right KIND of thing, and the
    /// text is what is checked.
    ///
    /// There is no "is it classified?" case any more: `discharge_of`
    /// is total over `WaitKind`, so a new variant fails to COMPILE
    /// and a runtime assertion would be claiming a compile-time
    /// property. What the old test could not see is exactly what this
    /// does — `Until(_)` passed for three commits while
    /// `command_event` told agents to break their own watch
    /// (the-turn-end-is-one-decision).
    #[test]
    fn a_discharge_says_what_actually_ends_it() {
        use Discharge::*;
        use clank_core::vocab::WaitingReason as R;
        use clank_core::wait::WaitKind as K;

        for r in R::ALL {
            let d = discharge_of(K::Master, Some(*r));
            match r {
                // The four master actually carries each name a
                // different ending.
                R::GateContinue
                | R::AddressCommitChanges
                | R::ReadyToFinalize
                | R::CommitPlanRevision => {
                    assert!(matches!(d, Until(_)), "master/{r:?}: {d:?}");
                }
                // The two it never carries state nothing rather than
                // borrowing a neighbour's condition.
                R::SessionFinished | R::CommitNeedsReview => {
                    assert_eq!(d, Unconditioned, "master/{r:?}");
                }
            }
        }
        assert_eq!(discharge_of(K::Master, None), Unconditioned);

        // An event owed until an ACK names the command that ends it.
        for k in [K::GithubEvent, K::CommandEvent] {
            let Until(t) = discharge_of(k, None) else {
                panic!("{k:?} is owed until acked")
            };
            assert!(
                t.contains("acked") && t.contains("clank events ack"),
                "{k:?} must name the ack: {t}"
            );
        }
        // The one wake where ACTING is wrong, and where an agent is
        // most likely to invent an action to stop the nagging.
        let Until(t) = discharge_of(K::Blocked, None) else {
            panic!("blocked carries guidance")
        };
        assert!(t.contains("CANNOT") && t.contains("human"), "{t}");
        assert!(t.contains("not answer it yourself"), "{t}");

        for k in [K::ForCommit, K::ForFinished, K::ForBlocked] {
            assert_eq!(discharge_of(k, None), ObserverOnly, "{k:?}");
        }
    }

    /// A kind this build has no arm for renders WITHOUT a condition.
    ///
    /// The one fail-soft, and it must not become an excuse: it is
    /// reachable only through the parse, never by forgetting an arm.
    #[test]
    fn an_unknown_kind_states_no_condition() {
        assert_eq!(discharge_suffix(None, None), "");
        assert_eq!(clank_core::wait::WaitKind::parse("some_future_kind"), None);
    }

    #[test]
    fn master_keys_on_the_reason_not_the_kind() {
        use clank_core::vocab::WaitingReason as R;
        use clank_core::wait::WaitKind as K;
        // Four reasons, four different discharges. Keying on the kind
        // alone would print a condition that does not apply — worse
        // than silence, because it reads as authoritative.
        let cont = discharge_of(K::Master, Some(R::GateContinue));
        let revise = discharge_of(K::Master, Some(R::AddressCommitChanges));
        assert_ne!(cont, revise);
        assert!(
            matches!(cont, Discharge::Until(t) if t.contains("gate")),
            "continue is discharged by moving the GATE: {cont:?}"
        );
        assert!(
            matches!(revise, Discharge::Until(t) if t.contains("feedback")),
            "revise is discharged by addressing the FEEDBACK: {revise:?}"
        );
        // A reason this build cannot read is not given a borrowed
        // condition — and it cannot even be SPELLED as a string here
        // any more, which is the point of the type.
        assert_eq!(R::parse("some_new_reason"), None);
    }

    /// The wake that sends a master to feedback names what reads it,
    /// on that wake's own sha. Nothing else does: a reviewer WRITES
    /// feedback, and a continue has none to read
    /// (the-nudge-names-the-command).
    #[test]
    fn the_wakes_about_feedback_name_what_reads_it() {
        let render = |item: serde_json::Value| {
            let items: Vec<WaitItem> = serde_json::from_value(serde_json::json!([item])).unwrap();
            let refs: Vec<&WaitItem> = items.iter().collect();
            render_wait_items_ref(&refs, &AgentLabel::parse("claude").unwrap(), Role::Master)
        };
        let reads = format!("clank feedback read --commit {}", &FULL_SHA[..12]);

        for about_feedback in [
            serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
              "next":"revise","reason":"address_commit_changes","gate":"changes_requested"}),
            serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
              "next":"finalize","reason":"ready_to_finalize","gate":"finished"}),
            serde_json::json!({"kind":"adhoc_revise","sha":FULL_SHA}),
        ] {
            let out = render(about_feedback.clone());
            assert!(out.contains(&reads), "{about_feedback}: {out}");
        }

        for not_about_feedback in [
            serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
              "next":"continue","reason":"gate_continue","gate":"continued"}),
            serde_json::json!({"kind":"reviewer","plan":"p","sha":FULL_SHA}),
        ] {
            let out = render(not_about_feedback.clone());
            assert!(
                !out.contains("feedback read"),
                "{not_about_feedback}: {out}"
            );
        }
    }

    /// An unknown KIND and an unknown REASON both survive the real
    /// parse-and-render path, and neither invents a condition.
    ///
    /// Built from the PRODUCTION SERIALIZER: a core `WaitItem` goes
    /// through `cli::wait::render_json`, and the two future-tag cases
    /// alter only the one field under test. A hand-written `json!`
    /// stays green while the producer's shape drifts away from it —
    /// the same fixture mistake that let `exit_code`/`output_tail`
    /// outlive the fields they named (codex 9cdf153).
    #[test]
    fn an_unknown_kind_or_reason_renders_loose_and_claims_nothing() {
        use clank_core::vocab::WaitingReason;
        use clank_core::wait::MasterNext;

        let label = AgentLabel::parse("codex").unwrap();
        // The CONTROL: a real wake, serialized the way `clank wait`
        // serializes it.
        let real = serde_json::to_value(crate::cli::wait::render_json(
            &clank_core::wait::WaitItem::Master {
                plan: crate::lifecycle::PlanKey::parse("p").unwrap(),
                sha: crate::lifecycle::CommitSha::parse(FULL_SHA).unwrap(),
                next: MasterNext::Commit,
                reason: WaitingReason::GateContinue,
                gate: clank_core::vocab::CommitGateState::Continued,
            },
        ))
        .expect("the production serializer");

        let render = |item: serde_json::Value| {
            let env = serde_json::json!({ "items": [item] });
            let items = parse_wait_json(env.to_string().as_bytes()).expect("fail-soft parse");
            render_wait_items(&items, &label, Role::Reviewer)
        };
        let with = |field: &str, value: &str| {
            let mut v = real.clone();
            v[field] = serde_json::Value::String(value.to_string());
            v
        };

        // A known wake states its condition — the control, without
        // which the two cases below would pass against a renderer
        // that had simply stopped stating any.
        let out = render(real.clone());
        assert!(out.contains("fires until a commit moves the gate"), "{out}");

        // A KIND from a newer binary: rendered by its raw tag, no
        // condition claimed.
        let out = render(with("kind", "some_future_kind"));
        assert!(out.contains("some_future_kind: p @"), "{out}");
        assert!(!out.contains("fires until"), "no borrowed condition: {out}");

        // A REASON from a newer binary, on a kind this build knows:
        // the item still renders, the raw reason is still legible,
        // and still no condition.
        let out = render(with("reason", "some_new_reason"));
        assert!(out.contains("master:"), "the item renders: {out}");
        assert!(
            out.contains("some_new_reason"),
            "raw reason stays visible: {out}"
        );
        assert!(!out.contains("fires until"), "no borrowed condition: {out}");
    }

    #[test]
    fn the_rendered_wake_carries_the_condition_and_still_carries_the_sha() {
        let items: Vec<WaitItem> = serde_json::from_value(serde_json::json!([
            {"kind":"master","plan":"p","sha":FULL_SHA,
             "next":"continue","reason":"gate_continue","gate":"continued"}
        ]))
        .unwrap();
        let refs: Vec<&WaitItem> = items.iter().collect();
        let out = render_wait_items_ref(&refs, &AgentLabel::parse("claude").unwrap(), Role::Master);
        assert!(out.contains("gate_continue"), "{out}");
        assert!(
            out.contains("fires until"),
            "the wake says what stops it: {out}"
        );
        assert!(
            out.contains(&FULL_SHA[..12]),
            "the 12-char sha stays — it is what `feedback write --commit` is built from: {out}"
        );
    }

    /// A source reporting ITSELF broken does not read as news about
    /// the world.
    ///
    /// The agent's remedy is different — the watch is not working, so
    /// there is nothing out there to act on — and an item that reads
    /// like an ordinary event invites acting on it.
    #[test]
    fn a_broken_source_does_not_read_as_something_it_observed() {
        let wire = |failure| {
            serde_json::json!({
                           "items": [serde_json::to_value(crate::cli::wait::render_json(
                               &clank_core::wait::WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
                                   name: "deploys".into(),
                                   id: "x".into(),
                                   event: "e".into(),
                                   content: "`sh -c ...` has stopped 3 times in a row".into(),
                                   at: None,
                                   failure,
                                   requires_reply: false,
                                   reply_route: None,
                                   selector: Some("command-deploys-0000abcd@2".into()),
                               },
                           ))
                           .unwrap()]
                       })
        };
        let label = AgentLabel::parse("codex").unwrap();
        let render = |failure| {
            let items = parse_wait_json(wire(failure).to_string().as_bytes()).unwrap();
            render_wait_items(&items, &label, Role::Reviewer)
        };
        let broken = render(true);
        assert!(
            broken.contains("command source BROKEN: deploys"),
            "{broken}"
        );
        assert!(
            !render(false).contains("BROKEN"),
            "an ordinary event is not a breakage"
        );
        // Still ackable: a report the agent cannot finish wakes it
        // forever.
        assert!(
            broken.contains("clank events ack command-deploys-0000abcd@2"),
            "{broken}",
        );
    }

    #[test]
    fn a_required_reply_wake_does_not_instruct_a_plain_ack() {
        let item: clank_core::wait::WaitItem = serde_json::from_value(serde_json::json!({
            "kind": "command_event", "name": "questions", "id": "utt/a",
            "event": "question", "content": "Which option?",
            "requires_reply": true, "selector": "command-questions-abcd@7"
        }))
        .unwrap();
        let wire = serde_json::json!({"items": [crate::cli::wait::render_json(&item)]});
        let items = parse_wait_json(wire.to_string().as_bytes()).unwrap();
        let reason = render_wait_items(&items, &AgentLabel::parse("codex").unwrap(), Role::Master);
        assert!(reason.contains("reply required"), "{reason}");
        assert!(reason.contains("command-questions-abcd@7"), "{reason}");
        assert!(!reason.contains("— ack `clank events ack"), "{reason}");
    }

    #[test]
    fn a_reply_required_message_hint_carries_the_same_policy_as_its_wait_wire() {
        let item: clank_core::wait::WaitItem = serde_json::from_value(serde_json::json!({
            "kind":"message", "handle":"request", "subject":"Please answer", "body":"Full request",
            "requires_reply":true, "reply_route":{"repo":"/origin","agent":"coordinator"}, "selector":"messages@7"
        })).unwrap();
        let wire = serde_json::json!({"items":[crate::cli::wait::render_json(&item)]});
        let parsed = parse_wait_json(wire.to_string().as_bytes()).unwrap();
        let rendered =
            render_wait_items(&parsed, &AgentLabel::parse("codex").unwrap(), Role::Master);
        assert!(
            rendered.contains("reply required; ack `clank events ack messages@7 --reply <text>`"),
            "{rendered}"
        );
        assert!(!rendered.contains("— ack `clank events ack messages@7`"));
    }

    #[test]
    fn recurring_timer_hint_preserves_snapshot_and_plain_ack_recipe() {
        use crate::cli::{
            github::log::{EventLog, RecurringTransition},
            timer_source::recurrence::Recurrence,
        };
        let dir = tempfile::tempdir().unwrap();
        let r = Recurrence::from_generation(clank_core::agent_config::TimerGeneration {
            name: "deploy\nextra".into(),
            scope: None,
            anchor: 100,
            interval: 10,
        })
        .unwrap();
        let RecurringTransition::Accepted { seq, mut item } =
            EventLog::open(dir.path(), &r.source_key())
                .unwrap()
                .accept_recurring(&r, "check\nnow\u{1b}", 140)
                .unwrap()
        else {
            panic!("not accepted")
        };
        crate::cli::command_source::stamp_selector(&mut item, &r.source_key(), seq);
        let wire = serde_json::json!({"items":[crate::cli::wait::render_json(&item)]});
        let parsed = parse_wait_json(&serde_json::to_vec(&wire).unwrap()).unwrap();
        let hint = render_wait_items(&parsed, &AgentLabel::parse("codex").unwrap(), Role::Master);
        assert!(hint.contains("5 scheduled occurrences"), "{hint}");
        assert!(hint.contains("1970-01-01T00:01:40Z..1970-01-01T00:02:20Z"));
        assert!(hint.contains("every 10s"));
        assert!(hint.contains(&format!("clank events ack {}@{seq}", r.source_key())));
        assert!(!hint.contains('\u{1b}'));
        assert_eq!(wire["items"][0]["content"], "check\nnow\u{1b}");
    }

    #[test]
    fn timer_hint_uses_the_actual_wait_wire_and_retains_ack_recipe() {
        let item = clank_core::wait::WaitItem::TimerEvent {
            presentation: None,
            name: "deploy".into(),
            recurrence: None,
            id: "timer/1".into(),
            due_at: 1,
            content: "check\nnow\u{1b}".into(),
            selector: Some("timer-config-deploy-abcd@3".into()),
        };
        let wire = serde_json::json!({"items":[crate::cli::wait::render_json(&item)]});
        assert_eq!(wire["items"][0]["kind"], "timer_event");
        assert_eq!(
            wire["items"][0]["content"], "check\nnow\u{1b}",
            "wire/WAL preserve original content"
        );
        let items = parse_wait_json(wire.to_string().as_bytes()).unwrap();
        let hint = render_wait_items(&items, &AgentLabel::parse("codex").unwrap(), Role::Master);
        // The body arrives WHOLE, its second line indented under its
        // item so it cannot read as the next one, and the ack on a line
        // of its own rather than at the end of the message. This used to
        // assert `check now` — the agent was told something the timer
        // never said.
        assert!(
            hint.contains(
                "  - timer: deploy due at 1: check\n      now\n      \
                 — ack `clank events ack timer-config-deploy-abcd@3`\n"
            ),
            "{hint}"
        );
        assert!(!hint.contains('\u{1b}'));
        assert!(hint.contains("fires until the event is acked"));
    }

    /// A command event wakes its agent with the WHOLE message, through
    /// the same JSON wire the hook consumes from `clank wait --json`.
    ///
    /// Before this the wake carried the content raw, which was only safe
    /// because ingestion had already cut it to one line. A second line
    /// now arrives indented under its item — so it cannot be read as the
    /// next item — and the reply recipe sits on its own line instead of at
    /// the end of the message, where it read as something the producer
    /// had said.
    #[test]
    fn a_command_wake_carries_the_whole_body_and_its_reply_recipe() {
        let body = "Para one.\n\n  indented\n\tTabbed\nüñí ✓";
        let item = clank_core::wait::WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "signal".into(),
            id: "m1".into(),
            event: "message".into(),
            content: body.into(),
            at: None,
            failure: false,
            requires_reply: true,
            reply_route: None,
            selector: Some("command-signal-0@1".into()),
        };
        let wire = serde_json::json!({"items": [crate::cli::wait::render_json(&item)]});
        let items = parse_wait_json(wire.to_string().as_bytes()).unwrap();
        let hint = render_wait_items(&items, &AgentLabel::parse("codex").unwrap(), Role::Master);

        let at = hint.find("  - command: signal message: ").expect(&hint);
        let mut lines = hint[at + "  - command: signal message: ".len()..].split('\n');
        let mut got = vec![lines.next().unwrap().to_string()];
        let mut rest: Vec<&str> = lines.collect();
        // Everything indented under the item is body, then the recipe.
        let recipe = rest
            .iter()
            .position(|l| l.starts_with("      — reply required"))
            .expect(&hint);
        got.extend(
            rest.drain(..recipe)
                .map(|l| l.strip_prefix("      ").unwrap_or(l).to_string()),
        );
        assert_eq!(got.join("\n"), body, "{hint}");
        assert!(
            rest[0].contains("clank events ack command-signal-0@1 --reply"),
            "{hint}"
        );

        // A single-line body keeps exactly the shape it always had.
        let mut one = item.clone();
        if let clank_core::wait::WaitItem::CommandEvent { content, .. } = &mut one {
            *content = "just this".into();
        }
        let wire = serde_json::json!({"items": [crate::cli::wait::render_json(&one)]});
        let items = parse_wait_json(wire.to_string().as_bytes()).unwrap();
        let hint = render_wait_items(&items, &AgentLabel::parse("codex").unwrap(), Role::Master);
        assert!(
            hint.contains(
                "  - command: signal message: just this — reply required; use \
                 `clank events ack command-signal-0@1 --reply <text>`\n"
            ),
            "{hint}"
        );
    }

    #[test]
    fn external_wake_hints_render_their_payload() {
        // external-wake-hints-carry-payload: the codex in-hook hints
        // for the external kinds carry the payload instead of the
        // useless `? @ ?` fallback. Built from wait's ACTUAL --json
        // field names (the same wire the hook consumes).
        let raw = serde_json::json!({
                   "items": [
                       {
                           "kind": "github_event",
                           "repo": "o/r",
                           "event": "pr_comment",
                           "detail": "review",
                           "number": 12,
                           "title": "Add the widget",
                           "actor": "hubot",
                           "url": "https://github.com/o/r/pull/12"
                       },
                       // The REAL wire, serialized from a real item: the
                       // last fixture here was hand-written and still
                       // carried `exit_code`/`output_tail` long after the
                       // wake stopped having them
                       // (a-command-is-an-event-source-like-any-other).
                       serde_json::to_value(crate::cli::wait::render_json(
                           &clank_core::wait::WaitItem::CommandEvent {
        presentation: None,
        source_cwd: None,
                               name: "signal".into(),
                               id: "sha-abc".into(),
                               event: "deployed".into(),
                               content: "prod is live".into(),
                               at: None,
                               failure: false,
                               requires_reply: false,
                               reply_route: None,
                               selector: Some("command-signal-0000abcd@7".into()),
                           },
                       ))
                       .unwrap(),
                       { "kind": "some_future_kind", "plan": "p", "sha": "abcdef123456789" }
                   ]
               });
        let items = parse_wait_json(raw.to_string().as_bytes()).unwrap();
        let label = AgentLabel::parse("codex").unwrap();
        let out = render_wait_items(&items, &label, Role::Reviewer);
        assert!(
            out.contains(
                "  - github: pr_comment (review) o/r #12 \"Add the widget\" by hubot \
                 https://github.com/o/r/pull/12"
            ),
            "github hint carries the payload: {out}"
        );
        assert!(
            out.contains(
                "  - command: signal deployed: prod is live — ack \
                 `clank events ack command-signal-0000abcd@7`"
            ),
            "the command hint carries the payload AND how to finish it: {out}"
        );
        // Unknown future kinds still fall through loose (fail-soft).
        assert!(
            out.contains("  - some_future_kind: p @ abcdef123456"),
            "unknown kind falls through: {out}"
        );
        // Promptless: no follow-on line anywhere.
        assert!(!out.contains('↳'), "no instructions line: {out}");
        // A prompted watch's instructions render as the sanitized
        // indented follow-on (github-watch-prompts) on THIS surface
        // too — the codex/opencode hook is the real wake wire.
        let raw = serde_json::json!({
            "items": [{
                "kind": "github_event",
                "repo": "o/r",
                "event": "issue_opened",
                "instructions": "Label it,\nthen reply.\tThen ack."
            }]
        });
        let items = parse_wait_json(raw.to_string().as_bytes()).unwrap();
        let out = render_wait_items(&items, &label, Role::Reviewer);
        assert!(
            out.contains("  - github: issue_opened o/r\n    ↳ Label it, then reply. Then ack.\n"),
            "instructions follow-on: {out}"
        );
    }

    /// Deserialize `json!` values through the real `WaitItem` path —
    /// the same typed parse the production stop hook uses — then
    /// render. Exercises both the deserialize and the projection.
    fn items_text(items: &[serde_json::Value]) -> String {
        let parsed: Vec<WaitItem> = items
            .iter()
            .map(|v| serde_json::from_value(v.clone()).unwrap())
            .collect();
        render_wait_items(
            &parsed,
            &AgentLabel::parse("codex").unwrap(),
            Role::Reviewer,
        )
    }

    const FULL_SHA: &str = "f6feba231685eb198eb412e5d014de836c4ddf81";

    #[test]
    fn reviewer_item_is_a_one_line_hint() {
        let out = items_text(&[serde_json::json!({
            "kind": "reviewer",
            "plan": "some-plan",
            "sha": FULL_SHA,
        })]);
        assert!(
            out.contains("  - reviewer: review some-plan @ f6feba231685\n"),
            "got:\n{out}"
        );
    }

    #[test]
    fn wake_carries_no_tutorial_strings() {
        // The HOW lives in the skill doc, not the wake
        // (wait-output-is-a-minimal-hint). One wake with every
        // tutorial-bearing kind must contain none of the old
        // reference material.
        let out = items_text(&[
            serde_json::json!({"kind": "reviewer", "plan": "p", "sha": FULL_SHA}),
            serde_json::json!({"kind": "promote_from_queue", "name": "n", "priority": 300}),
            serde_json::json!({"kind": "blocked", "agent": "claude", "name": "q",
                               "plan": "p", "question": "full question text"}),
            serde_json::json!({"kind": "adhoc_review", "sha": FULL_SHA}),
        ]);
        for tutorial in [
            "feedback write", // the spelled-out command
            "FINISHED",       // the verdict essay
            "continue|finished|request-changes",
            "clank unblock",      // the spelled-out unblock
            "evaluate whether",   // the promote walkthrough
            "full question text", // block question is for the human
            FULL_SHA,             // full sha never appears in the hint
        ] {
            assert!(
                !out.contains(tutorial),
                "wake must not carry `{tutorial}`; got:\n{out}"
            );
        }
    }

    /// A real message item, through the wait's own JSON, renders as
    /// its line and says what ends it — so the two sides' field names
    /// cannot drift apart.
    #[test]
    fn a_message_renders_its_handle_sender_and_subject() {
        let item = |from: Option<&str>, repo: Option<&str>| {
            serde_json::to_value(crate::cli::wait::render_json(
                &clank_core::wait::WaitItem::Message {
                    presentation: None,
                    requires_reply: false,
                    reply_route: None,
                    handle: "500-00000001-rebase-please".to_string(),
                    from: from.map(String::from),
                    repo: repo.map(String::from),
                    subject: "Rebase please".to_string(),
                    body: "# Rebase please\n\nonto master".to_string(),
                    selector: Some("messages@1".to_string()),
                },
            ))
            .unwrap()
        };
        let out = items_text(&[item(Some("claude"), Some("/src/clank")), item(None, None)]);
        let ack = "— ack `clank events ack messages@1`";
        assert!(
            out.contains(&format!(
                "  - message: 500-00000001-rebase-please from claude (/src/clank): \
                 Rebase please {ack}\n"
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "  - message: 500-00000001-rebase-please from a person: Rebase please {ack}\n"
            )),
            "{out}"
        );
        assert!(
            out.contains("fires until the message is acked — `clank events ack`"),
            "{out}"
        );
    }

    #[test]
    fn blocked_and_promote_are_one_liners() {
        let out = items_text(&[
            serde_json::json!({"kind": "blocked", "agent": "claude", "name": "q",
                               "plan": "foo", "question": "?"}),
            serde_json::json!({"kind": "promote_from_queue", "name": "next-up", "priority": 42}),
        ]);
        assert!(out.contains("  - blocked: claude/q on foo (awaiting human)\n"));
        assert!(out.contains("  - promote: next-up (priority 042)\n"));
    }

    #[test]
    fn unblocked_answer_is_signal_and_kept() {
        let out = items_text(&[serde_json::json!({
            "kind": "unblocked", "name": "q", "answer": "yes, proceed with B"
        })]);
        assert!(
            out.contains("  - unblocked: q: yes, proceed with B\n"),
            "the human's answer is the action payload; got:\n{out}"
        );
        assert!(out.contains("nothing to acknowledge"), "{out}");
        assert!(!out.contains("block clean"), "{out}");
    }

    #[test]
    fn parse_wait_json_reads_typed_envelope() {
        // typed-json-not-json-macro: the stop hook deserializes wait's
        // `--json` envelope into typed `WaitItem`s rather than walking
        // a `serde_json::Value`. `json!` here expresses the wait output
        // the hook consumes across the subprocess boundary.
        let raw = serde_json::json!({
            "items": [
                {"kind": "reviewer", "plan": "p", "sha": FULL_SHA},
                {"kind": "master", "plan": "p", "sha": FULL_SHA,
                 "next": "revise", "reason": "address_commit_changes"},
            ]
        })
        .to_string();
        let items = parse_wait_json(raw.as_bytes()).unwrap();
        assert_eq!(items.len(), 2);
        // TYPED by the parse, not a string a consumer re-reads.
        assert_eq!(
            items[0].wake_kind(),
            Some(clank_core::wait::WaitKind::Reviewer)
        );
        assert_eq!(items[1].next.as_deref(), Some("revise"));
    }

    #[test]
    fn parse_wait_json_is_fail_soft_on_unknown_kind_and_extra_fields() {
        // The subprocess boundary must stay loose: a future wait kind
        // or extra field can NEVER break parsing (the hook never fails
        // the agent). Unknown `kind` renders via the catchall arm;
        // unknown fields are ignored.
        let raw = serde_json::json!({
            "items": [
                {"kind": "future_kind", "plan": "p", "sha": FULL_SHA,
                 "some_new_field": {"nested": 1}},
            ]
        })
        .to_string();
        let items = parse_wait_json(raw.as_bytes()).unwrap();
        let out = render_wait_items(&items, &AgentLabel::parse("codex").unwrap(), Role::Reviewer);
        assert!(
            out.contains("  - future_kind: p @ f6feba231685\n"),
            "unknown kind must render via the catchall arm; got:\n{out}"
        );
    }

    fn duties(items: &[serde_json::Value]) -> Vec<String> {
        let wire = serde_json::json!({ "items": items });
        parse_wait_json(wire.to_string().as_bytes())
            .unwrap()
            .iter()
            .filter_map(duty_line)
            .collect()
    }

    fn one(item: serde_json::Value) -> String {
        let mut lines = duties(&[item]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        lines.remove(0)
    }

    /// Each kind's escalation line says what discharges it, by its own
    /// fields and class.
    #[test]
    fn each_repeated_item_says_what_discharges_it() {
        let required = one(serde_json::json!({"kind":"message","handle":"request",
            "requires_reply":true,"selector":"messages@7"}));
        assert!(
            required.contains("MUST handle this before ending your turn"),
            "{required}"
        );
        assert!(
            required.contains("`clank events read messages@7`"),
            "{required}"
        );
        assert!(
            required.contains("`clank events ack messages@7 --reply <text>`"),
            "{required}"
        );
        assert!(required.contains("does not waive it"), "{required}");
        assert!(!required.contains("clank block"), "{required}");
        assert!(
            !required.contains("request"),
            "the handle is not the selector: {required}"
        );

        let event = one(
            serde_json::json!({"kind":"command_event","name":"questions",
            "selector":"command-questions-abcd@7"}),
        );
        assert!(
            event.contains("`clank events ack command-questions-abcd@7`."),
            "{event}"
        );
        assert!(event.contains("answer or ack it now saying so"), "{event}");
        assert!(!event.contains("clank block"), "{event}");

        let review = one(serde_json::json!({"kind":"adhoc_review","sha":FULL_SHA}));
        assert!(review.contains(&FULL_SHA[..12]), "{review}");
        assert!(review.contains("feedback exists for that sha"), "{review}");
        assert!(!review.contains("clank block"), "{review}");
        let revise = one(serde_json::json!({"kind":"adhoc_revise","sha":FULL_SHA}));
        assert!(revise.contains("commit the revision"), "{revise}");

        let master = one(
            serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
            "next":"revise","reason":"address_commit_changes"}),
        );
        assert!(
            master.contains("commit work addressing the feedback"),
            "{master}"
        );
        assert!(master.contains("`clank block ask <name> -m"), "{master}");
        let promote =
            one(serde_json::json!({"kind":"promote_from_queue","name":"n","priority":500}));
        assert!(!promote.contains("clank block"), "{promote}");

        assert!(
            duties(&[serde_json::json!({"kind":"unblocked","name":"q","answer":"yes"})]).is_empty(),
            "an answer is received by its delivery and owes no duty"
        );
        let blocked = one(
            serde_json::json!({"kind":"blocked","agent":"claude","name":"q",
            "plan":"p","question":"?"}),
        );
        assert!(
            blocked.contains("only the human can clear this"),
            "{blocked}"
        );
        assert!(blocked.contains("do not answer it yourself"), "{blocked}");
        assert!(!blocked.contains("clank events") && !blocked.contains("clank block ask"));

        assert!(duties(&[serde_json::json!({"kind":"idle","prompt":"go"})]).is_empty());
        assert!(duties(&[serde_json::json!({"kind":"a_kind_from_the_future"})]).is_empty());
    }

    /// A mixed delivery treats each item by its own class: the block is
    /// offered for the plan's step alone, and a blocked item excuses
    /// nothing else.
    #[test]
    fn a_mixed_delivery_owes_each_item_its_own_duty() {
        let lines = duties(&[
            serde_json::json!({"kind":"message","handle":"h","selector":"messages@3"}),
            serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
                "next":"continue","reason":"gate_continue"}),
            serde_json::json!({"kind":"blocked","agent":"claude","name":"q","plan":"p"}),
        ]);
        assert_eq!(lines.len(), 3, "{lines:?}");
        let with_block: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("clank block ask"))
            .collect();
        assert_eq!(with_block.len(), 1, "{lines:?}");
        assert!(with_block[0].contains("master continue p"), "{lines:?}");
        assert!(
            lines[0].contains("`clank events ack messages@3`"),
            "{lines:?}"
        );
    }

    /// A repeated continuation carries its items' duties after the limits
    /// it keeps.
    #[test]
    fn a_repeated_continuation_carries_its_items_duties() {
        let lines = duties(&[serde_json::json!({"kind":"message","handle":"h",
            "selector":"messages@3"})]);
        let reason = |count| {
            let mut reason = "Items:".to_string();
            if let Some(more) = crate::cli::wake_repeat::escalation(count, &lines) {
                reason.push_str(&more);
            }
            reason
        };
        assert_eq!(reason(clank_core::turn_policy::ESCALATE_AT - 1), "Items:");
        let escalated = reason(clank_core::turn_policy::ESCALATE_AT);
        assert!(escalated.contains("grants no new authority"), "{escalated}");
        assert!(escalated.ends_with(&lines[0]), "{escalated}");
    }

    /// The line for an item as the real wait projection carries it.
    fn projected(item: clank_core::wait::WaitItem) -> String {
        let wire = serde_json::json!({"items":[crate::cli::wait::render_json(&item)]});
        let parsed = parse_wait_json(wire.to_string().as_bytes()).unwrap();
        duty_line(&parsed[0]).unwrap()
    }

    /// Through the real projection, every item is addressed by what
    /// identifies it: a GitHub event by its presentation's selector, an
    /// ad hoc commit by its sha.
    #[test]
    fn a_projected_item_is_addressed_by_its_own_identity() {
        use clank_core::wait::{EventPresentation, WaitItem as I};
        let github = projected(I::GithubEvent {
            presentation: Some(EventPresentation {
                source: "github-fixture".into(),
                seq: 7,
                ident: "fixture-event".into(),
            }),
            repo: "o/r".into(),
            event: "issue_opened".into(),
            detail: None,
            number: Some(1),
            title: None,
            actor: None,
            url: None,
            instructions: None,
            content: None,
        });
        assert!(
            github.contains("`clank events read github-fixture@7`"),
            "{github}"
        );
        assert!(
            github.contains("`clank events ack github-fixture@7`"),
            "{github}"
        );
        assert!(github.contains("MUST handle this"), "{github}");

        let sha = clank_core::ids::CommitSha::parse(FULL_SHA).unwrap();
        let review = projected(I::AdHocReview {
            sha: sha.clone(),
            feedback_path: "unused".into(),
        });
        assert!(review.contains(&FULL_SHA[..12]), "{review}");
        let revise = projected(I::AdHocRevise { sha });
        assert!(revise.contains(&FULL_SHA[..12]), "{revise}");
    }

    #[test]
    fn receipt_candidates_require_selected_known_real_command_identity_and_context() {
        let fixture = crate::cli::wake_receipt::fixtures::Fixture::new();
        let real = serde_json::to_value(super::super::wait::render_json(&fixture.item)).unwrap();
        for variant in ["failure", "unknown", "missing-selector", "missing-id"] {
            let mut item = real.clone();
            let mut envelope = serde_json::json!({"source_cwd": fixture.cwd});
            match variant {
                "failure" => item["failure"] = true.into(),
                "unknown" => item["kind"] = "future_command".into(),
                "missing-selector" => {
                    item.as_object_mut().unwrap().remove("selector");
                }
                "missing-id" => {
                    item.as_object_mut().unwrap().remove("id");
                }
                _ => unreachable!(),
            }
            envelope["items"] = serde_json::json!([item]);
            let items = parse_wait_json(&serde_json::to_vec(&envelope).unwrap()).unwrap();
            assert_eq!(receipt_candidates(&items).count(), 0, "{variant}");
        }
    }
}
