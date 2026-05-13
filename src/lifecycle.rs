//! Sans-IO lifecycle reducer.
//!
//! This module owns the decision logic for a Trinity session's active plan.
//! It is intentionally pure: no SQL, no filesystem, no async. All
//! decisions are taken as `(Option<ActivePlan>, Observation) -> Decision`.
//!
//! The reducer never branches on the plan file *path* — path is session-level
//! metadata maintained by the caller. The reducer cares only about content
//! and commit identity.
//!
//! Feedback is **not** modelled in the reducer. Feedback never alters
//! `ActivePlan`; it is a separate structural concern owned by
//! `SessionService::put_feedback`.

use std::fmt;

// =====================================================================
// Newtype wrappers
// =====================================================================

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }
    };
}

string_newtype!(SessionId);
string_newtype!(CommitSha);
string_newtype!(ContentHash);
string_newtype!(AgentLabel);
string_newtype!(PlanFilePath);

// =====================================================================
// Domain types
// =====================================================================

/// Snapshot of a git commit captured at observation time. Carried in
/// `CommitObserved` and applied via `RecordImplementation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSnapshot {
    pub sha: CommitSha,
    pub parent_sha: Option<CommitSha>,
    pub branch: Option<String>,
    pub message: String,
    pub diff_stat: String,
    pub worktree_status: Option<String>,
    pub is_head: bool,
}

/// In-memory cache shape of the live plan in a session. Persisted state
/// lives in SQL; this struct is rebuilt on daemon startup from
/// `sessions.active_plan_id` and the latest revision rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivePlan {
    Planning {
        base_commit: CommitSha,
        latest_plan_hash: ContentHash,
    },
    Implementing {
        base_commit: CommitSha,
        latest_plan_hash: ContentHash,
        latest_impl_commit: CommitSha,
    },
}

impl ActivePlan {
    fn latest_plan_hash(&self) -> &ContentHash {
        match self {
            ActivePlan::Planning {
                latest_plan_hash, ..
            }
            | ActivePlan::Implementing {
                latest_plan_hash, ..
            } => latest_plan_hash,
        }
    }

