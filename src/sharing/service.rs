//! Snippet sharing service (Feature 1).
//!
//! Creates an independent, recipient-owned **copy** of a source snippet. A
//! shared snippet is a value copy (trigger, content, snippet_type, ai_prompt)
//! with no foreign key back to the source, so later edits to the source never
//! affect the recipient's copy (Req 1.2).
//!
//! The service reuses the recipient lookup and the same trigger-uniqueness /
//! content-size rules already enforced by [`SyncService`], rather than
//! re-implementing them.
//!
//! NOTE: This module is scaffolding for task 4.1. The struct/type definitions
//! and the `SharingService::new` constructor are complete; `share_snippet`'s
//! full transactional logic is implemented in task 4.2.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::collaborators::access;
use crate::errors::AppError;
use crate::notifications::service::{CreateNotification, NotificationService};
use crate::sync::service::{CreateSnippetPayload, SyncService};

/// Request to share a snippet to a recipient.
///
/// `recipient` is resolved via the existing user-lookup mechanism (extended to
/// allow email lookup); `target_workspace_id` is the recipient's workspace the
/// copy is created in.
#[derive(Debug, Clone, Deserialize)]
pub struct ShareRequest {
    pub source_snippet_id: Uuid,
    /// Email or user id — resolved via the existing user-lookup mechanism.
    pub recipient: String,
    /// The recipient's target workspace the copied snippet is created in.
    pub target_workspace_id: Uuid,
}

/// Successful result of a share: the newly created recipient-owned copy.
#[derive(Debug, Clone, Serialize)]
pub struct SharedCopy {
    /// Identifier of the newly created recipient-owned snippet.
    pub snippet_id: Uuid,
    /// The resolved recipient user id the copy is owned by.
    pub recipient_id: Uuid,
    /// The trigger of the copied snippet.
    pub trigger: String,
}

/// Request to share every snippet in a folder to a recipient.
///
/// Same recipient/target semantics as [`ShareRequest`], but scoped to a folder:
/// each of the folder's non-deleted snippets is shared as an independent
/// recipient-owned copy.
#[derive(Debug, Clone, Deserialize)]
pub struct ShareFolderRequest {
    pub source_folder_id: Uuid,
    /// Email or user id — resolved via the existing user-lookup mechanism.
    pub recipient: String,
    /// The recipient's target workspace the copied snippets are created in.
    pub target_workspace_id: Uuid,
}

/// Per-snippet outcome within a folder share (partial success is expected: a
/// trigger conflict on one snippet must not abort the rest).
#[derive(Debug, Clone, Serialize)]
pub struct FolderShareItem {
    /// The source snippet's trigger (identifies the row for the UI).
    pub trigger: String,
    /// `"shared"` (copied), `"skipped"` (already present / trigger conflict), or
    /// `"failed"` (any other error — e.g. content too long).
    pub status: String,
    /// The new recipient-owned snippet id, present only when `status == "shared"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet_id: Option<Uuid>,
    /// A short reason when not shared (for the UI's per-row hint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Aggregate result of a folder share.
#[derive(Debug, Clone, Serialize)]
pub struct FolderShareResponse {
    pub recipient_id: Uuid,
    pub total: usize,
    pub shared: usize,
    pub skipped: usize,
    pub failed: usize,
    pub items: Vec<FolderShareItem>,
}

/// Service that creates recipient-owned copies of shared snippets.
///
/// Reuses [`SyncService`] for the recipient workspace's trigger-uniqueness and
/// tier content-size enforcement.
#[derive(Clone)]
pub struct SharingService {
    pool: PgPool,
    sync: Arc<SyncService>,
}

impl SharingService {
    /// Create a new `SharingService`.
    pub fn new(pool: PgPool, sync: Arc<SyncService>) -> Self {
        Self { pool, sync }
    }

