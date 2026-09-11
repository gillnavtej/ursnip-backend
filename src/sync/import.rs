//! Snippet import parsing service (Feature 4, task 5.1).
//!
//! [`ImportService::parse`] turns a raw import payload (bytes or string) into a
//! list of [`CandidateSnippet`] values. Each candidate carries exactly the
//! fields of the canonical import schema and maps 1:1 onto a
//! [`BatchOperation::CreateSnippet`] once a target `workspace_id` is supplied.
//!
//! [`ImportService::ingest`] wires those candidates into the existing batch
//! ingestion path (the same validations used by `POST /sync/snippets/batch`):
//! each candidate is mapped to a [`CreateSnippetPayload`] and created via
//! [`SyncService::create_snippet`], which enforces the workspace's trigger
//! uniqueness, prefix-conflict, and per-tier content-size rules (Req 4.3–4.5).
//! Unlike the all-or-nothing batch transaction, ingest does **not** abort the
//! whole import on the first bad item: successes are ingested and every
//! rule-violating candidate is reported as `{ index, trigger, rule }`
//! (Req 4.2, 4.7), matching the per-item `BatchItemResult` contract.
//!
//! Canonical schema:
//! ```json
//! {
//!   "version": 1,
//!   "workspace_name": "optional label",
//!   "snippets": [
//!     { "trigger": "/hi", "content": "hello", "snippet_type": "static", "ai_prompt": null }
//!   ]
//! }
//! ```
//!
//! Anything that is not valid JSON, or is valid JSON that does not match this
//! shape (missing required fields, wrong types, unknown fields), is rejected
//! with [`AppError::ImportParseError`] (HTTP 422) and **no** snippet is produced
//! — parsing is all-or-nothing, so a single malformed item ingests nothing
//! (Req 4.6).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::AppError;
use crate::sync::service::{BatchItemResult, BatchOperation, CreateSnippetPayload, SyncService};

// ─── Wire schema ────────────────────────────────────────────────────────────────

/// The top-level canonical import document.
///
/// `deny_unknown_fields` makes the shape strict: a payload that is valid JSON
/// but carries unexpected keys is treated as the wrong shape and rejected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportDocument {
    /// Schema version. Retained for forward-compatibility; required so a
    /// versionless blob is rejected as the wrong shape.
    #[allow(dead_code)]
    version: i64,
    /// Optional human-readable label for the exported workspace. Ignored by the
    /// parser (the import target workspace is chosen by the caller), but allowed
    /// so round-tripped exports parse cleanly.
    #[serde(default)]
    #[allow(dead_code)]
    workspace_name: Option<String>,
    snippets: Vec<ImportSnippet>,
}

/// A single snippet entry within an import document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportSnippet {
    trigger: String,
    content: String,
    snippet_type: String,
    #[serde(default)]
    ai_prompt: Option<String>,
}

// ─── Candidate output ─────────────────────────────────────────────────────────

/// A parsed, schema-valid snippet ready to be ingested.
///
/// Holds exactly the value fields of the canonical schema. It deliberately does
/// **not** carry a `workspace_id`: the import target is decided by the caller,
/// which supplies it via [`CandidateSnippet::into_create_payload`] when mapping
/// to a [`BatchOperation::CreateSnippet`] (task 5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSnippet {
    pub trigger: String,
    pub content: String,
    pub snippet_type: String,
    pub ai_prompt: Option<String>,
}

impl CandidateSnippet {
    /// Build the [`CreateSnippetPayload`] for this candidate targeting
    /// `workspace_id`. Imported snippets are never placed in a folder, so
    /// `folder_id` is always `None`.
    pub fn into_create_payload(self, workspace_id: Uuid) -> CreateSnippetPayload {
        CreateSnippetPayload {
            workspace_id,
            trigger: self.trigger,
            content: self.content,
            snippet_type: self.snippet_type,
            folder_id: None,
            ai_prompt: self.ai_prompt,
        }
    }

    /// Convenience wrapper mapping this candidate directly to the
    /// [`BatchOperation::CreateSnippet`] variant used by the batch endpoint.
    pub fn into_batch_operation(self, workspace_id: Uuid) -> BatchOperation {
        BatchOperation::CreateSnippet(self.into_create_payload(workspace_id))
    }
}

