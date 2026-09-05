//! Teams account-level collaborators with granular access control.
//!
//! A Teams-subscribed account holder ("owner") invites collaborators at the
//! account level by email. After acceptance, the owner grants each collaborator
//! View or Edit access to specific workspaces, folders, or snippets. Access to a
//! container cascades to its contents (workspace -> folder -> snippet) with the
//! most specific grant winning. Only the owner may delete resources or manage
//! grants; the owner always has full access to their own resources.

pub mod access;
pub mod handlers;
pub mod service;

pub use service::CollaboratorService;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Access level a collaborator can hold on a resource. `Edit` implies read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    View,
    Edit,
}

impl Level {
    /// Parse the DB/string representation.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "view" => Some(Level::View),
            "edit" => Some(Level::Edit),
            _ => None,
        }
    }

    /// String representation stored in the DB.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::View => "view",
            Level::Edit => "edit",
        }
    }

    /// Whether this level permits writing (create/update). Only `Edit` does.
    pub fn can_write(self) -> bool {
        matches!(self, Level::Edit)
    }

    /// Specificity ordering used by "most specific grant wins". Higher = more specific.
    /// (Used for level combination where the same specificity yields the stronger level.)
    pub fn rank(self) -> u8 {
        match self {
            Level::View => 1,
            Level::Edit => 2,
        }
    }
}

/// The kind of resource an access grant targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceType {
    Workspace,
    Folder,
    Snippet,
}

impl ResourceType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "workspace" => Some(ResourceType::Workspace),
            "folder" => Some(ResourceType::Folder),
            "snippet" => Some(ResourceType::Snippet),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ResourceType::Workspace => "workspace",
            ResourceType::Folder => "folder",
            ResourceType::Snippet => "snippet",
        }
    }

    /// Specificity: snippet (most specific) > folder > workspace (least specific).
    pub fn specificity(self) -> u8 {
        match self {
            ResourceType::Workspace => 1,
            ResourceType::Folder => 2,
            ResourceType::Snippet => 3,
        }
    }
}

/// Response returned after creating an invite.
#[derive(Debug, Serialize)]
pub struct InviteResponse {
    pub id: Uuid,
    pub email: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// A collaborator entry as seen by the owner (accepted or pending).
#[derive(Debug, Serialize)]
pub struct CollaboratorInfo {
    /// Present only for accepted collaborators (a real user account).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    pub email: String,
    /// "accepted" | "pending"
    pub status: String,
}

/// A single access grant.
#[derive(Debug, Serialize)]
pub struct GrantInfo {
    pub id: Uuid,
    pub resource_type: String,
    pub resource_id: Uuid,
    pub level: String,
}

/// Request body to create/update a grant.
#[derive(Debug, Deserialize)]
pub struct UpsertGrantRequest {
    pub resource_type: String,
    pub resource_id: Uuid,
    pub level: String,
}

/// Request body to invite a collaborator.
#[derive(Debug, Deserialize)]
pub struct InviteRequest {
    pub email: String,
}

/// Request body to accept an invite.
#[derive(Debug, Deserialize)]
pub struct AcceptInviteRequest {
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parses_and_serializes() {
        assert_eq!(Level::parse("view"), Some(Level::View));
        assert_eq!(Level::parse("edit"), Some(Level::Edit));
        assert_eq!(Level::parse("admin"), None);
        assert_eq!(Level::View.as_str(), "view");
        assert_eq!(Level::Edit.as_str(), "edit");
    }

    #[test]
    fn edit_can_write_view_cannot() {
        assert!(Level::Edit.can_write());
        assert!(!Level::View.can_write());
    }

    #[test]
    fn resource_type_specificity_order() {
        assert!(ResourceType::Snippet.specificity() > ResourceType::Folder.specificity());
        assert!(ResourceType::Folder.specificity() > ResourceType::Workspace.specificity());
    }

    #[test]
    fn resource_type_parses() {
        assert_eq!(ResourceType::parse("workspace"), Some(ResourceType::Workspace));
        assert_eq!(ResourceType::parse("folder"), Some(ResourceType::Folder));
        assert_eq!(ResourceType::parse("snippet"), Some(ResourceType::Snippet));
        assert_eq!(ResourceType::parse("nope"), None);
    }

    // Most-specific-wins resolution, exercised as a pure function over candidate grants.
    // Each candidate is (ResourceType, Level). The winner is the highest specificity;
    // ties resolve to the stronger level.
    fn resolve(candidates: &[(ResourceType, Level)]) -> Option<Level> {
        candidates
            .iter()
            .copied()
            .max_by(|a, b| {
                a.0.specificity()
                    .cmp(&b.0.specificity())
                    .then(a.1.rank().cmp(&b.1.rank()))
            })
            .map(|(_, level)| level)
    }

    #[test]
    fn most_specific_grant_wins() {
        // snippet grant beats folder + workspace
        assert_eq!(
            resolve(&[
                (ResourceType::Workspace, Level::Edit),
                (ResourceType::Folder, Level::Edit),
                (ResourceType::Snippet, Level::View),
            ]),
            Some(Level::View)
        );
        // folder grant beats workspace when no snippet grant
        assert_eq!(
            resolve(&[
                (ResourceType::Workspace, Level::View),
                (ResourceType::Folder, Level::Edit),
            ]),
            Some(Level::Edit)
        );
        // only workspace grant
        assert_eq!(
            resolve(&[(ResourceType::Workspace, Level::View)]),
            Some(Level::View)
        );
        // nothing
        assert_eq!(resolve(&[]), None);
    }
}
