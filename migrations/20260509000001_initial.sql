-- v0' schema. See /Users/llfourn/.claude/plans/i-want-to-auto-track-impl-commits.md
-- for the watcher-coordinator model and required invariants.
--
-- This migration is rewritten in place; Trinity is pre-shipped, so there
-- is no forward/backward compatibility burden. Drop the database to
-- pick up schema changes.

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    repo_root TEXT NOT NULL,
    plan_file_path TEXT NOT NULL,
    display_title TEXT,
    base_commit TEXT,
    state TEXT CHECK (state IS NULL OR state IN ('planning', 'implementing', 'finished', 'archived')),
    started_at INTEGER,
    finished_at INTEGER,
    current_event_floor INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    archived_at INTEGER
);

CREATE VIEW plans AS
    SELECT
        rowid AS id,
        id AS session_id,
        base_commit,
        state,
        started_at,
        archived_at,
        finished_at
    FROM sessions
    WHERE state IS NOT NULL;

CREATE TABLE plan_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    revision_number INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(session_id, revision_number)
);
CREATE INDEX plan_revisions_session ON plan_revisions(session_id, id);

CREATE TABLE implementation_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL,
    parent_sha TEXT,
    branch TEXT,
    commit_message TEXT NOT NULL,
    diff_stat TEXT NOT NULL,
    worktree_status TEXT,
    is_head INTEGER NOT NULL CHECK (is_head IN (0, 1)),
    registered_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(session_id, commit_sha)
);
CREATE INDEX impl_revisions_session ON implementation_revisions(session_id, id);

CREATE TABLE agents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    UNIQUE(session_id, label)
);
CREATE INDEX agents_session ON agents(session_id);

CREATE TABLE repo_effective_sessions (
    repo_root TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    claimed_by TEXT NOT NULL,
    claimed_at INTEGER NOT NULL
);
CREATE INDEX repo_effective_sessions_session ON repo_effective_sessions(session_id);

CREATE TABLE feedback (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id   TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    target_kind  TEXT    NOT NULL CHECK (target_kind IN ('plan_revision', 'implementation_commit')),
    target_id    TEXT    NOT NULL,
    author_label TEXT    NOT NULL,
    body         TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE(session_id, target_kind, target_id, author_label)
);
CREATE INDEX feedback_session ON feedback(session_id, id);

CREATE TABLE review_gate_overrides (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    phase TEXT NOT NULL CHECK (phase IN ('plan', 'impl')),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('plan_revision', 'implementation_commit')),
    target_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('changes_requested', 'ready')),
    actor TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, phase)
);
CREATE INDEX review_gate_overrides_session ON review_gate_overrides(session_id);

-- Sidecar for files in `<repo_root>/.trinity/feedback/<session_id>/<plan|impl>/<author_label>.md`.
-- One row per (session, feedback_kind, author). Same author can hold both
-- a `plan` and an `impl` row simultaneously; their states are derived
-- independently.
CREATE TABLE feedback_files (
    session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    feedback_kind TEXT NOT NULL CHECK (feedback_kind IN ('plan','impl')),
    author_label  TEXT NOT NULL,
    path          TEXT NOT NULL,
    last_observed_hash TEXT,
    last_observed_at   INTEGER,
    last_ingested_hash TEXT,
    last_ingested_at   INTEGER,
    last_ingested_target_kind TEXT
        CHECK (last_ingested_target_kind IS NULL
               OR last_ingested_target_kind IN ('plan_revision','implementation_commit')),
    last_ingested_target_id TEXT,
    parse_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, feedback_kind, author_label)
);
CREATE INDEX feedback_files_session ON feedback_files(session_id);

CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    plan_id INTEGER,
    target_kind TEXT CHECK (target_kind IS NULL OR target_kind IN ('plan_revision', 'implementation_commit')),
    target_id TEXT,
    ts INTEGER NOT NULL,
    kind TEXT NOT NULL,
    actor TEXT NOT NULL,
    payload TEXT NOT NULL,
    status TEXT CHECK (status IS NULL OR status IN ('pending', 'staged', 'delivered', 'withdrawn'))
);
CREATE INDEX events_session ON events(session_id, id);
CREATE INDEX events_plan ON events(plan_id, id);
CREATE INDEX events_target ON events(session_id, target_kind, target_id);
