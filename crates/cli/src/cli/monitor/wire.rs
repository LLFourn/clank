use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

use crate::cli::watch_store::Producer;

/// The wire version. Bumped for any change a previous build would
/// misread; a mismatch is not an error but a decline, after which a
/// waiter follows whichever monitor replaces this one, or tells its agent
/// that none did.
///
/// Skew is routine rather than exceptional here: monitors outlive
/// `cargo install`, so a fresh client meeting an old monitor is the
/// expected case and must degrade rather than fail.
// Version 7 also makes single-tag diagnostics scalar.
// Version 8 hosts native timers and presents their additive event variant.
// Old monitors must not keep serving the previous lifecycle after an upgrade.
// Version 10 owns eligible-event interruption even without an armed waiter.
// Version 9 can host the same inbox but cannot promise this control lifecycle;
// a fresh client must negotiate retirement rather than silently reuse it.
// 11 admits one automatic Grok wait per bound session and reports refusals.
// 12 publishes its sources' lifecycle and takes a control request to
// restart one.
// 15 no longer carries PR-review items, which an older monitor can still
// send and this build cannot decode.
// 16 owns turn-end policy: an agent's hello may carry its hook's report,
// answered with verdicts and then one delivery result on that connection,
// and control requests attend and clear.
// 17 carries the exact answer an `unblocked` item delivers and receives it
// on delivery; a 16 monitor sends neither and still teaches `block clean`.
// 18 parks an attending agent's turn-end instead of silencing it; a 17
// monitor would leave it deaf to new work until its attendance ends.
// 19 ends a parked Stop with the `user_command` silent reason when a user
// command waits, which an 18 hook cannot decode; an 18 monitor never
// releases a park for one.
// 20 reads no task listing: a `clank run` registers its process and a
// `clank attend` declares a name, and Grok's handoff carries no waiter
// evidence because the monitor's own registration is it. A 19 monitor would
// wait for a listing that never comes.
// 21 no longer carries `run_ended` items, which a 20 monitor can still send
// and this build cannot decode.
// 22 owns repository watches independently of their current recipient. An
// older monitor ignores these declarations and cannot deliver their selectors.
// 24 is the first protocol pinned by `wire_golden_tests`: development builds
// speaking 23 lack `Control::Questions` and drop it silently.
// 25 names a restart's producer as a tagged value: an agent labelled
// `watch:1` and watch 1 display alike, so a 24 peer's string named either.
pub(crate) const PROTO: u32 = 25;

/// What a monitor can do, not just what its frames look like.
///
/// 4 is the HOSTING contract: this monitor runs an author's config
/// `wait_events` and routes their rows back, so a wait that has such a
/// source may stop running it. 3 is the monitor that does not — and
/// one of those can still be RUNNING after an ordinary binary update,
/// holding its repo's lease. It would accept the same hello, serve
/// only projected repo work, and the client would have stopped
/// tailing its own sources: configured events missed for as long as
/// that monitor lives (codex on a135276).
///
/// 5 adds the STAND-DOWN: a holder on 5 can be ASKED to retire by a
/// successor, over the socket, and answers. Anything older cannot be
/// asked at all — proto 3 and 4 both exit only on their own fold
/// watchdog — so "the live peer answered with a number below ours" is
/// the positive evidence that a cooperative retirement is unavailable,
/// and it is the ONLY thing that may be read that way. The bump is
/// forced rather than chosen: proto-4 monitors were already running
/// when this was written, so putting the capability on 4 would have
/// made `proto: 4` ambiguous between a peer that can be asked and one
/// that cannot, which is exactly the inference the bootstrap must not
/// make (codex on b1d5d27).
///
/// So a capability change is a version change. The client refuses a
/// monitor on any other number, which is a decline rather than a failure.
///
/// The version, readable WITHOUT decoding the rest.
///
/// Negotiation must inspect this before the body, because a body
/// decoded under the wrong version fails in a way that cannot be told
/// from corruption: proto 1's `Hello` had no holder, so reading it as
/// proto 2 reports a missing field rather than a skew (codex on
/// 4915f33). Serde ignores unknown fields, so this reads any version's
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct Envelope {
    pub(crate) proto: u32,
}

/// The holder a client speaks for: BOTH halves, or neither.
///
/// Mirrors `WaitBinding(Option<(Tool, SessionId)>)`, whose doc is the
/// requirement — *"a terminal wait with no session remains a direct,
/// unclaimed observer"*. So an unbound wait must be representable, and
/// a half-pair must not be: a tool without a session would invent an
/// identity for a terminal client, and a session without a tool is the
/// misattribution the previous commit was fixing (codex on 4915f33).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Holder {
    pub(crate) tool: clank_core::vocab::Tool,
    pub(crate) session: String,
}