    /// Share a snippet: create an independent recipient-owned copy.
    ///
    /// Requirements 1.1–1.5, 2.1–2.3.
    ///
    /// Steps (ordered so that any pre-copy failure — unresolvable recipient,
    /// missing read access — creates no snippet, and the actual copy is written
    /// atomically by [`SyncService::create_snippet`], which commits only on
    /// success so a trigger conflict or oversize content leaves nothing behind):
    ///
    /// 1. Load the source snippet (Req 1.1). A missing source is a 403 rather
    ///    than a leak of existence to a non-reader.
    /// 2. Verify the requester has read access to the source via the same
    ///    collaborator access resolution `SyncService` uses. No access → 403
    ///    (Req 1.5).
    /// 3. Resolve `req.recipient` (email or user id) via the existing user
    ///    lookup, extended to match email. Unresolved → `RecipientNotFound` and
    ///    no write (Req 1.3, 1.4).
    /// 4. Copy `trigger, content, snippet_type, ai_prompt` into a NEW
    ///    recipient-owned snippet in `target_workspace_id` with
    ///    `created_by = recipient_id` — a value copy with no FK to the source
    ///    (Req 1.1, 1.2), delegating to `SyncService::create_snippet` so the
    ///    recipient workspace's trigger-uniqueness (409, Req 2.1/2.2) and tier
    ///    content-size (Req 2.3) rules are reused rather than duplicated.
    /// 5. Emit a share notification to the recipient (Feature 7), non-blocking:
    ///    a notification failure must not undo or fail the completed share.
    pub async fn share_snippet(
        &self,
        requester_id: Uuid,
        req: ShareRequest,
    ) -> Result<SharedCopy, AppError> {
        // ── 1. Load the source snippet (value fields only; no FK is copied). ──
        let source = sqlx::query_as::<_, SourceSnippet>(
            r#"
            SELECT id, workspace_id, trigger, content, snippet_type, ai_prompt
            FROM snippets
            WHERE id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(req.source_snippet_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        // A source that does not exist is reported as an access failure (403)
        // rather than 404 so the endpoint does not disclose snippet existence to
        // a caller who would not be allowed to read it anyway (Req 1.5).
        let source = source.ok_or(AppError::ResourceAccessDenied)?;

        // ── 2. Read-access check on the source (Req 1.5). ────────────────────
        // Reuse the same collaborator access resolution SyncService relies on:
        // owner bypass returns Edit; a collaborator with any grant (View/Edit)
        // resolved most-specific-first returns Some(_). None ⇒ no read access.
        let folder_ids = access::folders_of_snippet(&self.pool, source.id).await?;
        let effective = access::effective_access(
            &self.pool,
            requester_id,
            source.workspace_id,
            &folder_ids,
            Some(source.id),
        )
        .await?;
        if effective.is_none() {
            return Err(AppError::ResourceAccessDenied);
        }

        // ── 3. Resolve the recipient (Req 1.3, 1.4). ─────────────────────────
        // Extends the existing `lookup_users_handler` id lookup to also match by
        // email, so a share request may name a recipient by either. Unresolved
        // ⇒ RecipientNotFound and (because this precedes any write) no snippet.
        let recipient_id = self.resolve_recipient(&req.recipient).await?;

        // ── 4. Create the recipient-owned value copy (Req 1.1, 1.2, 2.1–2.3). ─
        // Delegating to SyncService::create_snippet reuses that workspace's
        // trigger-uniqueness (TriggerAlreadyExists / TriggerPrefixConflict → 409
        // identifying the trigger) and tier content-size enforcement
        // (SnippetContentTooLong), and is itself transactional — committing only
        // on success, so a conflict or oversize copy leaves no row behind.
        let created = self
            .sync
            .create_snippet(
                recipient_id,
                CreateSnippetPayload {
                    workspace_id: req.target_workspace_id,
                    trigger: source.trigger.clone(),
                    content: source.content.clone(),
                    snippet_type: source.snippet_type.clone(),
                    folder_id: None,
                    ai_prompt: source.ai_prompt.clone(),
                },
            )
            .await?;

        // ── 5. Emit a share notification to the recipient (Feature 7). ───────
        // Non-blocking: the share has already succeeded and must not be failed
        // or rolled back if the notification write fails. A fresh
        // NotificationService is built from the pool (its `create` applies the
        // recipient's preferences and bounded retry internally).
        let notifications = NotificationService::new(self.pool.clone());
        let notif = CreateNotification {
            user_id: recipient_id,
            workspace_id: req.target_workspace_id,
            event_type: "snippet_shared".to_string(),
            title: "A snippet was shared with you".to_string(),
            description: Some(format!(
                "You received a shared snippet with trigger \"{}\".",
                created.trigger
            )),
            metadata: Some(serde_json::json!({
                "snippet_id": created.id,
                "trigger": created.trigger,
                "shared_by": requester_id,
            })),
            classification: Some("system".to_string()),
        };
        tokio::spawn(async move {
            if let Err(e) = notifications.create(notif).await {
                tracing::error!(error = %e, "Failed to emit snippet-share notification");
            }
        });

        Ok(SharedCopy {
            snippet_id: created.id,
            recipient_id,
            trigger: created.trigger,
        })
    }

    /// Share every non-deleted snippet in a folder to a recipient.
    ///
    /// Reuses [`Self::share_snippet`] per snippet so all the same rules apply
    /// (read-access on the source, recipient resolution, and the recipient
    /// workspace's trigger-uniqueness / content-size enforcement). Partial
    /// success is expected and desired: a trigger conflict on one snippet is
    /// recorded as `skipped` and the remaining snippets are still shared, rather
    /// than aborting the whole folder.
    ///
    /// Pre-copy failures that apply to the whole request (unresolvable recipient,
    /// or a folder the requester cannot read) fail fast with no copies made.
    pub async fn share_folder(
        &self,
        requester_id: Uuid,
        req: ShareFolderRequest,
    ) -> Result<FolderShareResponse, AppError> {
        // ── 1. Resolve the folder + its workspace; check requester read access. ─
        // A folder the requester cannot see is reported as an access failure
        // rather than 404 (consistent with share_snippet, so folder existence is
        // not disclosed).
        let workspace_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT workspace_id FROM folders WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(req.source_folder_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;
        let workspace_id = workspace_id.ok_or(AppError::ResourceAccessDenied)?;

        let effective = access::effective_access(
            &self.pool,
            requester_id,
            workspace_id,
            &[req.source_folder_id],
            None,
        )
        .await?;
        if effective.is_none() {
            return Err(AppError::ResourceAccessDenied);
        }

        // ── 2. Resolve the recipient once (fail fast; no copies on failure). ──
        let recipient_id = self.resolve_recipient(&req.recipient).await?;

        // ── 3. Collect the folder's non-deleted snippets. ───────────────────
        let snippet_ids: Vec<(Uuid, String)> = sqlx::query_as(
            r#"
            SELECT s.id, s.trigger
            FROM snippets s
            JOIN snippet_folders sf ON sf.snippet_id = s.id
            WHERE sf.folder_id = $1 AND s.deleted_at IS NULL
            ORDER BY s.trigger
            "#,
        )
        .bind(req.source_folder_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        // ── 4. Share each snippet, recording per-item outcomes. ──────────────
        let mut items: Vec<FolderShareItem> = Vec::with_capacity(snippet_ids.len());
        let (mut shared, mut skipped, mut failed) = (0usize, 0usize, 0usize);

        for (snippet_id, trigger) in &snippet_ids {
            let result = self
                .share_snippet(
                    requester_id,
                    ShareRequest {
                        source_snippet_id: *snippet_id,
                        recipient: recipient_id.to_string(),
                        target_workspace_id: req.target_workspace_id,
                    },
                )
                .await;

            match result {
                Ok(copy) => {
                    shared += 1;
                    items.push(FolderShareItem {
                        trigger: trigger.clone(),
                        status: "shared".to_string(),
                        snippet_id: Some(copy.snippet_id),
                        reason: None,
                    });
                }
                // A trigger already present in the recipient workspace is an
                // expected "skip", not a hard failure.
                Err(AppError::TriggerAlreadyExists)
                | Err(AppError::TriggerPrefixConflict) => {
                    skipped += 1;
                    items.push(FolderShareItem {
                        trigger: trigger.clone(),
                        status: "skipped".to_string(),
                        snippet_id: None,
                        reason: Some("Trigger already exists in the target workspace".to_string()),
                    });
                }
                Err(e) => {
                    failed += 1;
                    items.push(FolderShareItem {
                        trigger: trigger.clone(),
                        status: "failed".to_string(),
                        snippet_id: None,
                        reason: Some(e.to_string()),
                    });
                }
            }
        }

        Ok(FolderShareResponse {
            recipient_id,
            total: snippet_ids.len(),
            shared,
            skipped,
            failed,
            items,
        })
    }

    /// Resolve a recipient identifier (user id or email) to an existing user id.
    ///
    /// Reuses the `users` table lookup behind `lookup_users_handler` (match by
    /// `id`) and extends it to also match by `email`, so a share request may
    /// address a recipient by either form. Returns [`AppError::RecipientNotFound`]
    /// when the identifier resolves to no user (Req 1.4).
    async fn resolve_recipient(&self, recipient: &str) -> Result<Uuid, AppError> {
        let recipient = recipient.trim();

        // Try an id match first when the identifier parses as a UUID.
        if let Ok(uuid) = Uuid::parse_str(recipient) {
            let by_id: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
                    .bind(uuid)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|_| AppError::InternalError)?;
            if let Some(id) = by_id {
                return Ok(id);
            }
        }

        // Fall back to a case-insensitive email match.
        let by_email: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM users WHERE lower(email) = lower($1)")
                .bind(recipient)
                .fetch_optional(&self.pool)
                .await
                .map_err(|_| AppError::InternalError)?;

        by_email.ok_or(AppError::RecipientNotFound)
    }
}

/// Source snippet value fields loaded for copying (no identity/FK fields are
/// carried into the copy — the copy is independent of the source, Req 1.2).
#[derive(Debug, sqlx::FromRow)]
struct SourceSnippet {
    id: Uuid,
    workspace_id: Uuid,
    trigger: String,
    content: String,
    snippet_type: String,
    ai_prompt: Option<String>,
}
