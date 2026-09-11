//! Property-based tests for Workspace name validation (Property 7).
//!
//! These tests validate that empty / whitespace-only workspace names are
//! rejected on both create and update, and that such a name is never
//! persisted. They use a simulated in-memory store that mirrors the real
//! validation logic in `src/workspace/service.rs`
//! (`create_individual_workspace`, `create_team_workspace`, and
//! `update_workspace`), which reject when `name.trim().is_empty()` with
//! `AppError::InvalidRequestBody`.
//!
//! Run with: `cargo test --test workspace_name_property_tests`

use proptest::prelude::*;

// ─── Feature: ursnip-feature-matrix, Property 7: Empty workspace names are rejected ─
//
// **Validates: Requirements 8.1**
//
// For any create or update request whose supplied workspace name is empty or
// consists solely of whitespace, the backend SHALL reject the request with a
// validation error (InvalidRequestBody) and SHALL NOT persist the name
// (workspace not created, or the existing name left unchanged on update).

/// Simulated error type mirroring the subset of `AppError` relevant to name validation.
#[derive(Debug, Clone, PartialEq)]
enum WorkspaceError {
    /// Mirrors `AppError::InvalidRequestBody` — the validation error returned
    /// by the real handlers for empty / whitespace-only names.
    InvalidRequestBody,
    WorkspaceNotFound,
}

/// Simulated workspace store that mirrors the real name-validation logic.
///
/// The real service rejects when `name.trim().is_empty()` before performing
/// any INSERT/UPDATE, so an invalid name never reaches the database.
#[derive(Debug, Clone)]
struct WorkspaceStore {
    /// (workspace_id, name)
    workspaces: Vec<(String, String)>,
    next_id: u64,
}

impl WorkspaceStore {
    fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            next_id: 1,
        }
    }

    /// Create a workspace. Mirrors `create_individual_workspace` /
    /// `create_team_workspace`: reject empty/whitespace-only names before any
    /// write occurs. Returns the new workspace id on success.
    fn create(&mut self, name: &str) -> Result<String, WorkspaceError> {
        if name.trim().is_empty() {
            return Err(WorkspaceError::InvalidRequestBody);
        }
        let id = format!("ws_{}", self.next_id);
        self.next_id += 1;
        self.workspaces.push((id.clone(), name.to_string()));
        Ok(id)
    }

    /// Update a workspace name. Mirrors `update_workspace`: a provided but
    /// empty/whitespace-only name is rejected (replacing the old silent no-op);
    /// a `None` name is a no-op. Returns the persisted name on success.
    fn update(
        &mut self,
        workspace_id: &str,
        name: Option<&str>,
    ) -> Result<String, WorkspaceError> {
        match name {
            Some(n) if n.trim().is_empty() => Err(WorkspaceError::InvalidRequestBody),
            Some(n) => {
                let ws = self
                    .workspaces
                    .iter_mut()
                    .find(|(wid, _)| wid == workspace_id)
                    .ok_or(WorkspaceError::WorkspaceNotFound)?;
                ws.1 = n.to_string();
                Ok(ws.1.clone())
            }
            None => {
                let ws = self
                    .workspaces
                    .iter()
                    .find(|(wid, _)| wid == workspace_id)
                    .ok_or(WorkspaceError::WorkspaceNotFound)?;
                Ok(ws.1.clone())
            }
        }
    }

    fn count(&self) -> usize {
        self.workspaces.len()
    }

    fn name_of(&self, workspace_id: &str) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|(wid, _)| wid == workspace_id)
            .map(|(_, name)| name.as_str())
    }
}

// ─── Strategies ─────────────────────────────────────────────────────────────────

/// Strategy generating empty / whitespace-only names: "", " ", "\t", "\n",
/// and arbitrary combinations of whitespace characters (including unicode
/// whitespace such as non-breaking space and vertical tab).
fn whitespace_only_name_strategy() -> impl Strategy<Value = String> {
    // A single whitespace-ish character. `str::trim` trims Unicode whitespace,
    // so these are all trimmed away, leaving an empty string.
    let ws_char = prop::sample::select(vec![
        ' ', '\t', '\n', '\r', '\u{000B}', '\u{000C}', '\u{00A0}', '\u{2003}',
    ]);
    // 0..=8 whitespace chars → includes the empty string case.
    prop::collection::vec(ws_char, 0..=8).prop_map(|chars| chars.into_iter().collect())
}

/// Strategy generating a non-empty, non-whitespace-only valid name.
fn valid_name_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9][a-zA-Z0-9 ]{0,20}".prop_filter("must not be whitespace-only", |s| {
        !s.trim().is_empty()
    })
}

// ─── Property Test Implementations ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Feature: ursnip-feature-matrix, Property 7: Empty workspace names are rejected

    /// **Validates: Requirements 8.1**
    ///
    /// Property 7a: Creating a workspace with an empty / whitespace-only name
    /// is rejected with InvalidRequestBody, and no workspace is persisted.
    #[test]
    fn prop_create_rejects_empty_name(name in whitespace_only_name_strategy()) {
        let mut store = WorkspaceStore::new();

        let result = store.create(&name);

        prop_assert_eq!(
            result,
            Err(WorkspaceError::InvalidRequestBody),
            "create with empty/whitespace-only name {:?} must be rejected",
            name
        );
        // Name not persisted → no workspace created.
        prop_assert_eq!(
            store.count(),
            0,
            "no workspace should be persisted for rejected name {:?}",
            name
        );
    }

    // Feature: ursnip-feature-matrix, Property 7: Empty workspace names are rejected

    /// **Validates: Requirements 8.1**
    ///
    /// Property 7b: Updating a workspace with an empty / whitespace-only name
    /// is rejected with InvalidRequestBody, and the existing name is unchanged.
    #[test]
    fn prop_update_rejects_empty_name(
        original in valid_name_strategy(),
        new_name in whitespace_only_name_strategy(),
    ) {
        let mut store = WorkspaceStore::new();
        let id = store.create(&original).expect("valid original name must create");

        let result = store.update(&id, Some(&new_name));

        prop_assert_eq!(
            result,
            Err(WorkspaceError::InvalidRequestBody),
            "update with empty/whitespace-only name {:?} must be rejected",
            new_name
        );
        // Existing name must be unchanged (rejected name not persisted).
        prop_assert_eq!(
            store.name_of(&id),
            Some(original.as_str()),
            "existing name must remain unchanged after rejected update"
        );
    }

    // Feature: ursnip-feature-matrix, Property 7: Empty workspace names are rejected

    /// **Validates: Requirements 8.1**
    ///
    /// Property 7c: A valid (non-empty, non-whitespace-only) name is accepted
    /// on both create and update — the rejection is specific to empty names.
    #[test]
    fn prop_valid_name_is_accepted(
        create_name in valid_name_strategy(),
        update_name in valid_name_strategy(),
    ) {
        let mut store = WorkspaceStore::new();

        let id = store.create(&create_name);
        prop_assert!(id.is_ok(), "valid create name {:?} must be accepted", create_name);
        let id = id.unwrap();
        prop_assert_eq!(store.count(), 1, "valid create must persist exactly one workspace");

        let updated = store.update(&id, Some(&update_name));
        prop_assert!(updated.is_ok(), "valid update name {:?} must be accepted", update_name);
        prop_assert_eq!(
            store.name_of(&id),
            Some(update_name.as_str()),
            "valid update must persist the new name"
        );
    }
}