    fn base_commit(&self) -> &CommitSha {
        match self {
            ActivePlan::Planning { base_commit, .. }
            | ActivePlan::Implementing { base_commit, .. } => base_commit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// Master calls `register_plan_file`. `path` is carried for caller-side
    /// path bookkeeping; the reducer does not branch on it.
    PlanRegistered {
        path: PlanFilePath,
        body: String,
        head: CommitSha,
    },
    /// File-watcher reports the registered file's contents have changed
    /// (already debounced + read).
    PlanFileObserved { body: String, head: CommitSha },
    /// Master registered a commit (real `git rev-parse` validated snapshot).
    CommitObserved { commit: CommitSnapshot },
    /// Human (or master) asked for the active plan to be archived.
    ArchiveRequested,
    /// Operator (or agent via `finish_plan`) declared the active plan
    /// successfully concluded. Terminal — observations cannot route into
    /// finished plans (`sessions.active_plan_id` is cleared by the apply
    /// layer, same shape as `ArchiveRequested`).
    FinishRequested,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    StartPlan {
        base_commit: CommitSha,
        initial_body: String,
    },
    RecordPlanRevision {
        body: String,
    },
    RecordImplementation {
        commit: CommitSnapshot,
    },
    ArchiveActivePlan,
    FinishActivePlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub new_active: Option<ActivePlan>,
    pub effects: Vec<Effect>,
}

impl Decision {
    fn noop(active: Option<ActivePlan>) -> Self {
        Self {
            new_active: active,
            effects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    #[error("observation requires an active plan, but the session has none")]
    NoActivePlan,
    #[error("invalid observation: {0}")]
    InvalidObservation(&'static str),
}

// =====================================================================
// Hashing helper
// =====================================================================

/// Stable content hash for a plan-file body. Identifies "is this the same
/// plan or a new one?" decisions in the reducer.
pub fn content_hash(body: &str) -> ContentHash {
    ContentHash(blake3::hash(body.as_bytes()).to_hex().to_string())
}

// =====================================================================
// The reducer
// =====================================================================

/// Pure decision function. Given the current active plan and an
/// observation, produce a `Decision` (new state + effect list). No IO.
pub fn decide(active: Option<ActivePlan>, obs: Observation) -> Result<Decision, LifecycleError> {
    use ActivePlan::*;
    use Observation::*;

    match (active, obs) {
        // ----- No active plan -----
        (None, PlanRegistered { body, head, .. }) => {
            let hash = content_hash(&body);
            Ok(Decision {
                new_active: Some(Planning {
                    base_commit: head.clone(),
                    latest_plan_hash: hash,
                }),
                effects: vec![Effect::StartPlan {
                    base_commit: head,
                    initial_body: body,
                }],
            })
        }
        (None, PlanFileObserved { .. }) => Err(LifecycleError::NoActivePlan),
        (None, CommitObserved { .. }) => Err(LifecycleError::NoActivePlan),
        (None, ArchiveRequested) => Ok(Decision::noop(None)),
        (None, FinishRequested) => Ok(Decision::noop(None)),

        // ----- Planning -----
        (Some(planning @ Planning { .. }), PlanRegistered { body, .. }) => {
            let new_hash = content_hash(&body);
            if &new_hash == planning.latest_plan_hash() {
                Ok(Decision::noop(Some(planning)))
            } else {
                let base = planning.base_commit().clone();
                Ok(Decision {
                    new_active: Some(Planning {
                        base_commit: base,
                        latest_plan_hash: new_hash,
                    }),
                    effects: vec![Effect::RecordPlanRevision { body }],
                })
            }
        }
        (Some(planning @ Planning { .. }), PlanFileObserved { body, .. }) => {
            let new_hash = content_hash(&body);
            if &new_hash == planning.latest_plan_hash() {
                Ok(Decision::noop(Some(planning)))
            } else {
                let base = planning.base_commit().clone();
                Ok(Decision {
                    new_active: Some(Planning {
                        base_commit: base,
                        latest_plan_hash: new_hash,
                    }),
                    effects: vec![Effect::RecordPlanRevision { body }],
                })
            }
        }
        (
            Some(Planning {
                base_commit,
                latest_plan_hash,
            }),
            CommitObserved { commit },
        ) => {
            let new_impl = commit.sha.clone();
            Ok(Decision {
                new_active: Some(Implementing {
                    base_commit,
                    latest_plan_hash,
                    latest_impl_commit: new_impl,
                }),
                effects: vec![Effect::RecordImplementation { commit }],
            })
        }
        (Some(Planning { .. }), ArchiveRequested) => Ok(Decision {
            new_active: None,
            effects: vec![Effect::ArchiveActivePlan],
        }),
        (Some(Planning { .. }), FinishRequested) => Ok(Decision {
            new_active: None,
            effects: vec![Effect::FinishActivePlan],
        }),

        // ----- Implementing -----
        (Some(implementing @ Implementing { .. }), PlanRegistered { body, head, .. }) => {
            let new_hash = content_hash(&body);
            if &new_hash == implementing.latest_plan_hash() {
                Ok(Decision::noop(Some(implementing)))
            } else {
                Ok(Decision {
                    new_active: Some(Planning {
                        base_commit: head.clone(),
                        latest_plan_hash: new_hash,
                    }),
                    effects: vec![
                        Effect::ArchiveActivePlan,
                        Effect::StartPlan {
                            base_commit: head,
                            initial_body: body,
                        },
                    ],
                })
            }
        }
        (Some(implementing @ Implementing { .. }), PlanFileObserved { body, head }) => {
            let new_hash = content_hash(&body);
            if &new_hash == implementing.latest_plan_hash() {
                Ok(Decision::noop(Some(implementing)))
            } else {
                Ok(Decision {
                    new_active: Some(Planning {
                        base_commit: head.clone(),
                        latest_plan_hash: new_hash,
                    }),
                    effects: vec![
                        Effect::ArchiveActivePlan,
                        Effect::StartPlan {
                            base_commit: head,
                            initial_body: body,
                        },
                    ],
                })
            }
        }
        (
            Some(Implementing {
                base_commit,
                latest_plan_hash,
                ..
            }),
            CommitObserved { commit },
        ) => {
            // Always emit the effect; the apply layer is now the
            // idempotency layer (it checks whether the SHA already exists
            // for the plan and either INSERTs or emits an audit-only
            // `head_reset_to_known_sha` event). This is necessary for
            // reset-to-older-SHA: the reducer cannot tell whether `sha`
            // is the latest, an older known, or wholly new.
            let new_impl = commit.sha.clone();
            Ok(Decision {
                new_active: Some(Implementing {
                    base_commit,
                    latest_plan_hash,
                    latest_impl_commit: new_impl,
                }),
                effects: vec![Effect::RecordImplementation { commit }],
            })
        }
        (Some(Implementing { .. }), ArchiveRequested) => Ok(Decision {
            new_active: None,
            effects: vec![Effect::ArchiveActivePlan],
        }),
        (Some(Implementing { .. }), FinishRequested) => Ok(Decision {
            new_active: None,
            effects: vec![Effect::FinishActivePlan],
        }),
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str) -> CommitSnapshot {
        CommitSnapshot {
            sha: CommitSha::from(sha),
            parent_sha: None,
            branch: Some("main".to_string()),
            message: format!("commit {sha}"),
            diff_stat: String::new(),
            worktree_status: Some("clean".to_string()),
            is_head: true,
        }
    }

    // ---------- None ----------

    #[test]
    fn none_plus_register_starts_planning() {
        let d = decide(
            None,
            Observation::PlanRegistered {
                path: PlanFilePath::from("/p"),
                body: "first".into(),
                head: CommitSha::from("abc"),
            },
        )
        .unwrap();
        assert_eq!(
            d.effects,
            vec![Effect::StartPlan {
                base_commit: CommitSha::from("abc"),
                initial_body: "first".into(),
            }]
        );
        assert_eq!(
            d.new_active,
            Some(ActivePlan::Planning {
                base_commit: CommitSha::from("abc"),
                latest_plan_hash: content_hash("first"),
            })
        );
    }

    #[test]
    fn none_plus_file_observed_errors() {
        let err = decide(
            None,
            Observation::PlanFileObserved {
                body: "x".into(),
                head: CommitSha::from("a"),
            },
        )
        .unwrap_err();
        assert_eq!(err, LifecycleError::NoActivePlan);
    }

    #[test]
    fn none_plus_commit_observed_errors() {
        let err = decide(
            None,
            Observation::CommitObserved {
                commit: commit("a"),
            },
        )
        .unwrap_err();
        assert_eq!(err, LifecycleError::NoActivePlan);
    }

    #[test]
    fn none_plus_archive_is_noop() {
        let d = decide(None, Observation::ArchiveRequested).unwrap();
        assert_eq!(d, Decision::noop(None));
    }

    // ---------- Planning ----------

    fn planning(body: &str, base: &str) -> ActivePlan {
        ActivePlan::Planning {
            base_commit: CommitSha::from(base),
            latest_plan_hash: content_hash(body),
        }
    }

    #[test]
    fn planning_plus_register_same_body_is_noop() {
        let p = planning("body", "head1");
        let d = decide(
            Some(p.clone()),
            Observation::PlanRegistered {
                path: PlanFilePath::from("/p"),
                body: "body".into(),
                head: CommitSha::from("head2"),
            },
        )
        .unwrap();
        assert!(d.effects.is_empty());
        assert_eq!(d.new_active, Some(p));
    }

    #[test]
    fn planning_plus_register_changed_body_records_revision_same_lifecycle() {
        let p = planning("body", "head1");
        let d = decide(
            Some(p),
            Observation::PlanRegistered {
                path: PlanFilePath::from("/p"),
                body: "body v2".into(),
                head: CommitSha::from("head2"),
            },
        )
        .unwrap();
        assert_eq!(
            d.effects,
            vec![Effect::RecordPlanRevision {
                body: "body v2".into()
            }]
        );
        assert_eq!(
            d.new_active,
            Some(ActivePlan::Planning {
                base_commit: CommitSha::from("head1"),
                latest_plan_hash: content_hash("body v2"),
            })
        );
    }

    #[test]
    fn planning_plus_file_observed_same_body_is_noop() {
        let p = planning("body", "head1");
        let d = decide(
            Some(p.clone()),
            Observation::PlanFileObserved {
                body: "body".into(),
                head: CommitSha::from("any"),
            },
        )
        .unwrap();
        assert!(d.effects.is_empty());
        assert_eq!(d.new_active, Some(p));
    }

    #[test]
    fn planning_plus_file_observed_changed_body_records_revision() {
        let p = planning("body", "head1");
        let d = decide(
            Some(p),
            Observation::PlanFileObserved {
                body: "body v2".into(),
                head: CommitSha::from("head-now"),
            },
        )
        .unwrap();
        assert_eq!(
            d.effects,
            vec![Effect::RecordPlanRevision {
                body: "body v2".into()
            }]
        );
        assert_eq!(
            d.new_active,
            Some(ActivePlan::Planning {
                base_commit: CommitSha::from("head1"),
                latest_plan_hash: content_hash("body v2"),
            })
        );
    }

    #[test]
    fn planning_plus_commit_transitions_to_implementing() {
        let p = planning("body", "head1");
        let c = commit("c1");
        let d = decide(Some(p), Observation::CommitObserved { commit: c.clone() }).unwrap();
        assert_eq!(d.effects, vec![Effect::RecordImplementation { commit: c }]);
        assert_eq!(
            d.new_active,
            Some(ActivePlan::Implementing {
                base_commit: CommitSha::from("head1"),
                latest_plan_hash: content_hash("body"),
                latest_impl_commit: CommitSha::from("c1"),
            })
        );
    }

    #[test]
    fn planning_plus_archive_returns_none() {
        let p = planning("body", "head1");
        let d = decide(Some(p), Observation::ArchiveRequested).unwrap();
        assert_eq!(d.effects, vec![Effect::ArchiveActivePlan]);
        assert_eq!(d.new_active, None);
    }

    // ---------- Implementing ----------

    fn implementing(body: &str, base: &str, impl_sha: &str) -> ActivePlan {
        ActivePlan::Implementing {
            base_commit: CommitSha::from(base),
            latest_plan_hash: content_hash(body),
            latest_impl_commit: CommitSha::from(impl_sha),
        }
    }

    #[test]
    fn implementing_plus_register_same_body_is_noop() {
        let p = implementing("body", "base", "impl1");
        let d = decide(
            Some(p.clone()),
            Observation::PlanRegistered {
                path: PlanFilePath::from("/p"),
                body: "body".into(),
                head: CommitSha::from("any"),
            },
        )
        .unwrap();
        assert!(d.effects.is_empty());
        assert_eq!(d.new_active, Some(p));
    }

    #[test]
    fn implementing_plus_register_changed_body_archives_and_starts_new() {
        let p = implementing("body", "base", "impl1");
        let d = decide(
            Some(p),
            Observation::PlanRegistered {
                path: PlanFilePath::from("/p"),
                body: "wholly new task".into(),
                head: CommitSha::from("head-now"),
            },
        )
        .unwrap();
        assert_eq!(
            d.effects,
            vec![
                Effect::ArchiveActivePlan,
                Effect::StartPlan {
                    base_commit: CommitSha::from("head-now"),
                    initial_body: "wholly new task".into(),
                }
            ]
        );
        assert_eq!(
            d.new_active,
            Some(ActivePlan::Planning {
                base_commit: CommitSha::from("head-now"),
                latest_plan_hash: content_hash("wholly new task"),
            })
        );
    }

    #[test]
    fn implementing_plus_file_observed_same_body_is_noop() {
        let p = implementing("body", "base", "impl1");
        let d = decide(
            Some(p.clone()),
            Observation::PlanFileObserved {
                body: "body".into(),
                head: CommitSha::from("any"),
            },
        )
        .unwrap();
        assert!(d.effects.is_empty());
        assert_eq!(d.new_active, Some(p));
    }

    #[test]
    fn implementing_plus_file_observed_changed_body_archives_and_starts_new() {
        let p = implementing("body", "base", "impl1");
        let d = decide(
            Some(p),
            Observation::PlanFileObserved {
                body: "different".into(),
                head: CommitSha::from("head-now"),
            },
        )
        .unwrap();
        assert_eq!(
            d.effects,
            vec![
                Effect::ArchiveActivePlan,
                Effect::StartPlan {
                    base_commit: CommitSha::from("head-now"),
                    initial_body: "different".into(),
                }
            ]
        );
        assert!(matches!(d.new_active, Some(ActivePlan::Planning { .. })));
    }

    #[test]
    fn implementing_plus_commit_same_sha_still_emits_effect_for_apply_layer() {
        let p = implementing("body", "base", "abc");
        let c = commit("abc");
        let d = decide(Some(p), Observation::CommitObserved { commit: c.clone() }).unwrap();
        assert_eq!(
            d.effects,
            vec![Effect::RecordImplementation { commit: c }],
            "reducer always emits the effect; apply layer is the idempotency layer (SHA-exists check + head_reset_to_known_sha audit event)"
        );
    }

    #[test]
    fn implementing_plus_commit_new_sha_records_implementation() {
        let p = implementing("body", "base", "abc");
        let c = commit("def");
        let d = decide(Some(p), Observation::CommitObserved { commit: c.clone() }).unwrap();
        assert_eq!(d.effects, vec![Effect::RecordImplementation { commit: c }]);
        assert_eq!(
            d.new_active,
            Some(ActivePlan::Implementing {
                base_commit: CommitSha::from("base"),
                latest_plan_hash: content_hash("body"),
                latest_impl_commit: CommitSha::from("def"),
            })
        );
    }

    #[test]
    fn implementing_plus_archive_returns_none() {
        let p = implementing("body", "base", "abc");
        let d = decide(Some(p), Observation::ArchiveRequested).unwrap();
        assert_eq!(d.effects, vec![Effect::ArchiveActivePlan]);
        assert_eq!(d.new_active, None);
    }

    // ---------- Round-trip narrative ----------

    #[test]
    fn round_trip_full_lifecycle() {
        let mut state: Option<ActivePlan> = None;
        let mut effects_log: Vec<Effect> = Vec::new();

        let d = decide(
            state.clone(),
            Observation::PlanRegistered {
                path: PlanFilePath::from("/p.md"),
                body: "draft v1".into(),
                head: CommitSha::from("head-a"),
            },
        )
        .unwrap();
        effects_log.extend(d.effects.clone());
        state = d.new_active;
        assert!(matches!(state, Some(ActivePlan::Planning { .. })));

        let d = decide(
            state.clone(),
            Observation::PlanFileObserved {
                body: "draft v2".into(),
                head: CommitSha::from("head-a"),
            },
        )
        .unwrap();
        effects_log.extend(d.effects.clone());
        state = d.new_active;
        assert!(matches!(state, Some(ActivePlan::Planning { .. })));

        let c1 = commit("impl1");
        let d = decide(
            state.clone(),
            Observation::CommitObserved { commit: c1.clone() },
        )
        .unwrap();
        effects_log.extend(d.effects.clone());
        state = d.new_active;
        assert!(matches!(state, Some(ActivePlan::Implementing { .. })));

        let c2 = commit("impl2");
        let d = decide(
            state.clone(),
            Observation::CommitObserved { commit: c2.clone() },
        )
        .unwrap();
        effects_log.extend(d.effects.clone());
        state = d.new_active;
        assert!(matches!(state, Some(ActivePlan::Implementing { .. })));

        let d = decide(
            state.clone(),
            Observation::CommitObserved { commit: c2.clone() },
        )
        .unwrap();
        // Same-SHA re-observation now emits an effect at the reducer
        // level; the apply layer is responsible for turning that into an
        // audit-only `head_reset_to_known_sha` event when the SHA is
        // already present in implementation_revisions.
        effects_log.extend(d.effects.clone());
        state = d.new_active;

        let d = decide(
            state.clone(),
            Observation::PlanFileObserved {
                body: "post-impl new task".into(),
                head: CommitSha::from("head-b"),
            },
        )
        .unwrap();
        effects_log.extend(d.effects.clone());
        state = d.new_active;
        assert!(matches!(state, Some(ActivePlan::Planning { .. })));

        let d = decide(state.clone(), Observation::ArchiveRequested).unwrap();
        effects_log.extend(d.effects.clone());
        state = d.new_active;
        assert_eq!(state, None);

        assert_eq!(
            effects_log,
            vec![
                Effect::StartPlan {
                    base_commit: CommitSha::from("head-a"),
                    initial_body: "draft v1".into(),
                },
                Effect::RecordPlanRevision {
                    body: "draft v2".into()
                },
                Effect::RecordImplementation { commit: c1 },
                Effect::RecordImplementation { commit: c2.clone() },
                Effect::RecordImplementation { commit: c2 },
                Effect::ArchiveActivePlan,
                Effect::StartPlan {
                    base_commit: CommitSha::from("head-b"),
                    initial_body: "post-impl new task".into(),
                },
                Effect::ArchiveActivePlan,
            ]
        );
    }
}