// ─── Ingestion report types ─────────────────────────────────────────────────────

/// The specific workspace rule a candidate snippet violated during ingestion.
///
/// These are exactly the rules the batch endpoint enforces per item
/// (Req 4.3–4.5): trigger uniqueness, trigger prefix-conflict, and the tier
/// content-size limit. `serde` renders them as stable snake_case strings so the
/// frontend can key its per-item violation report off `rule`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportRule {
    /// A snippet with the same trigger already exists in the workspace (Req 4.3).
    Uniqueness,
    /// The trigger is a prefix of, or has as a prefix, an existing trigger
    /// (Req 4.4).
    PrefixConflict,
    /// The content exceeds the user's tier content-size limit (Req 4.5).
    ContentSize,
}

/// A single candidate that could not be ingested, identified by its position in
/// the import payload, its trigger, and the rule it violated (Req 4.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportViolation {
    /// Zero-based index of the candidate within the submitted `snippets` array.
    pub index: usize,
    /// The trigger of the violating candidate, echoed back so the client can
    /// identify which entry failed without re-reading the payload.
    pub trigger: String,
    /// The specific workspace rule the candidate violated.
    pub rule: ImportRule,
}

/// Outcome of ingesting a parsed set of candidates through the batch path.
///
/// `ingested` holds the [`BatchItemResult`] for each candidate that was created
/// (same shape the batch endpoint returns), and `violations` reports every
/// candidate rejected for a recognised workspace rule (Req 4.7). Candidates
/// that neither succeed nor map to a known rule surface as a hard error from
/// [`ImportService::ingest`] instead of appearing here.
#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub ingested: Vec<BatchItemResult>,
    pub violations: Vec<ImportViolation>,
}

// ─── Service ────────────────────────────────────────────────────────────────────

/// Parses import payloads into candidate snippets (Req 4.1) and ingests them
/// through the existing batch path (Req 4.2).
///
/// [`parse`](ImportService::parse) is a stateless associated function; the type
/// is exposed as a service for symmetry with the other sync services.
pub struct ImportService;

impl ImportService {
    /// Parse a raw import `payload` into candidate snippets.
    ///
    /// Accepts anything convertible to a byte slice (`&str`, `String`,
    /// `Vec<u8>`, `&[u8]`), so callers can pass a request body directly.
    ///
    /// Returns [`AppError::ImportParseError`] (422) if `payload` is not valid
    /// UTF-8 JSON or does not match the canonical schema. On any such failure no
    /// candidates are produced — ingestion is all-or-nothing (Req 4.6).
    pub fn parse(payload: impl AsRef<[u8]>) -> Result<Vec<CandidateSnippet>, AppError> {
        let bytes = payload.as_ref();

        let document: ImportDocument =
            serde_json::from_slice(bytes).map_err(|_| AppError::ImportParseError)?;

        let candidates = document
            .snippets
            .into_iter()
            .map(|s| CandidateSnippet {
                trigger: s.trigger,
                content: s.content,
                snippet_type: s.snippet_type,
                ai_prompt: s.ai_prompt,
            })
            .collect();

        Ok(candidates)
    }

