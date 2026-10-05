//! What a watch admits: the one predicate the classifiers, the PR
//! scope and the replay tail all ask.
//!
//! A watch is a union of CLAUSES. Each names some objects, the events
//! under them, and the filters that describe those events — so a
//! filter sits at the level it is about, which is the distinction
//! `pr_comment` + a whole-watch `include_own_actions` could not draw
//! (`github-selectors`).
//!
//! The flat config fields are not a second model. They DESUGAR into
//! clauses, so the asymmetry they carry — `prs` scoping only the
//! PR-scoped kinds, `branches` only pushes — becomes a decomposition
//! into the same clause type rather than a mode the predicate has to
//! remember.
//!
//! Spreading one decision over three call sites is how the push path
//! lost its own-actions filter the moment the shared check moved: an
//! early return can skip a check that lives beside it, and cannot
//! skip one it has to ask for.

use clank_core::agent_config::{GithubEventKind, GithubSource, WatchClause};

/// One event, described by everything a clause can select on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Event<'a> {
    pub(crate) kind: GithubEventKind,
    /// The classified sub-kind, which is what narrows `pr_comment` to
    /// one comment class.
    pub(crate) detail: Option<&'a str>,
    pub(crate) number: Option<u64>,
    pub(crate) branch: Option<&'a str>,
    /// The actor is the watcher's own login.
    pub(crate) is_own: bool,
}

/// The answer, with the one rejection worth reporting kept apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Admitted,
    Rejected,
    /// Rejected because nothing could PLACE it: every clause that
    /// governs this kind scopes by object, and the event named none.
    ///
    /// Its own answer because it is the failure a scope makes hard to
    /// notice — an out-of-scope PR is the scope working, an
    /// unplaceable event is a payload the scope cannot judge.
    Unplaceable,
}

impl Admission {
    /// For a caller whose event can never be unplaceable — it carries
    /// a number by construction — and so has no diagnostic to print.
    pub(crate) fn admitted(self) -> bool {
        self == Self::Admitted
    }
}

/// Which objects a clause is about. A clause covers ONE object type,
/// so one list says which of them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    Any,
    Prs(Vec<u64>),
    Branches(Vec<String>),
}

impl Scope {
    /// An empty list means every object, and collapsing it here is
    /// what keeps "has a scope" and "the scope is empty" from ever
    /// disagreeing downstream.
    fn prs(ns: &[u64]) -> Self {
        if ns.is_empty() {
            Self::Any
        } else {
            Self::Prs(ns.to_vec())
        }
    }

    fn branches(bs: &[String]) -> Self {
        if bs.is_empty() {
            Self::Any
        } else {
            Self::Branches(bs.to_vec())
        }
    }

    /// The PR numbers this scope names, or `None` if it does not
    /// select PRs at all.
    fn pr_numbers(&self) -> Option<&[u64]> {
        match self {
            Self::Prs(ns) => Some(ns),
            _ => None,
        }
    }

    fn canonical(&self) -> String {
        match self {
            Self::Any => String::new(),
            Self::Prs(ns) => {
                let mut ns = ns.clone();
                ns.sort_unstable();
                ns.dedup();
                format!("prs{ns:?}")
            }
            Self::Branches(bs) => {
                let mut bs = bs.clone();
                bs.sort();
                bs.dedup();
                format!("branches{bs:?}")
            }
        }
    }

    fn admits(&self, ev: &Event<'_>) -> bool {
        match self {
            Self::Any => true,
            Self::Prs(ns) => ev.number.is_some_and(|n| ns.contains(&n)),
            Self::Branches(bs) => ev.branch.is_some_and(|b| bs.iter().any(|x| x == b)),
        }
    }
}

/// One event a clause names: a kind, optionally narrowed to one of
/// its details. `None` admits every detail — only `pr_comment` has
/// more than one worth naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selector {
    kind: GithubEventKind,
    detail: Option<&'static str>,
}

