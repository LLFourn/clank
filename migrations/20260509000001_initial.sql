CREATE TABLE plans (
    id TEXT PRIMARY KEY,
    repo_root TEXT NOT NULL,
    plan_path TEXT,
    display_title TEXT,
    state TEXT NOT NULL CHECK (state IN ('planning', 'plan_approved', 'implementation_review', 'done', 'archived')),
    master_agent_id INTEGER REFERENCES agents(id),
    current_implementation_id INTEGER REFERENCES implementation_revisions(id),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    archived_at INTEGER
);

CREATE TABLE plan_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    revision_number INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    detected_by TEXT NOT NULL CHECK (detected_by IN ('register_plan_file', 'watcher', 'resume_snapshot', 'submit_plan')),
    UNIQUE(plan_id, revision_number)
);

CREATE TABLE implementation_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL,
    parent_sha TEXT,
    branch TEXT,
    commit_message TEXT NOT NULL,
    diff_stat TEXT NOT NULL,
    worktree_status TEXT,
    is_head INTEGER NOT NULL CHECK (is_head IN (0, 1)),
    registered_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    detected_by TEXT NOT NULL CHECK (detected_by IN ('register_implementation_commit', 'ui_from_head')),
    UNIQUE(plan_id, commit_sha)
);

CREATE TABLE agents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('master', 'reviewer')),
    label TEXT NOT NULL,
    model_hint TEXT,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    UNIQUE(plan_id, label)
);

CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    target_kind TEXT CHECK (target_kind IS NULL OR target_kind IN ('plan_revision', 'implementation_commit')),
    target_id TEXT,
    ts INTEGER NOT NULL,
    kind TEXT NOT NULL,
    actor TEXT NOT NULL,
    payload TEXT NOT NULL,
    status TEXT CHECK (status IS NULL OR status IN ('pending', 'staged', 'delivered', 'withdrawn'))
);

CREATE TABLE directive_batches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    target_kind TEXT,
    created_at INTEGER NOT NULL,
    delivered_by TEXT NOT NULL,
    acked_at INTEGER,
    acked_by TEXT
);

CREATE TABLE directive_batch_items (
    batch_id INTEGER NOT NULL REFERENCES directive_batches(id) ON DELETE CASCADE,
    event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    PRIMARY KEY (batch_id, event_id)
);

CREATE INDEX events_plan_id_id ON events(plan_id, id);
CREATE INDEX events_target ON events(plan_id, target_kind, target_id);
CREATE INDEX events_status ON events(plan_id, status) WHERE status IS NOT NULL;
CREATE INDEX revisions_plan_id ON plan_revisions(plan_id, id);
CREATE INDEX impl_revisions_plan_id ON implementation_revisions(plan_id, id);
CREATE INDEX batches_plan_id ON directive_batches(plan_id, id);
CREATE INDEX agents_plan_id ON agents(plan_id);