/// Who is connecting, and therefore what may be sent to them.
///
/// An agent is told what IT is owed, which takes an identity the
/// roster recognises. An observer is told only that the fold moved:
/// `clank status --watch` and the TUI want a WAKE, not a projection,
/// and giving them an identity would have meant inventing a roster
/// entry for a panel. Two kinds of reader, so two kinds of hello —
/// the alternative was an author field that some clients leave empty,
/// which is the shape that let a missing `Option` serve a bound client
/// as an unbound one (codex on 4915f33).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "as", rename_all = "snake_case")]
pub(crate) enum Who {
    Agent {
        author: String,
        /// `None` for a terminal wait that holds nothing. The pair
        /// travels whole because `WaitBinding` validates it whole and
        /// rechecks it immediately before delivery, and one monitor
        /// serves agents of different tools — deriving either half
        /// from the monitor's own environment would attribute one
        /// agent's binding to another (codex on adb5453).
        ///
        /// The ROLE is deliberately absent: a demotion mid-wait must
        /// drop what the old watch queued, so the monitor routes by
        /// the author's current role and never one captured here.
        holder: Option<Holder>,
        /// Whether this client declared its own `--event` sources.
        /// Those belong to ONE invocation and the monitor never owns
        /// them, so it is told only that they exist — a monitor that
        /// pooled them would deliver one invocation's events to
        /// another.
        has_cli_sources: bool,
        /// The hook's report this wait carries. The monitor answers it
        /// with verdicts instead of items, and the connection's next
        /// frame is the delivery result. `None` for a plain wait.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_end: Option<crate::cli::turn_wire::Report>,
    },
    /// A read-only fold subscription. Any number, no identity, and
    /// never any items.
    Observer,
    /// A harness's recovery helper: waits for the monitor's decision to
    /// retry a failed turn and never takes inbox work, so its presence is
    /// not evidence the agent is waiting for any (`recovery.rs`).
    RecoveryHelper { author: String, holder: Holder },
    /// One request, answered with one note, after which the connection
    /// closes. Never a reader.
    Control { request: Control },
}

/// What a control client asks of the monitor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub(crate) enum Control {
    Questions {
        author: String,
        session: clank_core::SessionId,
        observation: crate::cli::question_state::Observation,
    },
    /// Restart one of `author`'s command sources, if what the monitor
    /// runs for it is still the configuration `accepted` fingerprints.
    Restart {
        author: Producer,
        key: String,
        accepted: String,
    },
    /// A `clank run` registering the process it is about to become.
    Run {
        author: String,
        session: clank_core::SessionId,
        desc: String,
        expect_ms: u64,
        proof: clank_core::turn_policy::ShellProof,
    },
    /// A `clank attend` declaration, standing until the harness's listing
    /// has neither its task nor work that task may have started, or
    /// `author` clears it.
    Attend {
        author: String,
        session: clank_core::SessionId,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        desc: Option<String>,
        expect_ms: u64,
    },
    /// Withdraw `which` of `author`'s attendances, and the check-ins they
    /// owe.
    Clear {
        author: String,
        session: clank_core::SessionId,
        #[serde(default)]
        which: clank_core::turn_policy::Clearing,
    },
    /// A turn `author`'s harness reported for its own `session`, for
    /// recovery to read. Answered with how a retry would reach it.
    Observe {
        author: String,
        tool: clank_core::Tool,
        session: String,
        turn: serde_json::Value,
    },
    /// A recovery helper's answer to one of `author`'s attempts.
    RecoveryReply {
        author: String,
        tool: clank_core::Tool,
        reply: crate::cli::recovery::Reply,
    },
}

/// What a client says on connecting.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Hello {
    pub(crate) proto: u32,
    /// The canonicalized repo this client speaks for, verified by
    /// [`serves`] — the filename is derived from it, and a derivation
    /// is not a guarantee.
    pub(crate) repo: String,
    pub(crate) who: Who,
    /// The build the client runs, as it says itself. `None` from a client
    /// built before clients said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) build: Option<String>,
}

/// What the monitor answers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum Welcome {
    Ok { proto: u32 },
    Decline { proto: u32, why: String },
}

