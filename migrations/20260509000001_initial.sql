-- v0' schema. See /Users/llfourn/.claude/plans/i-want-to-create-jolly-hamming.md
-- for the identity model and required invariants.

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,                         -- master-chosen URL-safe slug
    repo_root TEXT NOT NULL,
    plan_file_path TEXT NOT NULL,                -- current watched file
    display_title TEXT,
    master_agent_id INTEGER REFERENCES agents(id),
    active_plan_id INTEGER REFERENCES plans(id), -- NULL when no active lifecycle
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    archived_at INTEGER                          -- session-level archive (column reserved for v1)
);

CREATE TABLE plans (                              -- one plan-to-implementation lifecycle attempt
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    base_commit TEXT NOT NULL,                   -- HEAD at PlanRegistered time
    state TEXT NOT NULL CHECK (state IN ('planning', 'implementing', 'archived')),
    started_at INTEGER NOT NULL,
    archived_at INTEGER
);
CREATE INDEX plans_session ON plans(session_id, id);

-- Belt-and-suspenders: at most one non-archived plan per session.
-- The reducer encodes this rule; this index catches application-layer bugs
-- or concurrent transactions that would violate it. `sessions.active_plan_id`
-- consistency with this index is enforced by code-level checks in the
-- apply layer (see active_plan_invariant tests).
CREATE UNIQUE INDEX one_active_plan_per_session
    ON plans(session_id) WHERE state != 'archived';

CREATE TABLE plan_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id INTEGER NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    revision_number INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(plan_id, revision_number)
);
CREATE INDEX plan_revisions_plan ON plan_revisions(plan_id, id);

CREATE TABLE implementation_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id INTEGER NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL,
    parent_sha TEXT,
    branch TEXT,
    commit_message TEXT NOT NULL,
    diff_stat TEXT NOT NULL,
    worktree_status TEXT,
    is_head INTEGER NOT NULL CHECK (is_head IN (0, 1)),
    registered_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(plan_id, commit_sha)
);
CREATE INDEX impl_revisions_plan ON implementation_revisions(plan_id, id);

CREATE TABLE agents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('master', 'reviewer')),
    label TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    UNIQUE(session_id, label)
);
CREATE INDEX agents_session ON agents(session_id);

CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    plan_id INTEGER REFERENCES plans(id) ON DELETE CASCADE,  -- NULL for session-scoped events
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
CREATE INDEX events_status ON events(session_id, status) WHERE status IS NOT NULL;
CREATE INDEX events_target ON events(session_id, target_kind, target_id);

CREATE TABLE directive_batches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    plan_id INTEGER REFERENCES plans(id),
    target_kind TEXT,
    created_at INTEGER NOT NULL,
    delivered_by TEXT NOT NULL,
    acked_at INTEGER,
    acked_by TEXT
);
CREATE INDEX directive_batches_session ON directive_batches(session_id, id);

CREATE TABLE directive_batch_items (
    batch_id INTEGER NOT NULL REFERENCES directive_batches(id) ON DELETE CASCADE,
    event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    PRIMARY KEY (batch_id, event_id)
);