impl Selector {
    fn selects(&self, ev: &Event<'_>) -> bool {
        self.kind == ev.kind && self.detail.is_none_or(|d| Some(d) == ev.detail)
    }

    fn canonical(&self) -> String {
        match self.detail {
            Some(d) => format!("{}/{d}", self.kind.wire()),
            None => self.kind.wire().to_string(),
        }
    }
}

/// Objects × their events × the filters on those events.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Clause {
    selectors: Vec<Selector>,
    scope: Scope,
    include_own: bool,
}

impl Clause {
    /// Whether the clause names this KIND at all, detail aside. What
    /// a caller asks when it has no event yet — and what the row
    /// scope asks, since a saved row's detail is not what a scope is
    /// about.
    fn governs(&self, kind: GithubEventKind) -> bool {
        self.selectors.iter().any(|s| s.kind == kind)
    }

    fn selects(&self, ev: &Event<'_>) -> bool {
        self.selectors.iter().any(|s| s.selects(ev))
    }

    fn canonical(&self) -> String {
        let mut sels: Vec<String> = self.selectors.iter().map(Selector::canonical).collect();
        sels.sort();
        sels.dedup();
        format!(
            "{}|{}|{}",
            sels.join(","),
            self.scope.canonical(),
            self.include_own
        )
    }

    fn suppresses_own(&self, ev: &Event<'_>) -> bool {
        ev.is_own && !self.include_own && ev.kind.is_filterable_by_actor()
    }
}

/// Everything one watch admits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    clauses: Vec<Clause>,
}

impl Selection {
    /// What this source selects, in whichever shape it was written.
    pub(crate) fn of(src: &GithubSource) -> Self {
        if src.watch.is_empty() {
            Self::flat(src)
        } else {
            Self::nested(src)
        }
    }

    /// The flat fields, desugared.
    ///
    /// `prs` governs only the kinds [`GithubEventKind::is_pr_scoped`]
    /// names and `branches` only `branch_push`, so the flat shape is
    /// up to three clauses over one event list, not one clause.
    fn flat(src: &GithubSource) -> Self {
        let selectors = |keep: fn(GithubEventKind) -> bool| {
            src.events
                .iter()
                .copied()
                .filter(|k| keep(*k))
                .map(|kind| Selector { kind, detail: None })
                .collect::<Vec<_>>()
        };
        let clause = |selectors: Vec<Selector>, scope| {
            (!selectors.is_empty()).then_some(Clause {
                selectors,
                scope,
                include_own: src.include_own_actions,
            })
        };
        Self {
            clauses: [
                clause(
                    selectors(GithubEventKind::is_pr_scoped),
                    Scope::prs(&src.prs),
                ),
                clause(
                    selectors(|k| k == GithubEventKind::BranchPush),
                    Scope::branches(&src.branches),
                ),
                clause(
                    selectors(|k| !k.is_pr_scoped() && k != GithubEventKind::BranchPush),
                    Scope::Any,
                ),
            ]
            .into_iter()
            .flatten()
            .collect(),
        }
    }

    /// The written clauses, one for one. The top-level
    /// `include_own_actions` is the DEFAULT a clause may override, so
    /// a watch that wants one rule everywhere still writes it once.
    fn nested(src: &GithubSource) -> Self {
        Self {
            clauses: src
                .watch
                .iter()
                .map(|c| Clause {
                    selectors: c
                        .selects()
                        .into_iter()
                        .map(|(kind, detail)| Selector { kind, detail })
                        .collect(),
                    scope: match c {
                        WatchClause::Pr(w) => Scope::prs(&w.numbers),
                        WatchClause::Issue(_) => Scope::Any,
                        WatchClause::Branch(w) => Scope::branches(&w.names),
                    },
                    include_own: c.include_own_actions().unwrap_or(src.include_own_actions),
                })
                .collect(),
        }
    }