/// What the monitor pushes to a client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum Note {
    /// A lifecycle or admission refusal. It must never fall back to a local
    /// watcher, which would bypass the monitor's ownership decision.
    Refused { why: String },
    /// A newer wait of the same agent session was admitted, and this one
    /// is retired: it ends silently, without watching for itself.
    Superseded,
    /// What this client's author is owed. Empty is never sent; a client
    /// blocks until it has something, as a park does today.
    Items {
        items: Vec<WaitItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_cwd: Option<std::path::PathBuf>,
    },
    /// A recovery helper's one authorized attempt: submit `wake` and
    /// answer with a `Control::RecoveryReply`.
    Recover {
        /// With `n`, which attempt the helper's answer is for.
        opened_by: String,
        n: u32,
        tag: String,
        wake: String,
    },
    /// A control request was carried out.
    Done { what: String },
    /// A turn-end verdict, for a connection carrying a report.
    Verdict(crate::cli::turn_wire::Frame),
    /// A fold happened and owes this client nothing new. Sent only on
    /// a fold, never on a clock: a client that hears nothing is on a
    /// quiet repo, and learns of a dead or wedged monitor (its watchdog
    /// stands it down) from the socket closing. Each one is a client's
    /// chance to notice it has been superseded.
    Beat,
}

/// What a repo's monitor publishes about itself.
///
/// The descriptor is how a client FINDS the socket, which is the part
/// of Claude Code's design that matters most: it writes
/// `~/.claude/sessions/<pid>.json` carrying `messagingSocketPath`, so
/// the socket's own filename carries no meaning and there is no derived
/// name to collide. An earlier draft here hashed the repo path into the
/// filename; a descriptor is strictly better — no collisions, and it
/// can carry the version and the fold a client needs before it even
/// connects.
///
/// It lives OUTSIDE the repo, in this uid's [`runtime_dir`](super::runtime::runtime_dir) keyed by
/// [`repo_tag`](super::runtime::repo_tag), because it means nothing once its monitor is gone. As
/// a plain file it is bound by PATH_MAX rather than `sun_path`, so
/// neither the repo's depth nor the namespace's costs anything.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Descriptor {
    pub(crate) proto: u32,
    pub(crate) pid: i32,
    /// The canonicalized repo, so a descriptor left behind by a repo
    /// since moved is recognised rather than trusted.
    pub(crate) repo: String,
    pub(crate) socket: String,
    /// The build serving, fixed for the monitor's life. What it has since
    /// seen of the installation is in its own observation record, never
    /// here: this file is routing, and a stale monitor's late write must
    /// not be able to change where clients connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) build: Option<DescribedBuild>,
}

/// A monitor's build, as its descriptor names it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DescribedBuild {
    pub(crate) running: String,
    /// `None` from a monitor built before versions were embedded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<String>,
    pub(crate) installed_at: String,
    pub(crate) source: crate::build_stamp::Source,
}

/// Whether a monitor serving `theirs` may answer a client from `ours`.
///
/// Checked even with a descriptor, because a descriptor can outlive the
/// repo it describes: a moved or recreated repo would otherwise have
/// its clients folding another repo's state.
pub(crate) fn serves(ours: &std::path::Path, theirs: &str) -> bool {
    ours.to_string_lossy() == theirs
}

/// Whether a monitor at `theirs` can serve a client built for `ours`.
pub(crate) fn admits(ours: u32, theirs: u32) -> bool {
    ours == theirs
}

/// Read a client's opening frame, VERSION FIRST.
///
/// The order is the point. A body decoded under the wrong version can
/// succeed with wrong semantics — serde reads a missing `Option` as
/// `None`, which would serve a bound client as an unbound one — so the
/// envelope decides before the body is looked at, and every refusal is
/// a `Decline` the client can act on rather than a parse error it
/// cannot distinguish from corruption.
///
/// Pure over the line so the whole negotiation is testable without a
/// socket.
pub(crate) fn accept_hello(line: &str, repo: &std::path::Path) -> Result<Hello, Welcome> {
    let decline = |why: String| Err(Welcome::Decline { proto: PROTO, why });
    let Ok(envelope) = serde_json::from_str::<Envelope>(line) else {
        return decline("the opening frame is not a monitor hello".into());
    };
    if !admits(PROTO, envelope.proto) {
        return decline(format!(
            "this monitor speaks proto {PROTO}, the client speaks {}",
            envelope.proto
        ));
    }
    let Ok(hello) = serde_json::from_str::<Hello>(line) else {
        return decline(format!("a proto {PROTO} hello did not parse"));
    };
    if !serves(repo, &hello.repo) {
        return decline(format!(
            "this monitor serves {}, the client asked for {}",
            repo.display(),
            hello.repo
        ));
    }
    Ok(hello)
}

