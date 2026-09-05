//! Pool-only access-resolution helpers.
//!
//! These functions implement the effective-access algorithm and grant cleanup
//! using only a `&PgPool`, so they can be called both by the owner-facing
//! `CollaboratorService` (which also needs email/config) and by `SyncService`
//! (which holds only a pool) for read/write enforcement.

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

use super::{Level, ResourceType};

/// True if `owner_id` has an active Teams subscription on any workspace they own.
/// Treats `active` and `past_due` as still-entitled (matching existing precedent).
pub async fn account_is_teams_enabled(pool: &PgPool, owner_id: Uuid) -> Result<bool, AppError> {
    let enabled: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM subscriptions s
            INNER JOIN workspaces w ON w.id = s.workspace_id
            WHERE w.owner_id = $1
              AND s.tier = 'teams'
              AND s.status IN ('active', 'past_due')
        )
        "#,
    )
    .bind(owner_id)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;
    Ok(enabled)
}

/// The folder ids a snippet belongs to (via the `snippet_folders` join).
pub async fn folders_of_snippet(pool: &PgPool, snippet_id: Uuid) -> Result<Vec<Uuid>, AppError> {
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT folder_id FROM snippet_folders WHERE snippet_id = $1")
            .bind(snippet_id)
            .fetch_all(pool)
            .await
            .map_err(|_| AppError::InternalError)?;
    Ok(ids)
}

/// The strongest grant level for a collaborator across a set of resource ids of
/// one type. Returns the max level found, or None.
async fn grant_level(
    pool: &PgPool,
    collaborator_id: Uuid,
    rtype: ResourceType,
    resource_ids: &[Uuid],
) -> Result<Option<Level>, AppError> {
    if resource_ids.is_empty() {
        return Ok(None);
    }
    let levels: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT level FROM access_grants
        WHERE collaborator_id = $1 AND resource_type = $2 AND resource_id = ANY($3)
        "#,
    )
    .bind(collaborator_id)
    .bind(rtype.as_str())
    .bind(resource_ids)
    .fetch_all(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let best = levels
        .iter()
        .filter_map(|s| Level::parse(s))
        .max_by_key(|l| l.rank());
    Ok(best)
}

fn consider(best: &mut Option<(u8, Level)>, specificity: u8, level: Level) {
    match best {
        Some((cur_spec, cur_level)) => {
            if specificity > *cur_spec
                || (specificity == *cur_spec && level.rank() > cur_level.rank())
            {
                *best = Some((specificity, level));
            }
        }
        None => *best = Some((specificity, level)),
    }
}

/// Resolve the effective access `user_id` has for a resource in `workspace_id`.
///
/// - Owner bypass: if `user_id` owns the workspace, returns `Some(Edit)` without
///   touching grant tables.
/// - Otherwise requires an accepted collaboration AND the owner to be
///   Teams-enabled (a lapsed subscription suspends all collaborator access).
/// - Applies most-specific-wins across snippet > folder(s) > workspace grants.
pub async fn effective_access(
    pool: &PgPool,
    user_id: Uuid,
    workspace_id: Uuid,
    folder_ids: &[Uuid],
    snippet_id: Option<Uuid>,
) -> Result<Option<Level>, AppError> {
    // 1. Owner bypass.
    let owner_id: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_id FROM workspaces WHERE id = $1")
            .bind(workspace_id)
            .fetch_optional(pool)
            .await
            .map_err(|_| AppError::InternalError)?;
    let owner_id = match owner_id {
        Some(o) => o,
        None => return Ok(None),
    };
    if owner_id == user_id {
        return Ok(Some(Level::Edit));
    }

    // 2. Accepted collaboration required.
    let is_collaborator: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM collaborations WHERE owner_id = $1 AND collaborator_id = $2)",
    )
    .bind(owner_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;
    if !is_collaborator {
        return Ok(None);
    }

    // 3. Owner must currently be Teams-enabled.
    if !account_is_teams_enabled(pool, owner_id).await? {
        return Ok(None);
    }

    // 4. Most-specific-wins across candidate grants.
    let mut best: Option<(u8, Level)> = None;

    if let Some(sid) = snippet_id {
        if let Some(level) = grant_level(pool, user_id, ResourceType::Snippet, &[sid]).await? {
            consider(&mut best, ResourceType::Snippet.specificity(), level);
        }
    }
    if !folder_ids.is_empty() {
        if let Some(level) = grant_level(pool, user_id, ResourceType::Folder, folder_ids).await? {
            consider(&mut best, ResourceType::Folder.specificity(), level);
        }
    }
    if let Some(level) = grant_level(pool, user_id, ResourceType::Workspace, &[workspace_id]).await?
    {
        consider(&mut best, ResourceType::Workspace.specificity(), level);
    }

    Ok(best.map(|(_, level)| level))
}

/// Is `user_id` the owner of `workspace_id`?
pub async fn is_workspace_owner(
    pool: &PgPool,
    user_id: Uuid,
    workspace_id: Uuid,
) -> Result<bool, AppError> {
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT owner_id FROM workspaces WHERE id = $1")
        .bind(workspace_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| AppError::InternalError)?;
    Ok(owner == Some(user_id))
}

/// Delete any access grants that reference a now-deleted resource id.
pub async fn cleanup_grants_for_resource(
    pool: &PgPool,
    rtype: ResourceType,
    resource_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM access_grants WHERE resource_type = $1 AND resource_id = $2")
        .bind(rtype.as_str())
        .bind(resource_id)
        .execute(pool)
        .await
        .map_err(|_| AppError::InternalError)?;
    Ok(())
}