    /// Whether any clause names this kind at all — what a classifier
    /// asks before it has an event to judge.
    pub(crate) fn wants(&self, kind: GithubEventKind) -> bool {
        self.clauses.iter().any(|c| c.governs(kind))
    }

    /// Every kind any clause names, deduped — the watch's event
    /// VOCABULARY, which is an over-approximation of what it admits.
    ///
    /// Only for callers that subscribe or bound work, where asking
    /// for too much costs requests and asking for too little loses
    /// events: the webhook names to forward, and whether to read
    /// checks at all. NEVER for deciding that an event reaches the
    /// agent — a clause names an event AND the objects it is about,
    /// and twice now a caller that forgot the second half delivered
    /// something no clause had asked for (codex 9dc8a13, bcda3c8).
    /// That question is [`admits`](Self::admits), and it is the only
    /// one that answers it.
    pub(crate) fn kinds(&self) -> Vec<GithubEventKind> {
        GithubEventKind::ALL
            .into_iter()
            .filter(|k| self.wants(*k))
            .collect()
    }

    /// Whether to read checks at all. Vocabulary, not a verdict —
    /// which conclusions become wakes is `ci::wakes` asking
    /// [`admits`](Self::admits) per PR.
    pub(crate) fn wants_ci(&self) -> bool {
        self.kinds().iter().any(|k| k.is_ci())
    }

