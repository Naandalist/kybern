//! The outbox and the remote-children table (design §4.4, §6.2, §6.4).
//!
//! The store only keeps rows. What a delivery does, when it retries and how a child is followed is the
//! daemon's job (`network/outbox.rs`, `orchestrator/remote_delegation.rs`).

use anyhow::Result;
use chrono::{DateTime, Utc};
use kybern_protocol::{OutboxItem, OutboxState, RemoteChild, ThreadId};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::{Store, parse_time};

fn conversion(index: usize, error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(error))
}

fn uuid(r: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&r.get::<_, String>(index)?).map_err(|error| conversion(index, error))
}

fn enum_column<T: serde::de::DeserializeOwned>(r: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(r.get::<_, String>(index)?)).map_err(|error| conversion(index, error))
}

fn json_column<T: serde::de::DeserializeOwned>(r: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<T>> {
    r.get::<_, Option<String>>(index)?.map(|json| serde_json::from_str(&json)).transpose().map_err(|error| conversion(index, error))
}

fn time(r: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    parse_time(r.get::<_, String>(index)?)
}

const OUTBOX_SELECT: &str = "SELECT id, operation_id, target, kind, payload, origin, state, attempts, next_attempt_at, last_error, result,
    created_at, expires_at, updated_at FROM outbox";

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxItem> {
    Ok(OutboxItem {
        id: uuid(r, 0)?,
        operation_id: uuid(r, 1)?,
        target: r.get(2)?,
        kind: enum_column(r, 3)?,
        payload: serde_json::from_str(&r.get::<_, String>(4)?).map_err(|error| conversion(4, error))?,
        origin: json_column(r, 5)?,
        state: enum_column(r, 6)?,
        attempts: r.get(7)?,
        next_attempt_at: r.get::<_, Option<String>>(8)?.map(parse_time).transpose()?,
        last_error: r.get(9)?,
        result: json_column(r, 10)?,
        created_at: time(r, 11)?,
        expires_at: time(r, 12)?,
        updated_at: time(r, 13)?,
    })
}

const CHILD_SELECT: &str = "SELECT task_id, operation_id, parent_thread_id, target, remote_thread_id, status, stop_requested, last_seq,
    title, provider, model, info, created_at, updated_at, completed_at_from_target FROM remote_children";

fn row_to_child(r: &rusqlite::Row<'_>) -> rusqlite::Result<RemoteChild> {
    Ok(RemoteChild {
        task_id: uuid(r, 0)?,
        operation_id: uuid(r, 1)?,
        parent_thread_id: uuid(r, 2)?,
        target: r.get(3)?,
        remote_thread_id: r
            .get::<_, Option<String>>(4)?
            .map(|id| Uuid::parse_str(&id))
            .transpose()
            .map_err(|error| conversion(4, error))?,
        state: enum_column(r, 5)?,
        stop_requested: r.get(6)?,
        last_seq: r.get(7)?,
        title: r.get(8)?,
        provider: r
            .get::<_, Option<String>>(9)?
            .map(|kind| kind.parse())
            .transpose()
            .map_err(|error: String| conversion(9, std::io::Error::other(error)))?,
        model: r.get(10)?,
        info: serde_json::from_str(&r.get::<_, String>(11)?).map_err(|error| conversion(11, error))?,
        created_at: time(r, 12)?,
        updated_at: time(r, 13)?,
        completed_at_from_target: r.get(14)?,
    })
}

fn name<T: serde::Serialize>(value: T) -> Result<String> {
    Ok(serde_json::to_value(value)?.as_str().unwrap_or_default().to_string())
}

impl Store {
    // ---- outbox ----

    /// Queue an item. A repeated `operation_id` returns the first item and writes nothing.
    pub fn outbox_insert(&self, item: &OutboxItem) -> Result<OutboxItem> {
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO outbox(id, operation_id, target, kind, payload, origin, state, attempts, next_attempt_at,
                    last_error, result, created_at, expires_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    item.id.to_string(),
                    item.operation_id.to_string(),
                    item.target,
                    name(item.kind)?,
                    serde_json::to_string(&item.payload)?,
                    item.origin.as_ref().map(serde_json::to_string).transpose()?,
                    name(item.state)?,
                    item.attempts,
                    item.next_attempt_at.map(|at| at.to_rfc3339()),
                    item.last_error,
                    item.result.as_ref().map(serde_json::to_string).transpose()?,
                    item.created_at.to_rfc3339(),
                    item.expires_at.to_rfc3339(),
                    item.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(c.query_row(&format!("{OUTBOX_SELECT} WHERE operation_id = ?1"), [item.operation_id.to_string()], row_to_item)?)
        })
    }

    pub fn outbox_get(&self, id: Uuid) -> Result<Option<OutboxItem>> {
        self.with(|c| Ok(c.query_row(&format!("{OUTBOX_SELECT} WHERE id = ?1"), [id.to_string()], row_to_item).optional()?))
    }

    pub fn outbox_get_by_operation(&self, operation_id: Uuid) -> Result<Option<OutboxItem>> {
        self.with(|c| {
            Ok(c.query_row(&format!("{OUTBOX_SELECT} WHERE operation_id = ?1"), [operation_id.to_string()], row_to_item).optional()?)
        })
    }

    /// Items newest first, for one target and state or all.
    pub fn outbox_list(&self, target: Option<&str>, state: Option<OutboxState>) -> Result<Vec<OutboxItem>> {
        self.with(|c| {
            let state = state.map(name).transpose()?;
            let mut st = c.prepare(&format!(
                "{OUTBOX_SELECT} WHERE (?1 IS NULL OR target = ?1) AND (?2 IS NULL OR state = ?2) ORDER BY created_at DESC, id DESC"
            ))?;
            Ok(st.query_map(params![target, state], row_to_item)?.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Queued items whose next attempt is due and which have not expired, oldest first (FIFO per
    /// target; the worker keeps one in flight per target and kind).
    pub fn outbox_due(&self, now: DateTime<Utc>) -> Result<Vec<OutboxItem>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "{OUTBOX_SELECT} WHERE state = 'queued' AND expires_at > ?1 AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
                 ORDER BY created_at, id"
            ))?;
            Ok(st.query_map([now.to_rfc3339()], row_to_item)?.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Queued or sending items whose time ran out.
    pub fn outbox_overdue(&self, now: DateTime<Utc>) -> Result<Vec<OutboxItem>> {
        self.with(|c| {
            let mut st =
                c.prepare(&format!("{OUTBOX_SELECT} WHERE state IN ('queued','sending') AND expires_at <= ?1 ORDER BY created_at, id"))?;
            Ok(st.query_map([now.to_rfc3339()], row_to_item)?.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// When the soonest queued item is next due (an item with no time is due now).
    pub fn outbox_next_due(&self) -> Result<Option<DateTime<Utc>>> {
        self.with(|c| {
            let next: Option<Option<String>> =
                c.query_row("SELECT MIN(COALESCE(next_attempt_at, '')) FROM outbox WHERE state = 'queued'", [], |r| r.get(0)).optional()?;
            Ok(match next.flatten() {
                Some(text) if text.is_empty() => Some(Utc::now()),
                Some(text) => Some(parse_time(text)?),
                None => None,
            })
        })
    }

    /// Write back an item's mutable fields. Returns whether the item exists.
    pub fn outbox_update(&self, item: &OutboxItem) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE outbox SET state = ?2, attempts = ?3, next_attempt_at = ?4, last_error = ?5, result = ?6,
                    expires_at = ?7, updated_at = ?8, payload = ?9 WHERE id = ?1",
                params![
                    item.id.to_string(),
                    name(item.state)?,
                    item.attempts,
                    item.next_attempt_at.map(|at| at.to_rfc3339()),
                    item.last_error,
                    item.result.as_ref().map(serde_json::to_string).transpose()?,
                    item.expires_at.to_rfc3339(),
                    item.updated_at.to_rfc3339(),
                    serde_json::to_string(&item.payload)?,
                ],
            )? > 0)
        })
    }

    /// Take a queued item for delivery: `queued` → `sending`, counting the attempt. `None` when it is
    /// no longer queued (cancelled, or taken by another delivery).
    pub fn outbox_claim(&self, id: Uuid, now: DateTime<Utc>) -> Result<Option<OutboxItem>> {
        self.with(|c| {
            let changed = c.execute(
                "UPDATE outbox SET state = 'sending', attempts = attempts + 1, next_attempt_at = NULL, updated_at = ?2
                 WHERE id = ?1 AND state = 'queued'",
                params![id.to_string(), now.to_rfc3339()],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            Ok(c.query_row(&format!("{OUTBOX_SELECT} WHERE id = ?1"), [id.to_string()], row_to_item).optional()?)
        })
    }

    /// Write the outcome of a delivery, only if the item is still `sending` (a cancel in between wins).
    /// Returns whether it was written.
    pub fn outbox_finish(&self, item: &OutboxItem) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE outbox SET state = ?2, next_attempt_at = ?3, last_error = ?4, result = ?5, updated_at = ?6
                 WHERE id = ?1 AND state = 'sending'",
                params![
                    item.id.to_string(),
                    name(item.state)?,
                    item.next_attempt_at.map(|at| at.to_rfc3339()),
                    item.last_error,
                    item.result.as_ref().map(serde_json::to_string).transpose()?,
                    item.updated_at.to_rfc3339(),
                ],
            )? > 0)
        })
    }

    /// Make the queued move of `thread_id` due now if it was waiting for the thread ("Waiting for …"): its turn ended.
    pub fn outbox_wake_move(&self, thread_id: Uuid) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE outbox SET next_attempt_at = NULL
                 WHERE kind = 'move' AND state = 'queued' AND next_attempt_at IS NOT NULL AND last_error LIKE 'Waiting for%'
                   AND json_extract(payload, '$.source_thread_id') = ?1",
                [thread_id.to_string()],
            )? > 0)
        })
    }

    /// Whether a provisioning for `target` is still queued or in flight: work that needs the project there waits for it.
    pub fn outbox_provision_pending(&self, target: &str) -> Result<bool> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM outbox WHERE kind = 'provision' AND target = ?1 AND state IN ('queued','sending'))",
                [target],
                |r| r.get::<_, bool>(0),
            )?)
        })
    }

    /// A successful provisioning overlapped this attempt, so its "missing project" reply may be stale.
    pub fn outbox_provision_completed_since(&self, target: &str, since: DateTime<Utc>) -> Result<bool> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM outbox WHERE kind = 'provision' AND target = ?1
                 AND state = 'delivered' AND updated_at >= ?2)",
                params![target, since.to_rfc3339()],
                |r| r.get::<_, bool>(0),
            )?)
        })
    }

    /// Make the queued items for `target` that were waiting for a provisioning ("Waiting for …") due now: it settled.
    pub fn outbox_wake_waiting(&self, target: &str) -> Result<usize> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE outbox SET next_attempt_at = NULL
                 WHERE target = ?1 AND kind != 'provision' AND state = 'queued' AND last_error LIKE 'Waiting for%'",
                [target],
            )?)
        })
    }

    /// After a restart a delivery that was in flight is queued again; the target deduplicates it.
    pub fn outbox_requeue_sending(&self) -> Result<usize> {
        self.with(|c| Ok(c.execute("UPDATE outbox SET state = 'queued', next_attempt_at = NULL WHERE state = 'sending'", [])?))
    }

    /// How many items wait and how many failed, for the sidebar and `network.status`.
    pub fn outbox_counts(&self) -> Result<(u32, u32)> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(state IN ('queued','sending')), 0), COALESCE(SUM(state IN ('failed','expired')), 0) FROM outbox",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
    }

    // ---- remote children ----

    /// Write the row for a new remote child. A repeated `operation_id` returns the first row.
    pub fn remote_child_insert(&self, child: &RemoteChild) -> Result<RemoteChild> {
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO remote_children(task_id, operation_id, parent_thread_id, target, remote_thread_id, status,
                    stop_requested, last_seq, title, provider, model, info, created_at, updated_at, completed_at_from_target)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    child.task_id.to_string(),
                    child.operation_id.to_string(),
                    child.parent_thread_id.to_string(),
                    child.target,
                    child.remote_thread_id.map(|id| id.to_string()),
                    child.state.as_str(),
                    child.stop_requested,
                    child.last_seq,
                    child.title,
                    child.provider.map(|kind| kind.as_str()),
                    child.model,
                    serde_json::to_string(&child.info)?,
                    child.created_at.to_rfc3339(),
                    child.updated_at.to_rfc3339(),
                    child.completed_at_from_target,
                ],
            )?;
            Ok(c.query_row(&format!("{CHILD_SELECT} WHERE operation_id = ?1"), [child.operation_id.to_string()], row_to_child)?)
        })
    }

    /// Write a remote child and its outbox item in one transaction, so a crash leaves both or neither.
    /// A repeated `operation_id` returns the rows that already exist.
    pub fn remote_child_enqueue(&self, child: &RemoteChild, item: &OutboxItem) -> Result<(RemoteChild, OutboxItem)> {
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            let existing: Option<String> = tx
                .query_row("SELECT task_id FROM remote_children WHERE operation_id = ?1", [child.operation_id.to_string()], |r| r.get(0))
                .optional()?;
            if existing.is_none() {
                tx.execute(
                    "INSERT INTO remote_children(task_id, operation_id, parent_thread_id, target, remote_thread_id, status,
                        stop_requested, last_seq, title, provider, model, info, created_at, updated_at, completed_at_from_target)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                    params![
                        child.task_id.to_string(),
                        child.operation_id.to_string(),
                        child.parent_thread_id.to_string(),
                        child.target,
                        child.remote_thread_id.map(|id| id.to_string()),
                        child.state.as_str(),
                        child.stop_requested,
                        child.last_seq,
                        child.title,
                        child.provider.map(|kind| kind.as_str()),
                        child.model,
                        serde_json::to_string(&child.info)?,
                        child.created_at.to_rfc3339(),
                        child.updated_at.to_rfc3339(),
                        child.completed_at_from_target,
                    ],
                )?;
                tx.execute(
                    "INSERT OR IGNORE INTO outbox(id, operation_id, target, kind, payload, origin, state, attempts, next_attempt_at,
                        last_error, result, created_at, expires_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        item.id.to_string(),
                        item.operation_id.to_string(),
                        item.target,
                        name(item.kind)?,
                        serde_json::to_string(&item.payload)?,
                        item.origin.as_ref().map(serde_json::to_string).transpose()?,
                        name(item.state)?,
                        item.attempts,
                        item.next_attempt_at.map(|at| at.to_rfc3339()),
                        item.last_error,
                        item.result.as_ref().map(serde_json::to_string).transpose()?,
                        item.created_at.to_rfc3339(),
                        item.expires_at.to_rfc3339(),
                        item.updated_at.to_rfc3339(),
                    ],
                )?;
            }
            let child = tx.query_row(&format!("{CHILD_SELECT} WHERE operation_id = ?1"), [child.operation_id.to_string()], row_to_child)?;
            let item = tx.query_row(&format!("{OUTBOX_SELECT} WHERE operation_id = ?1"), [item.operation_id.to_string()], row_to_item)?;
            tx.commit()?;
            Ok((child, item))
        })
    }

    pub fn remote_child_get(&self, task_id: Uuid) -> Result<Option<RemoteChild>> {
        self.with(|c| Ok(c.query_row(&format!("{CHILD_SELECT} WHERE task_id = ?1"), [task_id.to_string()], row_to_child).optional()?))
    }

    pub fn remote_child_by_operation(&self, operation_id: Uuid) -> Result<Option<RemoteChild>> {
        self.with(|c| {
            Ok(c.query_row(&format!("{CHILD_SELECT} WHERE operation_id = ?1"), [operation_id.to_string()], row_to_child).optional()?)
        })
    }

    /// A parent's remote children, oldest first.
    pub fn remote_children_of(&self, parent_thread_id: ThreadId) -> Result<Vec<RemoteChild>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("{CHILD_SELECT} WHERE parent_thread_id = ?1 ORDER BY created_at, task_id"))?;
            Ok(st.query_map([parent_thread_id.to_string()], row_to_child)?.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Every remote child that has not finished, or still has a stop to confirm, oldest first: the
    /// followers restart from this.
    pub fn remote_children_active(&self) -> Result<Vec<RemoteChild>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "{CHILD_SELECT} WHERE status IN ('pending','running') OR stop_requested = 1 ORDER BY created_at, task_id"
            ))?;
            Ok(st.query_map([], row_to_child)?.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Finished children whose outcome never reached their parent (`info.parent_notified` is false).
    pub fn remote_children_unnotified(&self) -> Result<Vec<RemoteChild>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "{CHILD_SELECT} WHERE status NOT IN ('pending','running') AND json_extract(info, '$.parent_notified') = 0 ORDER BY created_at, task_id"
            ))?;
            Ok(st.query_map([], row_to_child)?.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// How many of a parent's remote children are still running (or not yet acknowledged).
    pub fn remote_children_active_count(&self, parent_thread_id: ThreadId) -> Result<u32> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM remote_children WHERE parent_thread_id = ?1 AND status IN ('pending','running')",
                [parent_thread_id.to_string()],
                |r| r.get(0),
            )?)
        })
    }

    /// Write back every mutable field of a remote child. Returns whether the row exists.
    pub fn remote_child_update(&self, child: &RemoteChild) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE remote_children SET remote_thread_id = ?2, status = ?3, stop_requested = ?4, last_seq = ?5, info = ?6,
                    updated_at = ?7, completed_at_from_target = ?8 WHERE task_id = ?1",
                params![
                    child.task_id.to_string(),
                    child.remote_thread_id.map(|id| id.to_string()),
                    child.state.as_str(),
                    child.stop_requested,
                    child.last_seq,
                    serde_json::to_string(&child.info)?,
                    child.updated_at.to_rfc3339(),
                    child.completed_at_from_target,
                ],
            )? > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use kybern_protocol::{DelegationInfo, DelegationRole, DelegationStatus, DelegationWorkspace};
    use serde_json::json;

    use kybern_protocol::{OutboxKind, RemoteChildState};

    use super::*;

    /// A store whose remote children need no real parent thread.
    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.with(|c| Ok(c.execute_batch("PRAGMA foreign_keys = OFF")?)).unwrap();
        store
    }

    fn item(operation: Uuid, target: &str, at: DateTime<Utc>) -> OutboxItem {
        OutboxItem {
            id: Uuid::now_v7(),
            operation_id: operation,
            target: target.into(),
            kind: OutboxKind::Delegate,
            payload: json!({"task": "x"}),
            origin: None,
            state: OutboxState::Queued,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
            result: None,
            created_at: at,
            expires_at: at + Duration::days(7),
            updated_at: at,
        }
    }

    #[test]
    fn a_waiting_move_is_woken_when_its_thread_goes_idle() {
        let store = store();
        let thread = Uuid::now_v7();
        let now = Utc::now();
        let mut waiting = item(Uuid::now_v7(), "m2", now);
        waiting.kind = OutboxKind::Move;
        waiting.payload = json!({ "source_thread_id": thread });
        waiting.next_attempt_at = Some(now + Duration::minutes(5));
        waiting.last_error = Some("Waiting for the running turn to finish.".into());
        store.outbox_insert(&waiting).unwrap();
        let mut other = item(Uuid::now_v7(), "m2", now);
        other.kind = OutboxKind::Move;
        other.payload = json!({ "source_thread_id": Uuid::now_v7() });
        other.next_attempt_at = Some(now + Duration::minutes(5));
        other.last_error = Some("Waiting for the running turn to finish.".into());
        store.outbox_insert(&other).unwrap();
        let mut failing = item(Uuid::now_v7(), "m2", now);
        failing.kind = OutboxKind::Move;
        failing.payload = json!({ "source_thread_id": thread });
        failing.next_attempt_at = Some(now + Duration::minutes(5));
        failing.last_error = Some("the session closed".into());
        store.outbox_insert(&failing).unwrap();

        assert!(!store.outbox_wake_move(Uuid::now_v7()).unwrap(), "nothing waits for an unrelated thread");
        assert!(store.outbox_wake_move(thread).unwrap());
        assert_eq!(store.outbox_get(waiting.id).unwrap().unwrap().next_attempt_at, None, "due now");
        assert!(store.outbox_get(other.id).unwrap().unwrap().next_attempt_at.is_some(), "another thread's move keeps its backoff");
        assert!(
            store.outbox_get(failing.id).unwrap().unwrap().next_attempt_at.is_some(),
            "a move that failed for another reason keeps its backoff"
        );
        assert!(!store.outbox_wake_move(thread).unwrap(), "already due");
    }

    fn child(parent: ThreadId, state: RemoteChildState) -> RemoteChild {
        let now = Utc::now();
        RemoteChild {
            task_id: Uuid::now_v7(),
            operation_id: Uuid::now_v7(),
            parent_thread_id: parent,
            target: "m2".into(),
            remote_thread_id: None,
            state,
            stop_requested: false,
            last_seq: 0,
            title: "Fix it".into(),
            provider: None,
            model: None,
            info: DelegationInfo {
                task_id: Uuid::nil(),
                operation_id: Uuid::nil(),
                parent_thread_id: parent,
                depth: 1,
                role: DelegationRole::General,
                workspace: DelegationWorkspace::Shared,
                default_override: None,
                owns: vec![],
                status: DelegationStatus::Running,
                result: None,
                error: None,
                files_touched: vec![],
                conflicts: vec![],
                base_commit: None,
                branch: None,
                head_commit: None,
                diffstat: None,
                worktree_state: None,
                started_at: now,
                completed_at: None,
                parent_notified: true,
            },
            completed_at_from_target: false,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_repeated_operation_returns_the_first_item() {
        let store = store();
        let now = Utc::now();
        let operation = Uuid::now_v7();
        let first = store.outbox_insert(&item(operation, "m2", now)).unwrap();
        let second = store.outbox_insert(&item(operation, "m3", now)).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.target, "m2");
        assert_eq!(store.outbox_list(None, None).unwrap().len(), 1);
    }

    #[test]
    fn due_items_skip_the_future_the_expired_and_the_settled() {
        let store = store();
        let now = Utc::now();
        let ready = store.outbox_insert(&item(Uuid::now_v7(), "m2", now - Duration::minutes(1))).unwrap();
        let mut later = item(Uuid::now_v7(), "m2", now);
        later.next_attempt_at = Some(now + Duration::minutes(5));
        store.outbox_insert(&later).unwrap();
        let mut old = item(Uuid::now_v7(), "m2", now - Duration::days(8));
        old.expires_at = now - Duration::days(1);
        let old = store.outbox_insert(&old).unwrap();
        let mut done = item(Uuid::now_v7(), "m2", now);
        done.state = OutboxState::Delivered;
        store.outbox_insert(&done).unwrap();

        let due = store.outbox_due(now).unwrap();
        assert_eq!(due.iter().map(|i| i.id).collect::<Vec<_>>(), vec![ready.id]);
        assert_eq!(store.outbox_overdue(now).unwrap().iter().map(|i| i.id).collect::<Vec<_>>(), vec![old.id]);
        assert_eq!(store.outbox_counts().unwrap(), (3, 0));
        assert!(store.outbox_next_due().unwrap().is_some());
    }

    #[test]
    fn sending_items_are_queued_again_after_a_restart_and_updates_stick() {
        let store = store();
        let now = Utc::now();
        let mut sending = store.outbox_insert(&item(Uuid::now_v7(), "m2", now)).unwrap();
        sending.state = OutboxState::Sending;
        sending.attempts = 2;
        sending.last_error = Some("offline".into());
        assert!(store.outbox_update(&sending).unwrap());
        assert_eq!(store.outbox_get(sending.id).unwrap().unwrap().attempts, 2);
        assert_eq!(store.outbox_requeue_sending().unwrap(), 1);
        let back = store.outbox_get(sending.id).unwrap().unwrap();
        assert_eq!(back.state, OutboxState::Queued);
        assert_eq!(back.last_error.as_deref(), Some("offline"));
        assert_eq!(store.outbox_list(Some("m2"), Some(OutboxState::Queued)).unwrap().len(), 1);
        assert!(store.outbox_list(Some("m9"), None).unwrap().is_empty());
    }

    #[test]
    fn remote_children_round_trip_and_the_active_ones_are_found() {
        let store = store();
        let parent = Uuid::now_v7();
        let pending = store.remote_child_insert(&child(parent, RemoteChildState::Pending)).unwrap();
        let mut finished = child(parent, RemoteChildState::Completed);
        finished.info.parent_notified = false;
        let finished = store.remote_child_insert(&finished).unwrap();
        let again = store.remote_child_insert(&RemoteChild { task_id: Uuid::now_v7(), ..pending.clone() }).unwrap();
        assert_eq!(again.task_id, pending.task_id, "a repeated operation returns the first row");

        assert_eq!(store.remote_children_active().unwrap().iter().map(|c| c.task_id).collect::<Vec<_>>(), vec![pending.task_id]);
        assert_eq!(store.remote_children_unnotified().unwrap().iter().map(|c| c.task_id).collect::<Vec<_>>(), vec![finished.task_id]);
        assert_eq!(store.remote_children_active_count(parent).unwrap(), 1);
        assert_eq!(store.remote_children_of(parent).unwrap().len(), 2);

        let mut running = pending;
        running.state = RemoteChildState::Running;
        running.remote_thread_id = Some(Uuid::now_v7());
        running.last_seq = 7;
        assert!(store.remote_child_update(&running).unwrap());
        let read = store.remote_child_get(running.task_id).unwrap().unwrap();
        assert_eq!((read.state, read.last_seq, read.remote_thread_id), (RemoteChildState::Running, 7, running.remote_thread_id));
    }
}
