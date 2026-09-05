//! Workspace context guard middleware.
//!
//! Resolves the workspace context for workspace-scoped requests by:
//! 1. Extracting `workspace_id` from the request (header, path param, or query param)
//! 2. Looking up the user's role in `workspace_members`
//! 3. Injecting `WorkspaceContext` into request extensions
//!
//! Returns 403 `NOT_A_WORKSPACE_MEMBER` if the user is not a member.
//! Returns 400 `MALFORMED_REQUEST_BODY` if workspace_id cannot be resolved.
//!
//! This middleware MUST run after `auth_middleware` (requires `AccessTokenClaims` in extensions).

use axum::{
    extract::Request,
    middleware::Next,
    response::Response,
};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;

// ─── Workspace Role ─────────────────────────────────────────────────────────────

/// Workspace-level role with a numeric level for comparison.
///
/// Hierarchy: Owner(4) > Admin(3) > Editor(2) > Viewer(1) > Member(0)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkspaceRole {
    /// Basic member with no explicit role assignment (legacy fallback).
    Member = 0,
    /// Read-only access to workspace content.
    Viewer = 1,
    /// Can create and edit own content.
    Editor = 2,
    /// Full CRUD on all content, manage members (except owner).
    Admin = 3,
    /// Full control including billing and workspace deletion.
    Owner = 4,
}

impl WorkspaceRole {
    /// Parse a role string from the database into a WorkspaceRole.
    pub fn from_db_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "owner" => Self::Owner,
            "admin" => Self::Admin,
            "editor" => Self::Editor,
            "viewer" => Self::Viewer,
            "member" => Self::Member,
            _ => Self::Member,
        }
    }

    /// Returns the numeric level of this role.
    pub fn level(&self) -> u8 {
        *self as u8
    }
}

// ─── Workspace Context ──────────────────────────────────────────────────────────

/// Workspace context injected into request extensions after successful authorization.
///
/// Downstream handlers and extractors can use this to access the resolved
/// workspace ID and the caller's role within that workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceContext {
    /// The resolved workspace ID.
    pub workspace_id: Uuid,
    /// The authenticated user's ID (copied from AccessTokenClaims for convenience).
    pub user_id: Uuid,
    /// The user's role within this workspace.
    pub role: WorkspaceRole,
    /// Custom permissions (if set). None = use role-based defaults.
    pub permissions: Option<Vec<String>>,
}

// ─── Workspace ID Extraction ────────────────────────────────────────────────────

/// Header name for workspace ID (used by web clients).
const WORKSPACE_ID_HEADER: &str = "x-workspace-id";

/// Row type for the workspace_members query.
#[derive(Debug, sqlx::FromRow)]
struct MembershipRow {
    pub role: String,
    pub permissions: Option<serde_json::Value>,
}

/// Extract workspace_id from the request in priority order:
/// 1. Path segment after `/teams/` or `/workspaces/` (the authoritative target
///    for path-scoped resources like `PATCH /workspaces/:id`)
/// 2. `X-Workspace-ID` header (used by header-scoped routes such as `/sync/*`)
/// 3. Query parameter `workspace_id`
///
/// Path takes precedence over the header: when a URL explicitly names a
/// workspace resource (e.g. `/workspaces/:id`), authorization MUST be scoped to
/// that workspace. Web clients always send `X-Workspace-ID` = the *active*
/// workspace, which may differ from the workspace being viewed/edited by URL;
/// preferring the header there would authorize against the wrong workspace.
fn extract_workspace_id(request: &Request) -> Option<Uuid> {
    // 1. Try path parameter — parse UUID segment after known prefixes.
    let path = request.uri().path();
    if let Some(id) = extract_workspace_id_from_path(path) {
        return Some(id);
    }

    // 2. Try X-Workspace-ID header (for routes with no workspace id in the path).
    if let Some(header_val) = request.headers().get(WORKSPACE_ID_HEADER) {
        if let Ok(s) = header_val.to_str() {
            if let Ok(id) = Uuid::parse_str(s) {
                return Some(id);
            }
        }
    }

    // 3. Try query parameter `workspace_id=<uuid>`
    if let Some(query_str) = request.uri().query() {
        for pair in query_str.split('&') {
            if let Some(value) = pair.strip_prefix("workspace_id=") {
                if let Ok(id) = Uuid::parse_str(value) {
                    return Some(id);
                }
            }
        }
    }

    None
}

