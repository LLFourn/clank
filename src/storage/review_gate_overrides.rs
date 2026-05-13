use crate::domain::TargetKind;
use crate::lifecycle::SessionId;
use crate::review_state::{ReviewGateOverride, ReviewGateState, ReviewPhase};

#[derive(Debug, Clone, sqlx::FromRow)]
struct ReviewGateOverrideRow {
    state: String,
    actor: String,
    created_at: i64,
}

pub async fn fetch_for_target<'e, E>(
    executor: E,
    session_id: &SessionId,
    phase: ReviewPhase,
    target_kind: TargetKind,
    target_id: &str,
) -> sqlx::Result<Option<ReviewGateOverride>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query_as::<_, ReviewGateOverrideRow>(
        "SELECT state, actor, created_at FROM review_gate_overrides \
         WHERE session_id = ? AND phase = ? AND target_kind = ? AND target_id = ?",
    )
    .bind(session_id.as_str())
    .bind(phase.as_str())
    .bind(target_kind.as_str())
    .bind(target_id)
    .fetch_optional(executor)
    .await?;

    Ok(row.and_then(|row| {
        ReviewGateState::parse(&row.state).map(|state| ReviewGateOverride {
            actor: row.actor,
            state,
            target_id: target_id.to_string(),
            created_at: row.created_at,
        })
    }))
}

pub async fn upsert<'e, E>(
    executor: E,
    session_id: &SessionId,
    phase: ReviewPhase,
    target_kind: TargetKind,
    target_id: &str,
    state: ReviewGateState,
    actor: &str,
    now: i64,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "INSERT INTO review_gate_overrides \
            (session_id, phase, target_kind, target_id, state, actor, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(session_id, phase) DO UPDATE SET \
            target_kind = excluded.target_kind, \
            target_id = excluded.target_id, \
            state = excluded.state, \
            actor = excluded.actor, \
            updated_at = excluded.updated_at",
    )
    .bind(session_id.as_str())
    .bind(phase.as_str())
    .bind(target_kind.as_str())
    .bind(target_id)
    .bind(state.as_str())
    .bind(actor)
    .bind(now)
    .bind(now)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn delete_phase<'e, E>(
    executor: E,
    session_id: &SessionId,
    phase: ReviewPhase,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("DELETE FROM review_gate_overrides WHERE session_id = ? AND phase = ?")
        .bind(session_id.as_str())
        .bind(phase.as_str())
        .execute(executor)
        .await?;
    Ok(())
}