    /// The PRs a CI watch must read, empty meaning every open one.
    ///
    /// An UNSCOPED ci clause means every open PR, so it erases the
    /// other clauses' numbers rather than narrowing the work to a
    /// list that would miss what the watch admits. Bounding the work
    /// tighter than the selection is how a scope stops being a filter
    /// and starts being a silent drop.
    pub(crate) fn ci_scope(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for c in self
            .clauses
            .iter()
            .filter(|c| c.selectors.iter().any(|s| s.kind.is_ci()))
        {
            match c.scope.pr_numbers() {
                None => return Vec::new(),
                Some(ns) => out.extend_from_slice(ns),
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether any clause filters the watcher's own actions, and so
    /// needs a login to compare against.
    ///
    /// A clause naming only `review_submitted` does not, however it
    /// sets the filter: that kind is never suppressed, so a login
    /// lookup could only fail and stall an event nothing was going to
    /// filter.
    pub(crate) fn needs_own_login(&self) -> bool {
        self.clauses
            .iter()
            .any(|c| !c.include_own && c.selectors.iter().any(|s| s.kind.is_filterable_by_actor()))
    }

    /// This selection's identity, independent of how it was written:
    /// clauses, their selectors, their scopes and their numbers all
    /// sorted, so reordering a watch does not re-key its log.
    ///
    /// Only a NESTED watch is keyed this way. A flat one keeps the
    /// bytes it has always hashed — see `supervisor::source_key`.
    pub(crate) fn canonical(&self) -> String {
        let mut clauses: Vec<String> = self.clauses.iter().map(Clause::canonical).collect();
        clauses.sort();
        clauses.dedup();
        clauses.join(";")
    }

    /// Whether this event reaches the watch.
    ///
    /// A union: one clause admitting it is enough, and a second
    /// clause matching the same event is a no-op rather than a
    /// second wake, because the event's identity — its feed id or
    /// action key — is what the WAL dedups on.
    ///
    /// `Unplaceable` is reported only for an event a clause would
    /// otherwise have taken, so a watch hears about a payload its
    /// scope cannot judge and stays quiet about one it filtered.
    pub(crate) fn admits(&self, ev: &Event<'_>) -> Admission {
        let mut unplaceable = false;
        for c in self
            .clauses
            .iter()
            .filter(|c| c.selects(ev) && !c.suppresses_own(ev))
        {
            if c.scope.admits(ev) {
                return Admission::Admitted;
            }
            unplaceable |= ev.number.is_none() && c.scope.pr_numbers().is_some();
        }
        if unplaceable {
            Admission::Unplaceable
        } else {
            Admission::Rejected
        }
    }

    /// Whether a STORED row still falls inside the watch's PR scope.
    ///
    /// The PR numbers alone — not the event filters, which a row
    /// already passed on its way into the log, and not the branches,
    /// which a row cannot be re-judged by. A log is keyed on every
    /// selector, so this is not a config-skew filter: it guards
    /// VERSION skew, because the set of kinds `prs` governs has grown
    /// and a log written when it meant less is full of rows a current
    /// binary would not have ingested.
    pub(crate) fn row_in_scope(&self, kind: GithubEventKind, number: Option<u64>) -> Admission {
        let Some(scopes) = self
            .clauses
            .iter()
            .filter(|c| c.governs(kind))
            .map(|c| c.scope.pr_numbers())
            .collect::<Option<Vec<_>>>()
        else {
            return Admission::Admitted;
        };
        if scopes.is_empty() {
            return Admission::Admitted;
        }
        match number {
            Some(n) if scopes.iter().any(|ns| ns.contains(&n)) => Admission::Admitted,
            Some(_) => Admission::Rejected,
            None => Admission::Unplaceable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GithubEventKind::*;

    fn src(events: &[GithubEventKind]) -> GithubSource {
        GithubSource {
            interrupt: false,
            interrupt_message: None,
            repo: "o/r".into(),
            events: events.to_vec(),
            watch: Vec::new(),
            poll_interval: None,
            include_own_actions: false,
            branches: Vec::new(),
            prs: Vec::new(),
            prompt: None,
            delivery: Default::default(),
        }
    }

    fn ev(kind: GithubEventKind, number: Option<u64>) -> Event<'static> {
        Event {
            kind,
            detail: None,
            number,
            branch: None,
            is_own: false,
        }
    }

    /// The flat shape's asymmetry, at the level a live event meets
    /// it. `prs` names PRs that already exist, so it cannot govern
    /// the event that brings one into being — a watch asking for new
    /// PRs and one named PR's traffic lists both and gets both.
    #[test]
    fn a_flat_scope_governs_only_the_kinds_that_name_a_pr() {
        let mut s = src(&[PrOpened, PrComment, IssueOpened]);
        s.prs = vec![7];
        let sel = Selection::of(&s);
        assert!(sel.admits(&ev(PrComment, Some(7))).admitted());
        assert!(!sel.admits(&ev(PrComment, Some(8))).admitted());
        assert!(sel.admits(&ev(PrOpened, Some(8))).admitted());
        assert!(sel.admits(&ev(IssueOpened, Some(8))).admitted());
    }

    #[test]
    fn a_flat_branch_list_governs_only_pushes() {
        let mut s = src(&[BranchPush, PrComment]);
        s.branches = vec!["main".into()];
        let sel = Selection::of(&s);
        let push = |b| Event {
            kind: BranchPush,
            detail: None,
            number: None,
            branch: Some(b),
            is_own: false,
        };
        assert!(sel.admits(&push("main")).admitted());
        assert!(!sel.admits(&push("topic")).admitted());
        // A comment is not a push and the branch list is not about it,
        // so it passes with no branch at all.
        assert!(sel.admits(&ev(PrComment, Some(1))).admitted());
    }

    #[test]
    fn own_actions_suppress_chatter_but_never_a_submitted_review() {
        let sel = Selection::of(&src(&[PrComment, ReviewSubmitted, BranchPush]));
        let own = |kind| Event {
            kind,
            detail: None,
            number: Some(1),
            branch: Some("main"),
            is_own: true,
        };
        assert!(!sel.admits(&own(PrComment)).admitted());
        assert!(!sel.admits(&own(BranchPush)).admitted());
        assert!(sel.admits(&own(ReviewSubmitted)).admitted());
    }

    #[test]
    fn a_watch_that_includes_its_own_actions_keeps_them_all() {
        let mut s = src(&[PrComment, ReviewSubmitted]);
        s.include_own_actions = true;
        let sel = Selection::of(&s);
        for kind in [PrComment, ReviewSubmitted] {
            assert!(
                sel.admits(&Event {
                    kind,
                    detail: None,
                    number: Some(1),
                    branch: None,
                    is_own: true,
                })
                .admitted(),
                "{}",
                kind.wire()
            );
        }
    }

    /// `Unplaceable` is the diagnostic for a payload the scope cannot
    /// judge, so it is reserved for an event a clause would otherwise
    /// have taken. An unwanted kind and a filtered actor are the
    /// filters working, and neither earns a word.
    #[test]
    fn only_a_wanted_unfiltered_event_can_be_unplaceable() {
        let mut s = src(&[PrComment, ReviewSubmitted]);
        s.prs = vec![7];
        let sel = Selection::of(&s);
        assert_eq!(sel.admits(&ev(PrComment, None)), Admission::Unplaceable);
        assert_eq!(sel.admits(&ev(PrMerged, None)), Admission::Rejected);
        assert_eq!(
            sel.admits(&Event {
                kind: PrComment,
                detail: None,
                number: None,
                branch: None,
                is_own: true,
            }),
            Admission::Rejected,
        );
        // Except for the one kind the actor filter never reaches.
        assert_eq!(
            sel.admits(&Event {
                kind: ReviewSubmitted,
                detail: None,
                number: None,
                branch: None,
                is_own: true,
            }),
            Admission::Unplaceable,
        );
    }

    fn nested(watch: &str) -> Selection {
        let mut s = src(&[]);
        s.watch = serde_json::from_str(watch).expect("the clauses parse");
        assert_eq!(s.refusal(), None, "{watch}");
        Selection::of(&s)
    }

    fn comment(detail: &str, number: u64, is_own: bool) -> Event<'_> {
        Event {
            kind: PrComment,
            detail: Some(detail),
            number: Some(number),
            branch: None,
            is_own,
        }
    }

    /// THE WATCH THE INCIDENT NEEDED, in the nested shape: everyone's
    /// submitted reviews on two PRs, everyone else's comments on them,
    /// and never this login's own drafts.
    #[test]
    fn one_clause_says_every_review_and_other_peoples_comments() {
        let sel = nested(
            r#"[{"pr": {"numbers": [603, 522],
                                    "events": ["review_submitted", "comment"]}}]"#,
        );
        let review = |number, is_own| Event {
            kind: ReviewSubmitted,
            detail: Some("approved"),
            number: Some(number),
            branch: None,
            is_own,
        };
        assert!(sel.admits(&review(603, false)).admitted());
        assert!(
            sel.admits(&review(603, true)).admitted(),
            "its own submission too — that is the act the team waits on",
        );
        assert!(
            sel.admits(&comment("review_comment", 603, false))
                .admitted()
        );
        assert!(
            !sel.admits(&comment("review_comment", 603, true)).admitted(),
            "while its own draft stays quiet",
        );
        assert!(
            !sel.admits(&review(999, false)).admitted(),
            "and another PR is not this watch's business",
        );
    }