/// What an opening frame turns out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Greeting {
    Client(Box<Hello>),
    /// Declined, and the decline says this monitor is OBSOLETE: the
    /// client speaks a NEWER version than it does.
    ///
    /// Which means it should go, and it is the only thing entitled to
    /// decide that — it is acting on its own process, in its own repo,
    /// so there is no identity to establish, no signal to aim and no
    /// permission to negotiate. All of that belonged to an earlier
    /// design where a STRANGER ended the holder, and all of it is
    /// deleted.
    Obsolete(Welcome),
    Decline(Welcome),
}

/// The compatibility FLOOR: what a monitor may rely on reading from
/// ANY version's opening frame, past or future.
///
/// Separate from [`Hello`] because `Hello` is the version-gated WORK
/// body and may change shape; these three fields are the contract that
/// does not. A frame that cannot produce one of these is a frame this
/// monitor knows nothing about, and nothing is decided from it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct Opening {
    pub(crate) proto: u32,
    pub(crate) repo: String,
    pub(crate) who: OpeningWho,
}

/// The floor's view of who is asking.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "as", rename_all = "snake_case")]
pub(crate) enum OpeningWho {
    Agent {
        author: String,
    },
    Observer,
    /// A kind this build does not know. Decided AGAINST, never for: a
    /// client we cannot identify is not one whose word ends a repo's
    /// owner.
    #[serde(other)]
    Unknown,
}

/// Read an opening frame, and notice when answering it means this
/// monitor is finished.
///
/// A client stays version-gated exactly as before — a body decoded
/// under the wrong version is the hazard that gate exists for. The ONE
/// addition is which side of the mismatch we are on, and it is reached
/// only after the frame has been VALIDATED as something addressed to
/// this repo by somebody this repo knows.
///
/// That validation is the correction, and it was a real hole: the
/// first version re-read `Envelope.proto` alone, so a bare
/// `{"proto":6}` — no repo, no identity, from anyone who could reach
/// the socket — retired the monitor, as did a higher-version frame
/// asking for a different repo entirely (codex on c4b5899). I had
/// claimed an unknown author could not reach this decision; that was
/// true only of the equal-version path, which is not the path that
/// matters.
/// What a monitor says when a newer client tells it it is obsolete. The
/// same words since proto 3, so a client can tell this answer from an
/// older monitor's other refusals byte for byte.
pub(crate) fn obsolete_why(ours: u32, theirs: u32) -> String {
    format!("this monitor speaks proto {ours}, the client speaks {theirs}")
}

pub(crate) fn greeting_of(line: &str, repo: &std::path::Path) -> Greeting {
    let decline = |why: String| Greeting::Decline(Welcome::Decline { proto: PROTO, why });
    let Ok(open) = serde_json::from_str::<Opening>(line) else {
        // Not even the floor. Whatever it is, it decides nothing.
        return match accept_hello(line, repo) {
            Ok(hello) => Greeting::Client(Box::new(hello)),
            Err(d) => Greeting::Decline(d),
        };
    };
    if !serves(repo, &open.repo) {
        return decline(format!(
            "this monitor serves {}, the client asked for {}",
            repo.display(),
            open.repo
        ));
    }
    if open.proto > PROTO {
        // Older than the client, and the client is addressed to this
        // repo and known to it. We are what an install replaced.
        return match &open.who {
            OpeningWho::Observer => Greeting::Obsolete(Welcome::Decline {
                proto: PROTO,
                why: obsolete_why(PROTO, open.proto),
            }),
            OpeningWho::Agent { author } => match AgentLabel::parse(author)
                .ok()
                .filter(|a| crate::agent_store::resolve_role(repo, a).is_ok())
            {
                Some(_) => Greeting::Obsolete(Welcome::Decline {
                    proto: PROTO,
                    why: obsolete_why(PROTO, open.proto),
                }),
                // On this repo's socket, for this repo, but not one of
                // its agents. Refused, and this monitor stays.
                None => decline(format!("`{author}` is not on this repo's roster")),
            },
            OpeningWho::Unknown => {
                decline("a client kind this monitor does not recognise".to_string())
            }
        };
    }
    match accept_hello(line, repo) {
        Ok(hello) => Greeting::Client(Box::new(hello)),
        Err(d) => Greeting::Decline(d),
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "wire_golden_tests.rs"]
mod wire_golden_tests;
