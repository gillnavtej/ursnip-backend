//! Fine-grained permission system for Teams workspaces.
//!
//! Provides:
//! - Permission constants (all valid permission strings)
//! - Default permission sets per role
//! - `check_permission()` function for handlers
//!
//! When `WorkspaceContext.permissions` is `Some(vec)`, the custom set is used.
//! When it's `None`, the system falls back to role-based defaults.

use crate::errors::AppError;
use crate::middleware::workspace_context::{WorkspaceContext, WorkspaceRole};

// ─── Permission Constants ────────────────────────────────────────────────────────

// Snippet permissions
pub const PERM_SNIPPETS_READ: &str = "snippets:read";
pub const PERM_SNIPPETS_CREATE: &str = "snippets:create";
pub const PERM_SNIPPETS_UPDATE_OWN: &str = "snippets:update_own";
pub const PERM_SNIPPETS_UPDATE_ANY: &str = "snippets:update_any";
pub const PERM_SNIPPETS_DELETE_OWN: &str = "snippets:delete_own";
pub const PERM_SNIPPETS_DELETE_ANY: &str = "snippets:delete_any";

// Folder permissions
pub const PERM_FOLDERS_CREATE: &str = "folders:create";
pub const PERM_FOLDERS_UPDATE: &str = "folders:update";
pub const PERM_FOLDERS_DELETE: &str = "folders:delete";

// Member permissions
pub const PERM_MEMBERS_INVITE: &str = "members:invite";
pub const PERM_MEMBERS_REMOVE: &str = "members:remove";
pub const PERM_MEMBERS_UPDATE_PERMISSIONS: &str = "members:update_permissions";

// Workspace permissions
pub const PERM_WORKSPACE_SETTINGS: &str = "workspace:settings";
pub const PERM_WORKSPACE_DELETE: &str = "workspace:delete";
pub const PERM_BILLING_MANAGE: &str = "billing:manage";

// Data & analytics
pub const PERM_DATA_SOURCES_MANAGE: &str = "data_sources:manage";
pub const PERM_ANALYTICS_VIEW: &str = "analytics:view";
pub const PERM_ANALYTICS_EXPORT: &str = "analytics:export";

/// All valid permission strings. Used for validation.
pub const ALL_PERMISSIONS: &[&str] = &[
    PERM_SNIPPETS_READ,
    PERM_SNIPPETS_CREATE,
    PERM_SNIPPETS_UPDATE_OWN,
    PERM_SNIPPETS_UPDATE_ANY,
    PERM_SNIPPETS_DELETE_OWN,
    PERM_SNIPPETS_DELETE_ANY,
    PERM_FOLDERS_CREATE,
    PERM_FOLDERS_UPDATE,
    PERM_FOLDERS_DELETE,
    PERM_MEMBERS_INVITE,
    PERM_MEMBERS_REMOVE,
    PERM_MEMBERS_UPDATE_PERMISSIONS,
    PERM_WORKSPACE_SETTINGS,
    PERM_WORKSPACE_DELETE,
    PERM_BILLING_MANAGE,
    PERM_DATA_SOURCES_MANAGE,
    PERM_ANALYTICS_VIEW,
    PERM_ANALYTICS_EXPORT,
];

// ─── Default Permission Sets ─────────────────────────────────────────────────────