    /// Ingest parsed `candidates` into `workspace_id` as `user_id`, reusing the
    /// batch endpoint's snippet-creation path (Req 4.2).
    ///
    /// Each candidate is mapped to a [`CreateSnippetPayload`] and created via
    /// [`SyncService::create_snippet`], which applies the same workspace rules
    /// the batch endpoint enforces per item: trigger uniqueness (Req 4.3),
    /// trigger prefix-conflict (Req 4.4), and the tier content-size limit
    /// (Req 4.5). `create_snippet` is itself transactional and commits only on
    /// success, so a rejected candidate leaves no row behind.
    ///
    /// Rather than aborting the whole import on the first bad item (as the
    /// all-or-nothing batch transaction would), this collects every successful
    /// creation into [`ImportReport::ingested`] and every candidate rejected by
    /// a recognised workspace rule into [`ImportReport::violations`] as
    /// `{ index, trigger, rule }` (Req 4.7).
    ///
    /// Errors that are not per-item rule violations (workspace not found,
    /// subscription missing, authorization failures, or any internal error)
    /// short-circuit and are returned as an [`AppError`]: they concern the
    /// import as a whole, not an individual candidate.
    pub async fn ingest(
        sync: &SyncService,
        user_id: Uuid,
        workspace_id: Uuid,
        candidates: Vec<CandidateSnippet>,
    ) -> Result<ImportReport, AppError> {
        let mut ingested: Vec<BatchItemResult> = Vec::new();
        let mut violations: Vec<ImportViolation> = Vec::new();

        for (index, candidate) in candidates.into_iter().enumerate() {
            let trigger = candidate.trigger.clone();
            let payload = candidate.into_create_payload(workspace_id);

            match sync.create_snippet(user_id, payload).await {
                Ok(snippet) => {
                    let version = snippet.version;
                    ingested.push(BatchItemResult {
                        index,
                        snippet: Some(snippet),
                        version,
                    });
                }
                Err(err) => match Self::rule_for_error(&err) {
                    // A recognised per-item workspace rule: record and continue
                    // so the rest of the import still proceeds (Req 4.7).
                    Some(rule) => violations.push(ImportViolation {
                        index,
                        trigger,
                        rule,
                    }),
                    // Anything else (not-found, no subscription, auth, internal)
                    // is an import-wide failure — propagate it.
                    None => return Err(err),
                },
            }
        }

        Ok(ImportReport {
            ingested,
            violations,
        })
    }

    /// Map an [`AppError`] from snippet creation to the per-item [`ImportRule`]
    /// it represents, or `None` if the error is not a per-item workspace-rule
    /// violation.
    ///
    /// `SnippetContentTooLong` and `ContentSoftLocked` both mean the content
    /// exceeded the applicable size limit (the latter when a lapsed
    /// subscription pins the workspace to free-tier limits), so both map to
    /// [`ImportRule::ContentSize`] (Req 4.5).
    fn rule_for_error(err: &AppError) -> Option<ImportRule> {
        match err {
            AppError::TriggerAlreadyExists => Some(ImportRule::Uniqueness),
            AppError::TriggerPrefixConflict => Some(ImportRule::PrefixConflict),
            AppError::SnippetContentTooLong | AppError::ContentSoftLocked => {
                Some(ImportRule::ContentSize)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_payload() {
        let payload = r#"{
            "version": 1,
            "workspace_name": "My Workspace",
            "snippets": [
                {"trigger": "/hi", "content": "hello", "snippet_type": "static", "ai_prompt": null},
                {"trigger": "/ai", "content": "", "snippet_type": "ai", "ai_prompt": "summarize"}
            ]
        }"#;

        let candidates = ImportService::parse(payload).expect("valid payload should parse");

        assert_eq!(
            candidates,
            vec![
                CandidateSnippet {
                    trigger: "/hi".into(),
                    content: "hello".into(),
                    snippet_type: "static".into(),
                    ai_prompt: None,
                },
                CandidateSnippet {
                    trigger: "/ai".into(),
                    content: "".into(),
                    snippet_type: "ai".into(),
                    ai_prompt: Some("summarize".into()),
                },
            ]
        );
    }

    #[test]
    fn workspace_name_is_optional() {
        let payload = r#"{"version": 1, "snippets": []}"#;
        let candidates = ImportService::parse(payload).expect("payload without workspace_name parses");
        assert!(candidates.is_empty());
    }

    #[test]
    fn ai_prompt_is_optional() {
        let payload =
            r#"{"version": 1, "snippets": [{"trigger": "/x", "content": "c", "snippet_type": "static"}]}"#;
        let candidates = ImportService::parse(payload).expect("payload without ai_prompt parses");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].ai_prompt, None);
    }

    #[test]
    fn rejects_invalid_json() {
        // Truncated / malformed JSON.
        assert!(matches!(
            ImportService::parse("{ not json"),
            Err(AppError::ImportParseError)
        ));
        assert!(matches!(
            ImportService::parse(""),
            Err(AppError::ImportParseError)
        ));
    }