    /// A filter sits at the level it describes. One clause hears
    /// everyone's merges including its own; its neighbour hears only
    /// other people's comments; and the watch-level setting is the
    /// default either may override.
    #[test]
    fn a_clause_carries_its_own_actor_filter() {
        let sel = nested(
            r#"[{"pr": {"events": ["merged"], "include_own_actions": true}},
                             {"pr": {"events": ["comment"]}}]"#,
        );
        assert!(
            sel.admits(&Event {
                kind: PrMerged,
                detail: None,
                number: Some(1),
                branch: None,
                is_own: true,
            })
            .admitted(),
            "the clause that asked for its own actions gets them",
        );
        assert!(
            !sel.admits(&comment("review_comment", 1, true)).admitted(),
            "while the clause beside it does not",
        );

        // And the watch-level setting is the default a clause inherits.
        let mut s = src(&[]);
        s.include_own_actions = true;
        s.watch = serde_json::from_str(r#"[{"pr": {"events": ["comment"]}}]"#).unwrap();
        assert!(
            Selection::of(&s)
                .admits(&comment("review_comment", 1, true))
                .admitted(),
            "written once at the top, inherited by a clause that says nothing",
        );
    }

    /// The asymmetry the nested shape exists to remove: numbers under
    /// an object scope EVERY event beneath them, `opened` included,
    /// where the flat `prs` deliberately leaves `pr_opened` out. The
    /// two rules over one config is why nothing translates between
    /// the shapes.
    #[test]
    fn object_numbers_scope_every_event_beneath_them() {
        let sel = nested(r#"[{"pr": {"numbers": [7], "events": ["opened", "merged"]}}]"#);
        assert!(sel.admits(&ev(PrOpened, Some(7))).admitted());
        assert!(!sel.admits(&ev(PrOpened, Some(8))).admitted());

        let mut flat = src(&[PrOpened, PrMerged]);
        flat.prs = vec![7];
        assert!(
            Selection::of(&flat)
                .admits(&ev(PrOpened, Some(8)))
                .admitted(),
            "the SAME numbers written flat still admit any new PR",
        );
    }

    /// A branch clause scopes by name the way a pr clause scopes by
    /// number, and an empty list is every branch — the same rule for
    /// the same reason, spelled once per object type.
    #[test]
    fn branch_names_scope_a_push_clause() {
        let push = |b| Event {
            kind: BranchPush,
            detail: None,
            number: None,
            branch: Some(b),
            is_own: false,
        };
        let named = nested(r#"[{"branch": {"names": ["main"], "events": ["push"]}}]"#);
        assert!(named.admits(&push("main")).admitted());
        assert!(!named.admits(&push("topic")).admitted());
        assert!(
            nested(r#"[{"branch": {"events": ["push"]}}]"#)
                .admits(&push("topic"))
                .admitted(),
            "no names is every branch",
        );
        // A push cannot be placed by PR numbers, so naming none is not
        // the unplaceable diagnostic — it is a branch that missed.
        assert_eq!(named.admits(&push("topic")), Admission::Rejected);
    }

    /// Clauses select; they do not fan out. Two clauses matching one
    /// event is one admission, and the log's own identity is what
    /// makes the second match a no-op — the same mechanism that stops
    /// a poll and a webhook delivering one event twice.
    #[test]
    fn two_clauses_matching_one_event_admit_it_once() {
        let sel = nested(
            r#"[{"pr": {"numbers": [603], "events": ["review_submitted"]}},
                             {"pr": {"events": ["review_submitted", "merged"]}}]"#,
        );
        assert_eq!(
            sel.admits(&ev(ReviewSubmitted, Some(603))),
            Admission::Admitted,
        );
    }

    /// A clause can name ONE comment class, which is what a watch
    /// that wants inline review traffic and nothing else writes.
    #[test]
    fn a_comment_class_narrows_without_inventing_a_kind() {
        let sel = nested(r#"[{"pr": {"events": ["review_comment"]}}]"#);
        assert!(sel.admits(&comment("review_comment", 1, false)).admitted());
        for other in ["issue_comment_on_pr", "review"] {
            assert!(
                !sel.admits(&comment(other, 1, false)).admitted(),
                "{other} is a different class",
            );
        }
        assert!(
            sel.wants(PrComment),
            "it is still pr_comment, so the log keys it as one",
        );

        // And the unnarrowed name takes all three.
        let all = nested(r#"[{"pr": {"events": ["comment"]}}]"#);
        for class in ["review_comment", "issue_comment_on_pr", "review"] {
            assert!(all.admits(&comment(class, 1, false)).admitted(), "{class}");
        }
    }

    /// CI reads a PR at a time, so its scope BOUNDS work rather than
    /// filtering it. An unscoped clause means every open PR, and
    /// bounding tighter than the selection would drop what the watch
    /// admits.
    #[test]
    fn the_ci_scope_never_bounds_tighter_than_the_selection() {
        assert_eq!(
            nested(
                r#"[{"pr": {"numbers": [9, 7, 9], "events": ["ci_failed"]}},
                       {"pr": {"numbers": [7, 3], "events": ["ci_passed"]}}]"#
            )
            .ci_scope(),
            vec![3, 7, 9],
        );
        assert_eq!(
            nested(
                r#"[{"pr": {"numbers": [7], "events": ["ci_failed"]}},
                       {"pr": {"events": ["ci_passed"]}}]"#
            )
            .ci_scope(),
            Vec::<u64>::new(),
            "one unscoped ci clause means every open PR",
        );
        assert_eq!(
            nested(r#"[{"pr": {"numbers": [7], "events": ["merged"]}}]"#).ci_scope(),
            Vec::<u64>::new(),
            "a watch with no ci events reads no checks at all",
        );

        let mut flat = src(&[GithubEventKind::CiFailed, PrOpened]);
        flat.prs = vec![7];
        assert_eq!(
            Selection::of(&flat).ci_scope(),
            vec![7],
            "unchanged for a flat watch"
        );
    }

    /// The canonical form is about WHAT a watch admits, so writing the
    /// same watch differently must not re-key its log.
    #[test]
    fn the_canonical_form_survives_reordering_and_relisting() {
        let a = nested(
            r#"[{"pr": {"numbers": [603, 522], "events": ["merged", "opened"]}},
                           {"issue": {"events": ["opened"]}}]"#,
        );
        let b = nested(
            r#"[{"issue": {"events": ["opened"]}},
                           {"pr": {"numbers": [522, 603, 522], "events": ["opened", "merged"]}}]"#,
        );
        assert_eq!(a.canonical(), b.canonical());

        // But a DIFFERENT selection is a different watch: narrowing
        // the numbers, dropping an event, or flipping a filter each
        // change what is presentable.
        for other in [
            r#"[{"pr": {"numbers": [603], "events": ["merged", "opened"]}},
                {"issue": {"events": ["opened"]}}]"#,
            r#"[{"pr": {"numbers": [603, 522], "events": ["merged"]}},
                {"issue": {"events": ["opened"]}}]"#,
            r#"[{"pr": {"numbers": [603, 522], "events": ["merged", "opened"],
                        "include_own_actions": true}},
                {"issue": {"events": ["opened"]}}]"#,
        ] {
            assert_ne!(a.canonical(), nested(other).canonical(), "{other}");
        }
    }

    /// A login lookup can fail, and a watch that has nothing to
    /// compare a login against must not be held up by one. Only a
    /// clause that filters a kind the filter can REACH needs it.
    #[test]
    fn only_a_clause_that_can_filter_needs_the_watchers_login() {
        assert!(
            !nested(r#"[{"pr": {"events": ["review_submitted"]}}]"#).needs_own_login(),
            "the one kind the filter never touches, however the filter is set",
        );
        assert!(
            !nested(r#"[{"pr": {"events": ["review_submitted"], "include_own_actions": false}}]"#)
                .needs_own_login(),
        );
        assert!(
            nested(r#"[{"pr": {"events": ["review_submitted", "comment"]}}]"#).needs_own_login()
        );
        assert!(
            !nested(r#"[{"pr": {"events": ["comment"], "include_own_actions": true}}]"#)
                .needs_own_login(),
            "and a clause that wants its own actions has nothing to compare either",
        );
        assert!(
            Selection::of(&src(&[PrComment])).needs_own_login(),
            "flat, unchanged"
        );
    }

    #[test]
    fn wants_is_membership_alone_so_a_scope_cannot_hide_a_kind() {
        let mut s = src(&[PrComment]);
        s.prs = vec![7];
        let sel = Selection::of(&s);
        assert!(sel.wants(PrComment));
        assert!(!sel.wants(ReviewSubmitted));
    }
}
