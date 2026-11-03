use anyhow::Result;
use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // v1
    "
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

    CREATE TABLE tokens (
        id TEXT PRIMARY KEY,
        hash TEXT NOT NULL UNIQUE,
        label TEXT NOT NULL,
        scopes TEXT NOT NULL,
        created_at TEXT NOT NULL,
        last_used_at TEXT,
        revoked INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE projects (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        path TEXT NOT NULL UNIQUE,
        is_git INTEGER NOT NULL,
        worktrees_default INTEGER,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );

    CREATE TABLE threads (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        title TEXT NOT NULL,
        provider_kind TEXT NOT NULL,
        provider_instance TEXT NOT NULL,
        model TEXT,
        permission_mode TEXT NOT NULL,
        status TEXT NOT NULL,
        worktree_path TEXT,
        worktree_branch TEXT,
        cwd TEXT NOT NULL,
        provider_session_id TEXT,
        pinned INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        last_seq INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX threads_project ON threads(project_id, updated_at);

    CREATE TABLE events (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        turn_id TEXT,
        at TEXT NOT NULL,
        kind TEXT NOT NULL,
        payload TEXT NOT NULL
    );
    CREATE INDEX events_thread ON events(thread_id, seq);

    CREATE TABLE approvals (
        id TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        turn_id TEXT NOT NULL,
        payload TEXT NOT NULL,
        resolved INTEGER NOT NULL DEFAULT 0,
        decision TEXT,
        created_at TEXT NOT NULL
    );
    CREATE INDEX approvals_pending ON approvals(resolved, thread_id);

    CREATE TABLE turn_usage (
        turn_id TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL,
        provider_kind TEXT NOT NULL,
        model TEXT,
        input_tokens INTEGER NOT NULL,
        output_tokens INTEGER NOT NULL,
        cache_read_tokens INTEGER NOT NULL,
        cache_write_tokens INTEGER NOT NULL,
        cost_usd REAL,
        duration_ms INTEGER NOT NULL,
        at TEXT NOT NULL
    );

    CREATE TABLE device_push_tokens (
        token TEXT PRIMARY KEY,
        platform TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    ",
    // v2: git checkpoints per turn
    "
    CREATE TABLE checkpoints (
        turn_id TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        before_commit TEXT NOT NULL,
        after_commit TEXT,
        created_at TEXT NOT NULL
    );
    CREATE INDEX checkpoints_thread ON checkpoints(thread_id, created_at);
    ",
    // v3: provider anchors for conversation rewind
    "
    ALTER TABLE checkpoints ADD COLUMN provider_turn_id TEXT;
    ALTER TABLE checkpoints ADD COLUMN provider_turn_end TEXT;
    ",
    // v4: uploaded assets
    "
    CREATE TABLE assets (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        media_type TEXT NOT NULL,
        size INTEGER NOT NULL,
        created_at TEXT NOT NULL
    );
    ",
    // v5: per-thread reasoning effort
    "
    ALTER TABLE threads ADD COLUMN effort TEXT;
    ",
    // v6: durable follow-ups, including consumed/canceled receipts for retry deduplication.
    "
    CREATE TABLE queued_messages (
        id TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        payload TEXT NOT NULL,
        seq INTEGER NOT NULL,
        pending INTEGER NOT NULL DEFAULT 1
    );
    CREATE INDEX queue_pending ON queued_messages(pending, seq);
    ",
    // v7: synchronized user notes and durable steering receipts.
    "
    CREATE TABLE thread_notes (
        thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
        text TEXT NOT NULL,
        revision INTEGER NOT NULL
    );
    CREATE TABLE steered_messages (
        id TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        turn_id TEXT NOT NULL,
        payload TEXT NOT NULL
    );
    ",
    // v8: page native Artifact calls without hydrating the conversation.
    "
    CREATE INDEX artifact_calls ON events(thread_id, seq DESC)
      WHERE kind = 'tool_call_started' AND json_extract(payload, '$.call.name') = 'Artifact';
    CREATE INDEX tool_completion_lookup ON events(thread_id, json_extract(payload, '$.tool_call_id'))
      WHERE kind = 'tool_call_completed';
    ",
    // v9: daemon-owned collaboration through the lasting-context phase.
    "
    CREATE TABLE collaboration_groups (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        coordinator_thread_id TEXT NOT NULL REFERENCES threads(id),
        status TEXT NOT NULL,
        payload TEXT NOT NULL
    );
    CREATE INDEX collaboration_groups_project ON collaboration_groups(project_id, status);

    CREATE TABLE collaboration_members (
        group_id TEXT NOT NULL REFERENCES collaboration_groups(id) ON DELETE CASCADE,
        thread_id TEXT NOT NULL REFERENCES threads(id),
        active INTEGER NOT NULL,
        payload TEXT NOT NULL,
        PRIMARY KEY(group_id, thread_id)
    );
    CREATE UNIQUE INDEX collaboration_one_active_group_per_thread ON collaboration_members(thread_id) WHERE active = 1;

    CREATE TABLE collaboration_assignments (
        id TEXT PRIMARY KEY,
        group_id TEXT NOT NULL REFERENCES collaboration_groups(id) ON DELETE CASCADE,
        owner_thread_id TEXT REFERENCES threads(id),
        status TEXT NOT NULL,
        kind TEXT NOT NULL,
        payload TEXT NOT NULL
    );
    CREATE INDEX collaboration_assignments_group ON collaboration_assignments(group_id, status);
    CREATE UNIQUE INDEX collaboration_one_mutating_assignment_per_worker
      ON collaboration_assignments(owner_thread_id)
      WHERE owner_thread_id IS NOT NULL AND kind IN ('edit', 'integration') AND status IN ('pending', 'working', 'waiting', 'blocked');

    CREATE TABLE collaboration_messages (
        id TEXT PRIMARY KEY,
        operation_id TEXT NOT NULL UNIQUE,
        group_id TEXT NOT NULL REFERENCES collaboration_groups(id) ON DELETE CASCADE,
        assignment_id TEXT REFERENCES collaboration_assignments(id),
        from_thread_id TEXT REFERENCES threads(id),
        to_thread_id TEXT NOT NULL REFERENCES threads(id),
        state TEXT NOT NULL,
        delivery_message_id TEXT UNIQUE,
        payload TEXT NOT NULL
    );
    CREATE INDEX collaboration_messages_recipient ON collaboration_messages(to_thread_id, state);
    CREATE INDEX collaboration_messages_group ON collaboration_messages(group_id, id);

    CREATE TABLE collaboration_context_revisions (
        entry_id TEXT NOT NULL,
        group_id TEXT NOT NULL REFERENCES collaboration_groups(id) ON DELETE CASCADE,
        key TEXT NOT NULL,
        revision INTEGER NOT NULL,
        payload TEXT NOT NULL,
        PRIMARY KEY(entry_id, revision),
        UNIQUE(group_id, key, revision)
    );
    CREATE INDEX collaboration_context_latest ON collaboration_context_revisions(group_id, key, revision DESC);

    CREATE TABLE collaboration_operations (
        operation_id TEXT PRIMARY KEY,
        actor TEXT NOT NULL,
        operation_kind TEXT NOT NULL,
        request TEXT NOT NULL,
        state TEXT NOT NULL,
        response TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    ",
    // v10: chat-first thread relationships and one persistent coordinator per project.
    "
    ALTER TABLE threads ADD COLUMN parent_thread_id TEXT REFERENCES threads(id);
    ALTER TABLE threads ADD COLUMN coordinator_project_id TEXT REFERENCES projects(id);
    ALTER TABLE threads ADD COLUMN collaboration_group_id TEXT;
    CREATE UNIQUE INDEX threads_one_project_coordinator ON threads(coordinator_project_id)
      WHERE coordinator_project_id IS NOT NULL;
    CREATE TABLE project_coordinators (
        project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
        thread_id TEXT NOT NULL UNIQUE REFERENCES threads(id) ON DELETE CASCADE,
        group_id TEXT NOT NULL UNIQUE REFERENCES collaboration_groups(id) ON DELETE CASCADE
    );
    ",
    // v11: reserve deterministic coordinator identities before filesystem or thread side effects.
    "
    CREATE TABLE project_coordinator_reservations (
        project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
        operation_id TEXT NOT NULL UNIQUE,
        request TEXT NOT NULL,
        thread_id TEXT NOT NULL UNIQUE,
        group_id TEXT NOT NULL UNIQUE,
        created_at TEXT NOT NULL
    );
    ",
    // v12: retry-safe client sends resolve a message id without scanning long-lived history.
    "
    CREATE UNIQUE INDEX events_turn_started_message_id
      ON events(json_extract(payload, '$.message_id'))
      WHERE kind = 'turn_started';
    ",
    // v13: notes across global, project and thread scopes. No foreign keys: a note
    // outlives its project or thread (it moves to Recently deleted instead). Thread
    // notes from the old per-thread notepad are copied over; `thread_notes` stays
    // behind untouched.
    "
    CREATE TABLE notes (
        id TEXT PRIMARY KEY,
        scope TEXT NOT NULL,
        project_id TEXT,
        thread_id TEXT UNIQUE,
        title TEXT NOT NULL DEFAULT '',
        body TEXT NOT NULL DEFAULT '',
        pinned INTEGER NOT NULL DEFAULT 0,
        revision INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        deleted_at TEXT,
        origin TEXT
    );
    CREATE INDEX notes_deleted ON notes(deleted_at);
    CREATE INDEX notes_project ON notes(project_id);
    INSERT INTO notes(id, scope, project_id, thread_id, title, body, pinned, revision, created_at, updated_at)
    SELECT
        lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' || substr(lower(hex(randomblob(2))), 2) || '-'
            || substr('89ab', abs(random()) % 4 + 1, 1) || substr(lower(hex(randomblob(2))), 2) || '-' || lower(hex(randomblob(6))),
        'thread', t.project_id, n.thread_id, t.title, n.text, 0, n.revision,
        COALESCE((SELECT MIN(at) FROM events WHERE thread_id = n.thread_id AND kind = 'thread_notes_updated'), t.created_at),
        COALESCE((SELECT MAX(at) FROM events WHERE thread_id = n.thread_id AND kind = 'thread_notes_updated'), t.updated_at)
    FROM thread_notes n JOIN threads t ON t.id = n.thread_id
    WHERE trim(n.text) != '';
    ",
    // v14: user tasks. No foreign key to projects or notes: a task outlives its
    // project (it is soft deleted with it) and a note link is only a reference.
    // `task_counters` hands out each key number once, so a key is never reused even
    // after its task is purged. Projects get a `task_prefix`; existing projects are
    // backfilled by the store when it opens (the derivation lives in Rust).
    "
    ALTER TABLE projects ADD COLUMN task_prefix TEXT;
    CREATE UNIQUE INDEX projects_task_prefix ON projects(task_prefix) WHERE task_prefix IS NOT NULL;
    CREATE TABLE task_counters (
        prefix TEXT PRIMARY KEY,
        next INTEGER NOT NULL
    );
    CREATE TABLE task_items (
        id TEXT PRIMARY KEY,
        key TEXT NOT NULL UNIQUE,
        project_id TEXT,
        title TEXT NOT NULL,
        body TEXT NOT NULL DEFAULT '',
        status TEXT NOT NULL,
        priority INTEGER NOT NULL DEFAULT 0,
        rank REAL NOT NULL,
        source_note_id TEXT,
        pending_followup TEXT,
        revision INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        status_changed_at TEXT NOT NULL,
        deleted_at TEXT
    );
    CREATE INDEX task_items_status ON task_items(status, rank);
    CREATE INDEX task_items_project ON task_items(project_id);
    CREATE INDEX task_items_deleted ON task_items(deleted_at);
    CREATE TABLE task_runs (
        task_id TEXT NOT NULL REFERENCES task_items(id) ON DELETE CASCADE,
        number INTEGER NOT NULL,
        thread_id TEXT NOT NULL UNIQUE,
        provider_kind TEXT NOT NULL,
        provider_instance TEXT NOT NULL,
        model TEXT,
        started_at TEXT NOT NULL,
        ended_at TEXT,
        state TEXT NOT NULL,
        activity TEXT,
        diff_added INTEGER,
        diff_removed INTEGER,
        diff_files INTEGER,
        notes TEXT NOT NULL DEFAULT '[]',
        PRIMARY KEY (task_id, number)
    );
    CREATE TABLE task_item_notes (
        task_id TEXT NOT NULL REFERENCES task_items(id) ON DELETE CASCADE,
        note_id TEXT NOT NULL,
        position INTEGER NOT NULL,
        PRIMARY KEY (task_id, note_id)
    );
    ",
    // v15: notes and tasks an agent created through its native tools remember the
    // thread that made them. Items the user creates leave it empty. No foreign key:
    // the item outlives the thread.
    "
    ALTER TABLE notes ADD COLUMN created_by_thread_id TEXT;
    ALTER TABLE task_items ADD COLUMN created_by_thread_id TEXT;
    ",
    // v16: a combined run is one thread that several tasks each record a run for,
    // so a thread id no longer identifies one run. SQLite cannot drop a UNIQUE
    // constraint, so the table is rebuilt (in one transaction) with a plain index.
    "
    BEGIN;
    CREATE TABLE task_runs_v16 (
        task_id TEXT NOT NULL REFERENCES task_items(id) ON DELETE CASCADE,
        number INTEGER NOT NULL,
        thread_id TEXT NOT NULL,
        provider_kind TEXT NOT NULL,
        provider_instance TEXT NOT NULL,
        model TEXT,
        started_at TEXT NOT NULL,
        ended_at TEXT,
        state TEXT NOT NULL,
        activity TEXT,
        diff_added INTEGER,
        diff_removed INTEGER,
        diff_files INTEGER,
        notes TEXT NOT NULL DEFAULT '[]',
        PRIMARY KEY (task_id, number)
    );
    INSERT INTO task_runs_v16(task_id, number, thread_id, provider_kind, provider_instance, model, started_at, ended_at, state,
                              activity, diff_added, diff_removed, diff_files, notes)
    SELECT task_id, number, thread_id, provider_kind, provider_instance, model, started_at, ended_at, state,
           activity, diff_added, diff_removed, diff_files, notes
    FROM task_runs ORDER BY rowid;
    DROP TABLE task_runs;
    ALTER TABLE task_runs_v16 RENAME TO task_runs;
    CREATE INDEX task_runs_thread ON task_runs(thread_id);
    COMMIT;
    ",
    // v17: read-only child threads that mirror provider-native subagents. The
    // JSON `subagent` column marks them and carries their lifecycle; one
    // runtime task of one session maps to exactly one child thread.
    "
    ALTER TABLE threads ADD COLUMN subagent TEXT;
    CREATE UNIQUE INDEX threads_subagent_task
      ON threads(json_extract(subagent, '$.root_thread_id'), json_extract(subagent, '$.task_id'))
      WHERE subagent IS NOT NULL;
    CREATE INDEX threads_parent ON threads(parent_thread_id) WHERE parent_thread_id IS NOT NULL;
    ",
    // v18: Orchestrator V2. Threads another thread delegated work to carry a JSON
    // `delegation` (one delegation per operation and per task id), and messages
    // between threads get their own table so held ones survive restarts.
    "
    ALTER TABLE threads ADD COLUMN delegation TEXT;
    CREATE UNIQUE INDEX threads_delegation_operation
      ON threads(json_extract(delegation, '$.operation_id'))
      WHERE delegation IS NOT NULL;
    CREATE UNIQUE INDEX threads_delegation_task
      ON threads(json_extract(delegation, '$.task_id'))
      WHERE delegation IS NOT NULL;
    CREATE TABLE thread_messages(
        id TEXT PRIMARY KEY,
        operation_id TEXT NOT NULL UNIQUE,
        from_thread_id TEXT,
        to_thread_id TEXT NOT NULL,
        purpose TEXT NOT NULL,
        reply_to TEXT,
        body TEXT NOT NULL,
        delivery TEXT NOT NULL,
        state TEXT NOT NULL,
        held_reason TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX thread_messages_to ON thread_messages(to_thread_id, state);
    CREATE INDEX thread_messages_from ON thread_messages(from_thread_id);
    CREATE INDEX thread_messages_reply ON thread_messages(reply_to);
    ",
    // v19: durable inline visual source, independent of a thread's workspace.
    "CREATE TABLE html_visuals (
       id TEXT PRIMARY KEY,
       thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
       title TEXT NOT NULL,
       height INTEGER NOT NULL,
       html TEXT NOT NULL
     ); CREATE INDEX html_visuals_thread ON html_visuals(thread_id);",
    // v20: bounded, indexed identity lookup for preview-result persistence.
    "CREATE INDEX tool_start_identity ON events(thread_id, json_extract(payload, '$.call.id'), seq)
      WHERE kind = 'tool_call_started';",
    // v21: durable native child inbox, bound to the admitted process.
    "CREATE TABLE subagent_messages(id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, session_instance_id TEXT NOT NULL,
      task_id TEXT NOT NULL, status TEXT NOT NULL, record TEXT NOT NULL);
      CREATE INDEX subagent_messages_child ON subagent_messages(thread_id);
      CREATE INDEX subagent_messages_pending ON subagent_messages(session_instance_id, task_id, status);",
    // v22: historical answer overlays must not scan large unrelated event logs.
    "CREATE INDEX assistant_recovery_identity ON events(thread_id, json_extract(payload, '$.message_id'), seq)
      WHERE kind = 'assistant_message_blocks_recovered';",
    // v23: lifecycle settlement queries only pending messages, independently of UI history.
    "CREATE INDEX subagent_messages_pending_child ON subagent_messages(thread_id) WHERE status = 'pending';
      CREATE INDEX subagent_messages_pending_owner ON subagent_messages(session_instance_id) WHERE status = 'pending';
      CREATE INDEX subagent_messages_pending_status ON subagent_messages(status) WHERE status = 'pending';",
    // v24: the Coordinator becomes an ordinary lead thread on Orchestrator V2. Project
    // notes gain a role (the user's brief, the Coordinator's setup note), projects get
    // a switch to share the brief with every thread, and the collaboration system is
    // removed. Each project's latest `project.brief` becomes its pinned brief note and
    // its `project.setup` the setup note; the old Coordinator's overview and setup log
    // become ordinary memory notes credited to it;
    // old coordinators and their workers are archived (start fresh); old
    // collaboration events are deleted because they can no longer be decoded. The operation
    // receipts of the agents' note and task tools survive as `tool_operations`.
    "
    BEGIN;
    ALTER TABLE notes ADD COLUMN project_role TEXT;
    CREATE UNIQUE INDEX notes_project_role ON notes(project_id, project_role) WHERE project_role IS NOT NULL;
    ALTER TABLE projects ADD COLUMN brief_in_all_threads INTEGER NOT NULL DEFAULT 0;
    INSERT INTO notes(id, scope, project_id, title, body, pinned, revision, created_at, updated_at, project_role)
    SELECT
        lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' || substr(lower(hex(randomblob(2))), 2) || '-'
            || substr('89ab', abs(random()) % 4 + 1, 1) || substr(lower(hex(randomblob(2))), 2) || '-' || lower(hex(randomblob(6))),
        'project', pc.project_id, 'Project brief', e.body, 1, 1, e.at, e.at, 'brief'
    FROM project_coordinators pc
    JOIN (
        SELECT group_id,
               json_extract(payload, '$.body') AS body,
               COALESCE(json_extract(payload, '$.updated_at'), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')) AS at,
               ROW_NUMBER() OVER (PARTITION BY group_id ORDER BY revision DESC) AS rn
        FROM collaboration_context_revisions
        WHERE key = 'project.brief'
    ) e ON e.group_id = pc.group_id AND e.rn = 1
    WHERE trim(COALESCE(e.body, '')) != '';
    INSERT INTO notes(id, scope, project_id, title, body, pinned, revision, created_at, updated_at, project_role, created_by_thread_id)
    SELECT
        lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' || substr(lower(hex(randomblob(2))), 2) || '-'
            || substr('89ab', abs(random()) % 4 + 1, 1) || substr(lower(hex(randomblob(2))), 2) || '-' || lower(hex(randomblob(6))),
        'project', pc.project_id,
        CASE e.key WHEN 'project.setup' THEN 'How to build, run and test' WHEN 'project-overview' THEN 'Project overview' ELSE 'Coordinator setup log' END,
        e.body, e.key = 'project.setup', 1, e.at, e.at,
        CASE e.key WHEN 'project.setup' THEN 'setup' END,
        pc.thread_id
    FROM project_coordinators pc
    JOIN (
        SELECT group_id, key,
               json_extract(payload, '$.body') AS body,
               COALESCE(json_extract(payload, '$.updated_at'), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')) AS at,
               ROW_NUMBER() OVER (PARTITION BY group_id, key ORDER BY revision DESC) AS rn
        FROM collaboration_context_revisions
        WHERE key IN ('project.setup', 'project-overview', 'coordinator-setup')
    ) e ON e.group_id = pc.group_id AND e.rn = 1
    WHERE trim(COALESCE(e.body, '')) != '';
    UPDATE queued_messages SET pending = 0
      WHERE thread_id IN (SELECT id FROM threads WHERE coordinator_project_id IS NOT NULL OR collaboration_group_id IS NOT NULL);
    UPDATE threads SET status = 'archived', coordinator_project_id = NULL
      WHERE coordinator_project_id IS NOT NULL OR collaboration_group_id IS NOT NULL;
    DELETE FROM events WHERE kind IN ('project_coordinator_deleted', 'collaboration_group_updated', 'collaboration_member_updated',
      'collaboration_assignment_updated', 'collaboration_message_updated', 'collaboration_context_updated');
    DELETE FROM collaboration_operations WHERE substr(operation_kind, 1, 7) != 'kybern_';
    ALTER TABLE collaboration_operations RENAME TO tool_operations;
    DROP TABLE collaboration_context_revisions;
    DROP TABLE collaboration_messages;
    DROP TABLE collaboration_assignments;
    DROP TABLE collaboration_members;
    DROP TABLE project_coordinator_reservations;
    DROP TABLE project_coordinators;
    DROP TABLE collaboration_groups;
    ALTER TABLE threads DROP COLUMN collaboration_group_id;
    COMMIT;
    ",
    // v25: a project's Coordinator settings overrides (JSON of `CoordinatorOverrides`).
    "ALTER TABLE projects ADD COLUMN coordinator TEXT;",
    // v26: the thread-list activity projection reads sparse lifecycle metadata,
    // not every message/tool payload in each conversation. Include root-session
    // bindings: they suppress providers that reported their root as a child.
    "CREATE INDEX events_runtime_tasks ON events(thread_id, seq)
      WHERE kind IN ('runtime_task_started', 'runtime_task_updated', 'runtime_task_completed', 'provider_session_bound');",
    // v27: spaces. Every project except free chat belongs to one; the migration
    // puts existing projects in "Personal".
    "
    BEGIN;
    CREATE TABLE spaces (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        color TEXT NOT NULL,
        -- JSON of `SpaceLogo`.
        logo TEXT NOT NULL,
        position INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    INSERT INTO spaces(id, name, color, logo, position, created_at, updated_at)
    SELECT lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' ||
           substr(lower(hex(randomblob(2))), 2) || '-' ||
           substr('89ab', 1 + (abs(random()) % 4), 1) || substr(lower(hex(randomblob(2))), 2) || '-' ||
           lower(hex(randomblob(6))),
           'Personal', 'green', '{\"kind\":\"initials\"}', 0,
           strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), strftime('%Y-%m-%dT%H:%M:%SZ', 'now');
    ALTER TABLE projects ADD COLUMN space_id TEXT REFERENCES spaces(id);
    UPDATE projects SET space_id = (SELECT id FROM spaces LIMIT 1)
      WHERE id != '00000000-0000-0000-0000-000000000001';
    CREATE INDEX projects_space ON projects(space_id);
    COMMIT;
    ",
    // A thread note follows its thread's title until someone renames the note itself.
    "ALTER TABLE notes ADD COLUMN title_edited INTEGER NOT NULL DEFAULT 0;",
    // The default space cannot be deleted. v27 seeded "Personal" first, so the
    // oldest space is the seeded one while it exists (even renamed) and the
    // oldest survivor once it is gone. The partial index keeps it to one.
    "
    BEGIN;
    ALTER TABLE spaces ADD COLUMN is_default INTEGER NOT NULL DEFAULT 0;
    UPDATE spaces SET is_default = 1
      WHERE id = (SELECT id FROM spaces ORDER BY created_at, position, id LIMIT 1);
    CREATE UNIQUE INDEX spaces_one_default ON spaces(is_default) WHERE is_default = 1;
    COMMIT;
    ",
    // v30: group projects. A project of kind 'group' is a parent folder whose members
    // are git repositories inside it, stored as '/'-separated paths relative to
    // projects.path ('.' is the folder itself). Position 0 is the primary member.
    "
    BEGIN;
    ALTER TABLE projects ADD COLUMN kind TEXT NOT NULL DEFAULT 'single';
    ALTER TABLE projects ADD COLUMN instructions_offer_dismissed INTEGER NOT NULL DEFAULT 0;
    -- JSON array of top-level entry names copied into new worktree containers.
    ALTER TABLE projects ADD COLUMN worktree_copy TEXT;
    CREATE TABLE project_members (
        project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        path TEXT NOT NULL,
        position INTEGER NOT NULL,
        -- `MemberStatus` from the last probe, cached like projects.is_git.
        status TEXT NOT NULL DEFAULT 'ok',
        added_at TEXT NOT NULL,
        PRIMARY KEY (project_id, path)
    );
    CREATE INDEX project_members_order ON project_members(project_id, position);
    -- JSON of Vec<ThreadMember>; NULL for single-repo threads.
    ALTER TABLE threads ADD COLUMN members TEXT;
    -- JSON of Vec<RepoCheckpoint>; NULL for single-repo threads.
    ALTER TABLE checkpoints ADD COLUMN repos TEXT;
    ALTER TABLE checkpoints ADD COLUMN incomplete INTEGER NOT NULL DEFAULT 0;
    COMMIT;
    ",
    // v31: notes as history. A note may hold one kind of memory (preference, decision,
    // convention, gotcha). Uncategorized notes written before this migration are
    // "undated": `meta.notes_history_since` is when it ran.
    "
    BEGIN;
    ALTER TABLE notes ADD COLUMN category TEXT
      CHECK (category IS NULL OR category IN ('preference', 'decision', 'convention', 'gotcha'));
    INSERT INTO meta(key, value) VALUES ('notes_history_since', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
      ON CONFLICT(key) DO NOTHING;
    COMMIT;
    ",
    // v32: the machine network. Membership operations are an append-only signed set; `network_machines`
    // caches peer descriptors and presence (rebuilt from the operations and live presence).
    // `projects.repo_identity` is JSON `ProjectRepoIdentity`; `tokens.device_id` ties a device
    // token to the client device so revoking the device revokes it on every machine.
    "
    BEGIN;
    CREATE TABLE network_ops (
      id TEXT PRIMARY KEY,
      body BLOB NOT NULL,
      signer TEXT NOT NULL,
      signature BLOB NOT NULL,
      kind TEXT NOT NULL,
      lamport INTEGER NOT NULL,
      received_at TEXT NOT NULL
    );
    CREATE INDEX network_ops_order ON network_ops(lamport, received_at, id);
    CREATE TABLE network_machines (
      machine_id TEXT PRIMARY KEY,
      descriptor TEXT NOT NULL,
      presence TEXT,
      last_seen_at TEXT
    );
    ALTER TABLE projects ADD COLUMN repo_identity TEXT;
    ALTER TABLE tokens ADD COLUMN device_id TEXT;
    ALTER TABLE tokens ADD COLUMN issued_via TEXT;
    CREATE INDEX tokens_device ON tokens(device_id);
    COMMIT;
    ",
    // Secret store (S3, design §9.3, §4.4 "v35 (M5)"; numbered here in merge order). `secrets` holds
    // named values encrypted with XChaCha20-Poly1305 under the machine's data key: `ciphertext` is
    // the AEAD output, `nonce` the 24-byte nonce, `key_version` the data key that sealed it, and
    // `source` is `local` or `pushed:<machine_id>`. The store only keeps bytes; the daemon encrypts.
    "
    BEGIN;
    CREATE TABLE secrets (
      name TEXT PRIMARY KEY,
      ciphertext BLOB NOT NULL,
      nonce BLOB NOT NULL,
      key_version INTEGER NOT NULL,
      source TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );
    COMMIT;
    ",
    // Remote delegation and the outbox (R1, design §4.4 "v33 (M2)"; numbered here in merge order).
    // `outbox` is the visible queue of work for machines that are offline; a delivery repeats under
    // its `operation_id`, which the target deduplicates. `threads.origin` records which machine
    // started a thread (JSON `Origin`, written once); `threads.inbound` is the column for inbound
    // audit threads. `remote_children` is the delegating machine's row for a child that runs
    // elsewhere, written before anything is sent.
    "
    BEGIN;
    CREATE TABLE outbox (
      id TEXT PRIMARY KEY,
      operation_id TEXT NOT NULL UNIQUE,
      target TEXT NOT NULL,
      kind TEXT NOT NULL,
      payload TEXT NOT NULL,
      origin TEXT,
      state TEXT NOT NULL CHECK (state IN ('queued','sending','delivered','failed','expired','cancelled')),
      attempts INTEGER NOT NULL DEFAULT 0,
      next_attempt_at TEXT,
      last_error TEXT,
      result TEXT,
      created_at TEXT NOT NULL,
      expires_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );
    CREATE INDEX outbox_due ON outbox(state, next_attempt_at);
    ALTER TABLE threads ADD COLUMN origin TEXT;
    ALTER TABLE threads ADD COLUMN inbound TEXT;
    CREATE TABLE remote_children (
      task_id TEXT PRIMARY KEY,
      operation_id TEXT NOT NULL UNIQUE,
      parent_thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
      target TEXT NOT NULL,
      remote_thread_id TEXT,
      status TEXT NOT NULL,
      stop_requested INTEGER NOT NULL DEFAULT 0,
      last_seq INTEGER NOT NULL DEFAULT 0,
      title TEXT NOT NULL DEFAULT '',
      provider TEXT,
      model TEXT,
      info TEXT NOT NULL,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );
    CREATE INDEX remote_children_parent ON remote_children(parent_thread_id);
    CREATE INDEX remote_children_active ON remote_children(status) WHERE status IN ('pending','running');
    COMMIT;
    ",
    // Login broker loans (S4, design §9.4; numbered here in merge order). One row per loan, on both ends:
    // the broker's `lender` rows say whom to push a rotated token to; a borrower's `borrower` rows say which
    // broker it must release from and re-borrow from when the register moves. **No token is ever stored**:
    // the broker re-reads its credential, a borrower borrows again after a restart.
    "
    BEGIN;
    CREATE TABLE broker_loans (
      loan_id TEXT PRIMARY KEY,
      role TEXT NOT NULL CHECK (role IN ('lender','borrower')),
      peer TEXT NOT NULL,
      provider TEXT NOT NULL,
      account TEXT NOT NULL,
      local_instance TEXT,
      generation INTEGER NOT NULL DEFAULT 0,
      expires_at TEXT,
      last_pushed_at TEXT,
      last_error TEXT,
      created_at TEXT NOT NULL,
      released_at TEXT
    );
    CREATE INDEX broker_loans_open ON broker_loans(role, provider) WHERE released_at IS NULL;
    COMMIT;
    ",
    // Continue on another machine (F2, design §7.2, §4.4 "v34 (M3)"; numbered here in merge order).
    // `threads.moved` is JSON `MoveInfo`: the lease while the thread departs and, once the target
    // confirmed, the link to the thread that continues the work. Only the move code writes it
    // (`thread_set_moved`); a thread upsert never touches it.
    "
    BEGIN;
    ALTER TABLE threads ADD COLUMN moved TEXT;
    CREATE INDEX threads_moving ON threads(id) WHERE moved IS NOT NULL;
    COMMIT;
    ",
    // Client token ancestry (machine network review). `tokens.issued_lineage` is a JSON array of every
    // machine a network-minted token's authority passed through before `issued_via`, so removing any of
    // them revokes it, however many hops or self-grants later it was minted.
    "
    BEGIN;
    ALTER TABLE tokens ADD COLUMN issued_lineage TEXT;
    COMMIT;
    ",
    // Spaces replicas are scoped to a network, never written to network_ops.
    "
    CREATE TABLE network_space_fields (
      id TEXT PRIMARY KEY,
      network_id TEXT NOT NULL,
      envelope TEXT NOT NULL
    );
    CREATE INDEX network_space_fields_network ON network_space_fields(network_id);
    CREATE TABLE network_space_logos (
      hash TEXT PRIMARY KEY,
      bytes BLOB NOT NULL
    );
    ",
    // `remote_children.completed_at_from_target`: the end time in `info` is the target's own (the child was followed
    // to its end), not this machine's. Clients compare it with the target's copy of the thread only then.
    "
    ALTER TABLE remote_children ADD COLUMN completed_at_from_target INTEGER NOT NULL DEFAULT 0;
    ",
];

pub fn migrate(conn: &Connection) -> Result<()> {
    migrate_to(conn, MIGRATIONS.len())
}

/// Apply migrations up to `target` (a schema version). Tests use it to build an older database.
pub(crate) fn migrate_to(conn: &Connection, target: usize) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    // A newer Kybern migrated this store. Running older code against it could misread or corrupt rows
    // (a stale deb or an old AppImage next to a newer install shares `~/.kybern`), so refuse to open it.
    if version > MIGRATIONS.len() as i64 {
        anyhow::bail!(
            "this data folder was upgraded by a newer Kybern (schema {version}, this build knows {}). Update Kybern, or open the folder with the newer version",
            MIGRATIONS.len()
        );
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().take(target).skip(version as usize) {
        conn.execute_batch(sql)?;
        conn.pragma_update(None, "user_version", (i + 1) as i64)?;
        tracing::info!(version = i + 1, "applied store migration");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn migration_count() -> usize {
    MIGRATIONS.len()
}