    #[test]
    fn rejects_valid_json_wrong_shape() {
        // Missing the `snippets` array entirely.
        assert!(matches!(
            ImportService::parse(r#"{"version": 1}"#),
            Err(AppError::ImportParseError)
        ));
        // `snippets` is not an array.
        assert!(matches!(
            ImportService::parse(r#"{"version": 1, "snippets": "nope"}"#),
            Err(AppError::ImportParseError)
        ));
        // A snippet missing a required field (`content`).
        assert!(matches!(
            ImportService::parse(
                r#"{"version": 1, "snippets": [{"trigger": "/x", "snippet_type": "static"}]}"#
            ),
            Err(AppError::ImportParseError)
        ));
        // Unknown field on a snippet → wrong shape.
        assert!(matches!(
            ImportService::parse(
                r#"{"version": 1, "snippets": [{"trigger": "/x", "content": "c", "snippet_type": "static", "bogus": true}]}"#
            ),
            Err(AppError::ImportParseError)
        ));
        // Missing required `version`.
        assert!(matches!(
            ImportService::parse(r#"{"snippets": []}"#),
            Err(AppError::ImportParseError)
        ));
        // Top-level is a JSON array, not the expected object.
        assert!(matches!(
            ImportService::parse(r#"[]"#),
            Err(AppError::ImportParseError)
        ));
    }

    #[test]
    fn accepts_byte_slice_input() {
        let bytes: &[u8] = br#"{"version": 1, "snippets": []}"#;
        assert!(ImportService::parse(bytes).is_ok());
    }

    #[test]
    fn rule_for_error_maps_per_item_violations() {
        assert_eq!(
            ImportService::rule_for_error(&AppError::TriggerAlreadyExists),
            Some(ImportRule::Uniqueness)
        );
        assert_eq!(
            ImportService::rule_for_error(&AppError::TriggerPrefixConflict),
            Some(ImportRule::PrefixConflict)
        );
        assert_eq!(
            ImportService::rule_for_error(&AppError::SnippetContentTooLong),
            Some(ImportRule::ContentSize)
        );
        // A lapsed subscription pins the workspace to free-tier size limits;
        // an oversize import item there is still a content-size violation.
        assert_eq!(
            ImportService::rule_for_error(&AppError::ContentSoftLocked),
            Some(ImportRule::ContentSize)
        );
    }

    #[test]
    fn rule_for_error_leaves_import_wide_errors_unmapped() {
        // These concern the import as a whole, not a single candidate, so they
        // must propagate as a hard error rather than becoming a violation.
        assert_eq!(
            ImportService::rule_for_error(&AppError::WorkspaceNotFound),
            None
        );
        assert_eq!(
            ImportService::rule_for_error(&AppError::SubscriptionNotFound),
            None
        );
        assert_eq!(ImportService::rule_for_error(&AppError::InternalError), None);
    }

    #[test]
    fn import_violation_serializes_with_snake_case_rule() {
        let violation = ImportViolation {
            index: 2,
            trigger: "/csv".into(),
            rule: ImportRule::PrefixConflict,
        };
        let json = serde_json::to_value(&violation).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!({
                "index": 2,
                "trigger": "/csv",
                "rule": "prefix_conflict"
            })
        );
        assert_eq!(
            serde_json::to_value(ImportRule::Uniqueness).unwrap(),
            serde_json::json!("uniqueness")
        );
        assert_eq!(
            serde_json::to_value(ImportRule::ContentSize).unwrap(),
            serde_json::json!("content_size")
        );
    }

    #[test]
    fn candidate_maps_to_create_snippet_operation() {
        let workspace_id = Uuid::new_v4();
        let candidate = CandidateSnippet {
            trigger: "/hi".into(),
            content: "hello".into(),
            snippet_type: "static".into(),
            ai_prompt: None,
        };

        match candidate.into_batch_operation(workspace_id) {
            BatchOperation::CreateSnippet(payload) => {
                assert_eq!(payload.workspace_id, workspace_id);
                assert_eq!(payload.trigger, "/hi");
                assert_eq!(payload.content, "hello");
                assert_eq!(payload.snippet_type, "static");
                assert_eq!(payload.folder_id, None);
                assert_eq!(payload.ai_prompt, None);
            }
            other => panic!("expected CreateSnippet, got {other:?}"),
        }
    }
}
