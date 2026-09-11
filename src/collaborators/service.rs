//! `CollaboratorService`: invite/accept lifecycle, grant management, and
//! effective-access resolution used by `SyncService` to enforce reads/writes.

use std::sync::Arc;

use chrono::{Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::service::{generate_refresh_token, hash_token};
use crate::config::AppConfig;
use crate::email::{service::EmailService, templates};
use crate::errors::AppError;
use crate::notifications::service::{CreateNotification, NotificationService};

use super::{access, CollaboratorInfo, GrantInfo, InviteResponse, Level, ResourceType};

/// Invite time-to-live.
const INVITE_TTL_DAYS: i64 = 7;

pub struct CollaboratorService {
    pool: PgPool,
    config: Arc<AppConfig>,
    email_service: EmailService,
}

impl CollaboratorService {
    pub fn new(pool: PgPool, config: Arc<AppConfig>) -> Self {
        let email_service = EmailService::new(config.clone());
        Self {
            pool,
            config,
            email_service,
        }
    }

    // ── Teams gating ──────────────────────────────────────────────────────

    /// True if `owner_id` has an active Teams subscription on any workspace they own.
    pub async fn account_is_teams_enabled(&self, owner_id: Uuid) -> Result<bool, AppError> {
        access::account_is_teams_enabled(&self.pool, owner_id).await
    }

    /// Ensure the owner is Teams-enabled, else `TeamsPlanRequired`.
    async fn require_teams(&self, owner_id: Uuid) -> Result<(), AppError> {
        if self.account_is_teams_enabled(owner_id).await? {
            Ok(())
        } else {
            Err(AppError::TeamsPlanRequired)
        }
    }

    // ── Invite / accept lifecycle ─────────────────────────────────────────

    /// Invite a collaborator by email. Teams-gated. Stores only the token hash
    /// and emails the raw token as an accept link. The plaintext token is never
    /// logged or persisted.
    pub async fn invite(&self, owner_id: Uuid, email: &str) -> Result<InviteResponse, AppError> {
        self.require_teams(owner_id).await?;

        let email = email.trim().to_lowercase();
        if email.is_empty() || !email.contains('@') {
            return Err(AppError::CollaboratorInviteInvalid);
        }

        let raw_token = generate_refresh_token();
        let token_hash = hash_token(&raw_token);
        let expires_at = Utc::now() + Duration::days(INVITE_TTL_DAYS);

        let id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO collaborator_invites (owner_id, email, token_hash, expires_at)
            VALUES ($1, $2, $3, $4)
            RETURNING id
            "#,
        )
        .bind(owner_id)
        .bind(&email)
        .bind(&token_hash)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        // Resolve the owner's display name (fall back to their email) for the email body.
        let owner_name: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
            .bind(owner_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)?
            .unwrap_or_else(|| self.config.email_from_name.clone());

        let base = self.config.frontend_url.trim_end_matches('/');
        let invite_link = format!("{}/collaborators/accept?token={}", base, raw_token);
        let message = templates::collaborator_invite_email(
            &email,
            &invite_link,
            &owner_name,
            &self.config.email_from_name,
        );
        self.email_service.send_email(message);

        // ── Workspace-invite notifications (Feature 7, Req 21.1 / 21.2) ──────
        // Emit a System_Notification for the sender recording the sent invite
        // (Req 21.1), and — when the invited email resolves to an existing user
        // — for the receiver recording the received invite (Req 21.2). Both are
        // fire-and-forget: a notification write failure must never fail or roll
        // back the completed invite. A fresh `NotificationService` is built from
        // the pool (its `create` applies each user's preferences and bounded
        // retry internally), mirroring the sharing service's convention.
        self.emit_invite_notifications(owner_id, &email, id).await;

        Ok(InviteResponse {
            id,
            email,
            expires_at,
        })
    }

    /// Emit the sender/receiver workspace-invite notifications (Req 21.1, 21.2).
    ///
    /// Non-blocking: notification writes are spawned so that a failure cannot
    /// fail or roll back the already-completed invite. Each notification is
    /// anchored to the sender's individual workspace, which is the account-level
    /// context the invite originates from (collaborator invites are account
    /// level, but the `notifications` table requires a workspace anchor).
    ///
    /// - Sender (Req 21.1): always created for `owner_id`.
    /// - Receiver (Req 21.2): created only when `email` resolves to an existing
    ///   user account. A not-yet-registered invitee has no user row to notify;
    ///   they learn of the invite via the emailed accept link and will see it
    ///   once they have an account.
    async fn emit_invite_notifications(&self, owner_id: Uuid, email: &str, invite_id: Uuid) {
        // Anchor workspace: the sender's individual workspace. If it cannot be
        // resolved we skip notifications rather than failing the invite.
        let anchor_workspace_id = match self.individual_workspace_id(owner_id).await {
            Ok(Some(ws)) => ws,
            Ok(None) => {
                tracing::warn!(
                    owner_id = %owner_id,
                    "No individual workspace to anchor invite notifications; skipping"
                );
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to resolve anchor workspace for invite notifications");
                return;
            }
        };

        // Resolve the invited email to an existing user for the receiver
        // notification (Req 21.2). Absent user ⇒ no receiver notification.
        let receiver_id = match self.user_id_by_email(email).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = %e, "Failed to resolve invitee for invite notifications");
                None
            }
        };

        let notifications = NotificationService::new(self.pool.clone());
        let email = email.to_string();

        // Sender notification (Req 21.1) — always.
        let sender_notif = CreateNotification {
            user_id: owner_id,
            workspace_id: anchor_workspace_id,
            event_type: "workspace_invite_sent".to_string(),
            title: "Invite sent".to_string(),
            description: Some(format!("You invited {email} to collaborate.")),
            metadata: Some(serde_json::json!({
                "invite_id": invite_id,
                "invited_email": email,
            })),
            classification: Some("system".to_string()),
        };

        // Receiver notification (Req 21.2) — only when the invitee has an account.
        let receiver_notif = receiver_id.map(|rid| CreateNotification {
            user_id: rid,
            workspace_id: anchor_workspace_id,
            event_type: "workspace_invite_received".to_string(),
            title: "You have a new invite".to_string(),
            description: Some("You were invited to collaborate.".to_string()),
            metadata: Some(serde_json::json!({
                "invite_id": invite_id,
                "invited_by": owner_id,
            })),
            classification: Some("system".to_string()),
        });

        tokio::spawn(async move {
            if let Err(e) = notifications.create(sender_notif).await {
                tracing::error!(error = %e, "Failed to emit workspace-invite (sender) notification");
            }
            if let Some(receiver_notif) = receiver_notif {
                if let Err(e) = notifications.create(receiver_notif).await {
                    tracing::error!(error = %e, "Failed to emit workspace-invite (receiver) notification");
                }
            }
        });
    }

    /// Resolve a user's individual (personal) workspace id, if any.
    async fn individual_workspace_id(&self, owner_id: Uuid) -> Result<Option<Uuid>, AppError> {
        sqlx::query_scalar(
            r#"
            SELECT id FROM workspaces
            WHERE owner_id = $1 AND type = 'individual'
            ORDER BY created_at
            LIMIT 1
            "#,
        )
        .bind(owner_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)
    }

    /// Resolve an existing user's id from their email (case-insensitive).
    /// Returns `None` when no account matches (a not-yet-registered invitee).
    async fn user_id_by_email(&self, email: &str) -> Result<Option<Uuid>, AppError> {
        sqlx::query_scalar("SELECT id FROM users WHERE lower(email) = lower($1)")
            .bind(email)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)
    }

    /// Accept an invite. Requires the authenticated user's email to match the
    /// invite email. Validates not-used/not-expired. Idempotently creates an
    /// accepted collaboration and marks the invite used.
    pub async fn accept_invite(
        &self,
        user_id: Uuid,
        user_email: &str,
        raw_token: &str,
    ) -> Result<(), AppError> {
        let token_hash = hash_token(raw_token);

        let row: Option<(Uuid, Uuid, String, bool, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as(
                r#"
                SELECT id, owner_id, email, used, expires_at
                FROM collaborator_invites
                WHERE token_hash = $1
                "#,
            )
            .bind(&token_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)?;

        let (invite_id, owner_id, invite_email, used, expires_at) =
            row.ok_or(AppError::CollaboratorInviteInvalid)?;

        if used || expires_at < Utc::now() {
            return Err(AppError::CollaboratorInviteInvalid);
        }
        if invite_email.to_lowercase() != user_email.trim().to_lowercase() {
            return Err(AppError::CollaboratorInviteInvalid);
        }
        // A user cannot collaborate with themselves.
        if owner_id == user_id {
            return Err(AppError::CollaboratorInviteInvalid);
        }

        let mut tx = self.pool.begin().await.map_err(|_| AppError::InternalError)?;

        sqlx::query(
            r#"
            INSERT INTO collaborations (owner_id, collaborator_id, status)
            VALUES ($1, $2, 'accepted')
            ON CONFLICT (owner_id, collaborator_id) DO NOTHING
            "#,
        )
        .bind(owner_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| AppError::InternalError)?;

        sqlx::query("UPDATE collaborator_invites SET used = true WHERE id = $1")
            .bind(invite_id)
            .execute(&mut *tx)
            .await
            .map_err(|_| AppError::InternalError)?;

        tx.commit().await.map_err(|_| AppError::InternalError)?;
        Ok(())
    }

    /// List accepted collaborators plus pending (unaccepted, unexpired) invites.
    pub async fn list_collaborators(
        &self,
        owner_id: Uuid,
    ) -> Result<Vec<CollaboratorInfo>, AppError> {
        let accepted: Vec<(Uuid, String)> = sqlx::query_as(
            r#"
            SELECT c.collaborator_id, u.email
            FROM collaborations c
            INNER JOIN users u ON u.id = c.collaborator_id
            WHERE c.owner_id = $1
            ORDER BY u.email
            "#,
        )
        .bind(owner_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        let pending: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT DISTINCT email
            FROM collaborator_invites
            WHERE owner_id = $1 AND used = false AND expires_at > now()
            ORDER BY email
            "#,
        )
        .bind(owner_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        let accepted_emails: std::collections::HashSet<String> =
            accepted.iter().map(|(_, e)| e.to_lowercase()).collect();

        let mut out: Vec<CollaboratorInfo> = accepted
            .into_iter()
            .map(|(uid, email)| CollaboratorInfo {
                user_id: Some(uid),
                email,
                status: "accepted".to_string(),
            })
            .collect();

        for (email,) in pending {
            if !accepted_emails.contains(&email.to_lowercase()) {
                out.push(CollaboratorInfo {
                    user_id: None,
                    email,
                    status: "pending".to_string(),
                });
            }
        }

        Ok(out)
    }

    /// Remove a collaborator: delete the collaboration and all of that
    /// collaborator's grants under this owner, in one transaction.
    pub async fn remove_collaborator(
        &self,
        owner_id: Uuid,
        collaborator_id: Uuid,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await.map_err(|_| AppError::InternalError)?;

        sqlx::query(
            "DELETE FROM access_grants WHERE owner_id = $1 AND collaborator_id = $2",
        )
        .bind(owner_id)
        .bind(collaborator_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| AppError::InternalError)?;

        let result = sqlx::query(
            "DELETE FROM collaborations WHERE owner_id = $1 AND collaborator_id = $2",
        )
        .bind(owner_id)
        .bind(collaborator_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| AppError::InternalError)?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotAccountOwner);
        }

        tx.commit().await.map_err(|_| AppError::InternalError)?;
        Ok(())
    }

    // ── Grants ────────────────────────────────────────────────────────────

    /// List all grants an owner has issued to a specific collaborator.
    pub async fn list_grants(
        &self,
        owner_id: Uuid,
        collaborator_id: Uuid,
    ) -> Result<Vec<GrantInfo>, AppError> {
        let rows: Vec<(Uuid, String, Uuid, String)> = sqlx::query_as(
            r#"
            SELECT id, resource_type, resource_id, level
            FROM access_grants
            WHERE owner_id = $1 AND collaborator_id = $2
            ORDER BY resource_type, created_at
            "#,
        )
        .bind(owner_id)
        .bind(collaborator_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        Ok(rows
            .into_iter()
            .map(|(id, resource_type, resource_id, level)| GrantInfo {
                id,
                resource_type,
                resource_id,
                level,
            })
            .collect())
    }

    /// Create or update a grant. Teams-gated. Validates that:
    /// - `level` and `resource_type` are valid,
    /// - an accepted collaboration exists between owner and collaborator,
    /// - the target resource is owned by `owner_id`.
    pub async fn upsert_grant(
        &self,
        owner_id: Uuid,
        collaborator_id: Uuid,
        resource_type: &str,
        resource_id: Uuid,
        level: &str,
    ) -> Result<GrantInfo, AppError> {
        self.require_teams(owner_id).await?;

        let rtype = ResourceType::parse(resource_type).ok_or(AppError::CollaboratorInviteInvalid)?;
        let level_enum = Level::parse(level).ok_or(AppError::CollaboratorInviteInvalid)?;

        // Must be an accepted collaborator of this owner.
        let is_collaborator: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM collaborations
                WHERE owner_id = $1 AND collaborator_id = $2
            )
            "#,
        )
        .bind(owner_id)
        .bind(collaborator_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;
        if !is_collaborator {
            return Err(AppError::NotAccountOwner);
        }

        // Validate the resource is owned by owner_id.
        if !self
            .owner_owns_resource(owner_id, rtype, resource_id)
            .await?
        {
            return Err(AppError::NotAccountOwner);
        }

        let id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO access_grants (owner_id, collaborator_id, resource_type, resource_id, level)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (collaborator_id, resource_type, resource_id)
            DO UPDATE SET level = EXCLUDED.level, updated_at = now()
            RETURNING id
            "#,
        )
        .bind(owner_id)
        .bind(collaborator_id)
        .bind(rtype.as_str())
        .bind(resource_id)
        .bind(level_enum.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        Ok(GrantInfo {
            id,
            resource_type: rtype.as_str().to_string(),
            resource_id,
            level: level_enum.as_str().to_string(),
        })
    }

    /// Delete a grant the owner issued.
    pub async fn delete_grant(&self, owner_id: Uuid, grant_id: Uuid) -> Result<(), AppError> {
        let result = sqlx::query("DELETE FROM access_grants WHERE id = $1 AND owner_id = $2")
            .bind(grant_id)
            .bind(owner_id)
            .execute(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotAccountOwner);
        }
        Ok(())
    }

    /// Verify `owner_id` owns the given resource (via its workspace's owner_id).
    async fn owner_owns_resource(
        &self,
        owner_id: Uuid,
        rtype: ResourceType,
        resource_id: Uuid,
    ) -> Result<bool, AppError> {
        let sql = match rtype {
            ResourceType::Workspace => {
                "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id = $1 AND owner_id = $2)"
            }
            ResourceType::Folder => {
                "SELECT EXISTS(SELECT 1 FROM folders f INNER JOIN workspaces w ON w.id = f.workspace_id WHERE f.id = $1 AND w.owner_id = $2)"
            }
            ResourceType::Snippet => {
                "SELECT EXISTS(SELECT 1 FROM snippets s INNER JOIN workspaces w ON w.id = s.workspace_id WHERE s.id = $1 AND w.owner_id = $2)"
            }
        };
        let owns: bool = sqlx::query_scalar(sql)
            .bind(resource_id)
            .bind(owner_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)?;
        Ok(owns)
    }

    // ── Effective access resolution (delegates to the pool-only `access` module) ──

    /// Resolve the effective access `user_id` has for a resource in `workspace_id`.
    /// See [`access::effective_access`] for the algorithm.
    pub async fn effective_access(
        &self,
        user_id: Uuid,
        workspace_id: Uuid,
        folder_ids: &[Uuid],
        snippet_id: Option<Uuid>,
    ) -> Result<Option<Level>, AppError> {
        access::effective_access(&self.pool, user_id, workspace_id, folder_ids, snippet_id).await
    }

    /// The folder ids a snippet belongs to (via the `snippet_folders` join).
    pub async fn folders_of_snippet(&self, snippet_id: Uuid) -> Result<Vec<Uuid>, AppError> {
        access::folders_of_snippet(&self.pool, snippet_id).await
    }

    /// Delete any access grants that reference a now-deleted resource id.
    pub async fn cleanup_grants_for_resource(
        &self,
        rtype: ResourceType,
        resource_id: Uuid,
    ) -> Result<(), AppError> {
        access::cleanup_grants_for_resource(&self.pool, rtype, resource_id).await
    }
}