/// Extract workspace_id UUID from path segments after `/teams/` or `/workspaces/`.
fn extract_workspace_id_from_path(path: &str) -> Option<Uuid> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    for (i, segment) in segments.iter().enumerate() {
        if (*segment == "teams" || *segment == "workspaces") && i + 1 < segments.len() {
            if let Ok(id) = Uuid::parse_str(segments[i + 1]) {
                return Some(id);
            }
        }
    }

    None
}

// ─── Middleware Function ────────────────────────────────────────────────────────

/// Workspace context guard middleware.
///
/// Extracts workspace_id from the request, verifies the authenticated user is a member
/// of that workspace, and injects `WorkspaceContext` into request extensions.
///
/// # Behavior
///
/// - If no `AccessTokenClaims` are present (unauthenticated route), passes through.
/// - If no workspace_id can be extracted, returns 400.
/// - If the user is not a member of the workspace, returns 403.
/// - On success, injects `WorkspaceContext` into request extensions.
pub async fn workspace_context_guard(
    mut request: Request,
    next: Next,
    pool: Arc<PgPool>,
) -> Result<Response, AppError> {
    // If no claims in extensions, this is a public route — pass through.
    let claims = match request.extensions().get::<AccessTokenClaims>().cloned() {
        Some(c) => c,
        None => return Ok(next.run(request).await),
    };

    // Extract workspace_id from the request.
    let workspace_id = match extract_workspace_id(&request) {
        Some(id) => id,
        None => return Err(AppError::MalformedRequestBody),
    };

    // Query the user's role and permissions in this workspace.
    let membership = sqlx::query_as::<_, MembershipRow>(
        "SELECT role, permissions FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id)
    .bind(claims.sub)
    .fetch_optional(pool.as_ref())
    .await
    .map_err(|_| AppError::InternalError)?;

    let context = match membership {
        Some(m) => {
            // Standard workspace member.
            // Parse permissions from JSONB: null → None, array → Some(Vec<String>)
            let permissions: Option<Vec<String>> =
                m.permissions.and_then(|v| serde_json::from_value(v).ok());
            WorkspaceContext {
                workspace_id,
                user_id: claims.sub,
                role: WorkspaceRole::from_db_str(&m.role),
                permissions,
            }
        }
        None => {
            // Not a workspace member. Fall back to the account-level collaborator
            // model: an accepted collaborator of the workspace owner (with an
            // active Teams subscription) is admitted with a role derived from
            // their broadest effective grant on this workspace. Fine-grained
            // per-folder/per-snippet enforcement then happens in SyncService.
            match collaborator_workspace_role(pool.as_ref(), claims.sub, workspace_id).await? {
                Some(role) => WorkspaceContext {
                    workspace_id,
                    user_id: claims.sub,
                    role,
                    permissions: None,
                },
                None => return Err(AppError::NotAWorkspaceMember),
            }
        }
    };

    request.extensions_mut().insert(context);

    Ok(next.run(request).await)
}

/// Resolve an account-level collaborator's coarse role on a workspace, used to
/// admit them through the workspace guard when they are not a `workspace_members`
/// row. Returns:
/// - `Some(Editor)` if they hold any `edit` grant reaching this workspace,
/// - `Some(Viewer)` if they hold any `view` grant (but no edit),
/// - `None` if they have no grants, no accepted collaboration, or the owner is
///   not currently Teams-enabled.
///
/// This is intentionally coarse: it only decides admittance. The precise
/// most-specific-wins per-resource decision is made in `SyncService`.
async fn collaborator_workspace_role(
    pool: &PgPool,
    user_id: Uuid,
    workspace_id: Uuid,
) -> Result<Option<WorkspaceRole>, AppError> {
    // Determine the workspace owner.
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
    // The owner themselves is handled by ownership checks elsewhere; they are not
    // a collaborator of their own account.
    if owner_id == user_id {
        return Ok(None);
    }

    // Must be an accepted collaborator whose owner is currently Teams-enabled.
    let ok: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(SELECT 1 FROM collaborations WHERE owner_id = $1 AND collaborator_id = $2)
          AND EXISTS(
            SELECT 1 FROM subscriptions s
            INNER JOIN workspaces w ON w.id = s.workspace_id
            WHERE w.owner_id = $1 AND s.tier = 'teams' AND s.status IN ('active', 'past_due')
          )
        "#,
    )
    .bind(owner_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;
    if !ok {
        return Ok(None);
    }

    // Find the broadest level of any grant reaching this workspace: a workspace
    // grant, a folder grant on a folder in this workspace, or a snippet grant on
    // a snippet in this workspace. 'edit' outranks 'view'.
    let level: Option<String> = sqlx::query_scalar(
        r#"
        SELECT g.level FROM access_grants g
        WHERE g.collaborator_id = $1 AND (
            (g.resource_type = 'workspace' AND g.resource_id = $2)
            OR (g.resource_type = 'folder' AND g.resource_id IN (
                SELECT id FROM folders WHERE workspace_id = $2))
            OR (g.resource_type = 'snippet' AND g.resource_id IN (
                SELECT id FROM snippets WHERE workspace_id = $2))
        )
        ORDER BY CASE g.level WHEN 'edit' THEN 2 ELSE 1 END DESC
        LIMIT 1
        "#,
    )
    .bind(user_id)
    .bind(workspace_id)
    .fetch_optional(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    Ok(match level.as_deref() {
        Some("edit") => Some(WorkspaceRole::Editor),
        Some("view") => Some(WorkspaceRole::Viewer),
        _ => None,
    })
}

// ─── Role Guard Functions ───────────────────────────────────────────────────────

/// Require at least VIEWER role.
pub fn check_role_viewer(context: &WorkspaceContext) -> Result<(), AppError> {
    if context.role >= WorkspaceRole::Viewer {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// Require at least EDITOR role.
pub fn check_role_editor(context: &WorkspaceContext) -> Result<(), AppError> {
    if context.role >= WorkspaceRole::Editor {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// Require at least ADMIN role.
pub fn check_role_admin(context: &WorkspaceContext) -> Result<(), AppError> {
    if context.role >= WorkspaceRole::Admin {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// Require OWNER role.
pub fn check_role_owner(context: &WorkspaceContext) -> Result<(), AppError> {
    if context.role >= WorkspaceRole::Owner {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// Check ownership for EDITOR-level "own only" operations.
///
/// - If role >= ADMIN: always allowed (can edit/delete anyone's content)
/// - If role == EDITOR: allowed only if `entity_created_by == user_id`
/// - If role < EDITOR: always denied
pub fn check_ownership(
    context: &WorkspaceContext,
    entity_created_by: Uuid,
) -> Result<(), AppError> {
    if context.role >= WorkspaceRole::Admin {
        return Ok(());
    }
    if context.role >= WorkspaceRole::Editor {
        if entity_created_by == context.user_id {
            return Ok(());
        }
        return Err(AppError::Forbidden);
    }
    Err(AppError::Forbidden)
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_role_ordering() {
        assert!(WorkspaceRole::Owner > WorkspaceRole::Admin);
        assert!(WorkspaceRole::Admin > WorkspaceRole::Editor);
        assert!(WorkspaceRole::Editor > WorkspaceRole::Viewer);
        assert!(WorkspaceRole::Viewer > WorkspaceRole::Member);
    }

    #[test]
    fn workspace_role_from_db_str_variants() {
        assert_eq!(WorkspaceRole::from_db_str("owner"), WorkspaceRole::Owner);
        assert_eq!(WorkspaceRole::from_db_str("admin"), WorkspaceRole::Admin);
        assert_eq!(WorkspaceRole::from_db_str("editor"), WorkspaceRole::Editor);
        assert_eq!(WorkspaceRole::from_db_str("viewer"), WorkspaceRole::Viewer);
        assert_eq!(WorkspaceRole::from_db_str("member"), WorkspaceRole::Member);
        assert_eq!(WorkspaceRole::from_db_str("OWNER"), WorkspaceRole::Owner);
        assert_eq!(WorkspaceRole::from_db_str("Admin"), WorkspaceRole::Admin);
        assert_eq!(WorkspaceRole::from_db_str("unknown"), WorkspaceRole::Member);
    }

    #[test]
    fn check_role_viewer_logic() {
        let ctx = |role| WorkspaceContext {
            workspace_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            role,
            permissions: None,
        };
        assert!(check_role_viewer(&ctx(WorkspaceRole::Owner)).is_ok());
        assert!(check_role_viewer(&ctx(WorkspaceRole::Admin)).is_ok());
        assert!(check_role_viewer(&ctx(WorkspaceRole::Editor)).is_ok());
        assert!(check_role_viewer(&ctx(WorkspaceRole::Viewer)).is_ok());
        assert!(check_role_viewer(&ctx(WorkspaceRole::Member)).is_err());
    }

    #[test]
    fn check_role_editor_logic() {
        let ctx = |role| WorkspaceContext {
            workspace_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            role,
            permissions: None,
        };
        assert!(check_role_editor(&ctx(WorkspaceRole::Owner)).is_ok());
        assert!(check_role_editor(&ctx(WorkspaceRole::Admin)).is_ok());
        assert!(check_role_editor(&ctx(WorkspaceRole::Editor)).is_ok());
        assert!(check_role_editor(&ctx(WorkspaceRole::Viewer)).is_err());
        assert!(check_role_editor(&ctx(WorkspaceRole::Member)).is_err());
    }

    #[test]
    fn check_role_admin_logic() {
        let ctx = |role| WorkspaceContext {
            workspace_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            role,
            permissions: None,
        };
        assert!(check_role_admin(&ctx(WorkspaceRole::Owner)).is_ok());
        assert!(check_role_admin(&ctx(WorkspaceRole::Admin)).is_ok());
        assert!(check_role_admin(&ctx(WorkspaceRole::Editor)).is_err());
        assert!(check_role_admin(&ctx(WorkspaceRole::Viewer)).is_err());
    }

    #[test]
    fn check_role_owner_logic() {
        let ctx = |role| WorkspaceContext {
            workspace_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            role,
            permissions: None,
        };
        assert!(check_role_owner(&ctx(WorkspaceRole::Owner)).is_ok());
        assert!(check_role_owner(&ctx(WorkspaceRole::Admin)).is_err());
        assert!(check_role_owner(&ctx(WorkspaceRole::Editor)).is_err());
    }

    #[test]
    fn check_ownership_admin_can_modify_any() {
        let user_id = Uuid::new_v4();
        let other_user = Uuid::new_v4();
        let ctx = WorkspaceContext {
            workspace_id: Uuid::new_v4(),
            user_id,
            role: WorkspaceRole::Admin,
            permissions: None,
        };
        assert!(check_ownership(&ctx, other_user).is_ok());
        assert!(check_ownership(&ctx, user_id).is_ok());
    }

    #[test]
    fn check_ownership_editor_can_modify_own_only() {
        let user_id = Uuid::new_v4();
        let other_user = Uuid::new_v4();
        let ctx = WorkspaceContext {
            workspace_id: Uuid::new_v4(),
            user_id,
            role: WorkspaceRole::Editor,
            permissions: None,
        };
        assert!(check_ownership(&ctx, user_id).is_ok());
        assert!(check_ownership(&ctx, other_user).is_err());
    }

    #[test]
    fn check_ownership_viewer_cannot_modify() {
        let user_id = Uuid::new_v4();
        let ctx = WorkspaceContext {
            workspace_id: Uuid::new_v4(),
            user_id,
            role: WorkspaceRole::Viewer,
            permissions: None,
        };
        assert!(check_ownership(&ctx, user_id).is_err());
    }

    #[test]
    fn extract_workspace_id_from_path_teams() {
        let id = Uuid::new_v4();
        let path = format!("/teams/{}/members", id);
        assert_eq!(extract_workspace_id_from_path(&path), Some(id));
    }

    #[test]
    fn extract_workspace_id_from_path_workspaces() {
        let id = Uuid::new_v4();
        let path = format!("/workspaces/{}", id);
        assert_eq!(extract_workspace_id_from_path(&path), Some(id));
    }

    #[test]
    fn extract_workspace_id_from_path_no_match() {
        assert_eq!(extract_workspace_id_from_path("/sync/snippets"), None);
        assert_eq!(extract_workspace_id_from_path("/admin/users"), None);
    }
}