/// Returns the default permissions for a given role.
/// Used when `workspace_members.permissions` is NULL.
pub fn default_permissions_for_role(role: WorkspaceRole) -> &'static [&'static str] {
    match role {
        WorkspaceRole::Owner => ALL_PERMISSIONS,
        WorkspaceRole::Admin => &[
            PERM_SNIPPETS_READ,
            PERM_SNIPPETS_CREATE,
            PERM_SNIPPETS_UPDATE_OWN,
            PERM_SNIPPETS_UPDATE_ANY,
            PERM_SNIPPETS_DELETE_OWN,
            PERM_SNIPPETS_DELETE_ANY,
            PERM_FOLDERS_CREATE,
            PERM_FOLDERS_UPDATE,
            PERM_FOLDERS_DELETE,
            PERM_MEMBERS_INVITE,
            PERM_MEMBERS_REMOVE,
            PERM_MEMBERS_UPDATE_PERMISSIONS,
            PERM_WORKSPACE_SETTINGS,
            PERM_DATA_SOURCES_MANAGE,
            PERM_ANALYTICS_VIEW,
            PERM_ANALYTICS_EXPORT,
        ],
        WorkspaceRole::Editor => &[
            PERM_SNIPPETS_READ,
            PERM_SNIPPETS_CREATE,
            PERM_SNIPPETS_UPDATE_OWN,
            PERM_SNIPPETS_DELETE_OWN,
            PERM_FOLDERS_CREATE,
            PERM_FOLDERS_UPDATE,
            PERM_DATA_SOURCES_MANAGE,
            PERM_ANALYTICS_VIEW,
        ],
        WorkspaceRole::Viewer => &[
            PERM_SNIPPETS_READ,
            PERM_ANALYTICS_VIEW,
        ],
        WorkspaceRole::Member => &[
            PERM_SNIPPETS_READ,
        ],
    }
}

// ─── Permission Checking ─────────────────────────────────────────────────────────

/// Check if the current user has a specific permission.
///
/// Logic:
/// 1. Owner always passes (all permissions).
/// 2. If custom permissions are set (non-null), check the list.
/// 3. If no custom permissions, use role-based defaults.
///
/// Returns `Ok(())` if permitted, `Err(AppError::Forbidden)` otherwise.
pub fn check_permission(ctx: &WorkspaceContext, permission: &str) -> Result<(), AppError> {
    // Owner bypasses all permission checks
    if ctx.role == WorkspaceRole::Owner {
        return Ok(());
    }

    // If custom permissions are set, use them exclusively
    if let Some(ref perms) = ctx.permissions {
        if perms.iter().any(|p| p == permission) {
            return Ok(());
        }
        return Err(AppError::Forbidden);
    }

    // Fallback: role-based defaults
    let defaults = default_permissions_for_role(ctx.role);
    if defaults.contains(&permission) {
        return Ok(());
    }

    Err(AppError::Forbidden)
}

/// Check if the user can modify a specific entity (ownership check + permission).
///
/// For "update_own" / "delete_own" permissions:
/// - If user has `*_any` permission → always allowed
/// - If user has `*_own` permission → only if they own the entity
/// - Otherwise → forbidden
pub fn check_entity_permission(
    ctx: &WorkspaceContext,
    own_permission: &str,
    any_permission: &str,
    entity_owner_id: uuid::Uuid,
) -> Result<(), AppError> {
    // Owner bypasses
    if ctx.role == WorkspaceRole::Owner {
        return Ok(());
    }

    // Check "any" permission first (more permissive)
    if has_permission(ctx, any_permission) {
        return Ok(());
    }

    // Check "own" permission + ownership
    if has_permission(ctx, own_permission) && entity_owner_id == ctx.user_id {
        return Ok(());
    }

    Err(AppError::Forbidden)
}

/// Internal helper: check if a user has a permission (without returning error).
fn has_permission(ctx: &WorkspaceContext, permission: &str) -> bool {
    if ctx.role == WorkspaceRole::Owner {
        return true;
    }

    if let Some(ref perms) = ctx.permissions {
        return perms.iter().any(|p| p == permission);
    }

    let defaults = default_permissions_for_role(ctx.role);
    defaults.contains(&permission)
}

/// Validate that all permission strings in a list are known/valid.
pub fn validate_permissions(permissions: &[String]) -> Result<(), AppError> {
    for perm in permissions {
        if !ALL_PERMISSIONS.contains(&perm.as_str()) {
            return Err(AppError::ValidationError {
                details: vec![crate::errors::FieldError {
                    field: "permissions".into(),
                    message: format!("Unknown permission: '{}'", perm),
                }],
            });
        }
    }
    Ok(())
}
