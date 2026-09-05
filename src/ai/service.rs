use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tracing::warn;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::errors::AppError;
use crate::models::common::{SubscriptionStatus, Tier};

// ─── Constants ──────────────────────────────────────────────────────────────────

/// Free tier: 50 AI requests per 24-hour rolling window.
const FREE_TIER_QUOTA: usize = 50;

/// Paid tier (pro/teams): 1000 AI requests per 24-hour rolling window.
const PAID_TIER_QUOTA: usize = 1000;

/// Maximum trigger length in characters.
const MAX_TRIGGER_LENGTH: usize = 500;

/// Maximum system_prompt size in bytes (10 KB).
const MAX_SYSTEM_PROMPT_BYTES: usize = 10_240;

/// Maximum context size in bytes (50 KB).
const MAX_CONTEXT_BYTES: usize = 51_200;

/// Maximum description length for generation requests (2 KB).
const MAX_DESCRIPTION_BYTES: usize = 2_048;

/// Maximum content length for improve requests (50 KB).
const MAX_CONTENT_BYTES: usize = 51_200;

/// Timeout for Gemini API calls.
const GEMINI_TIMEOUT: Duration = Duration::from_secs(15);

/// Timeout for acquiring a semaphore permit (queue wait time).
const SEMAPHORE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Gemini model to use.
const GEMINI_MODEL: &str = "gemini-2.0-flash";

/// Gemini API base URL.
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

// ─── Request / Response DTOs ────────────────────────────────────────────────────

/// Request body for the AI expand endpoint (existing).
#[derive(Debug, Clone, Deserialize)]
pub struct AiExpandRequest {
    pub trigger: String,
    pub system_prompt: String,
    pub context: Option<String>,
}

/// Response body for the AI expand endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct AiExpandResponse {
    pub expanded_text: String,
}

/// Request body for AI snippet generation.
#[derive(Debug, Clone, Deserialize)]
pub struct AiGenerateRequest {
    /// Description of the snippet to generate (e.g. "a professional email follow-up template").
    pub description: String,
    /// Optional tone/style hint (e.g. "professional", "casual", "empathetic").
    pub tone: Option<String>,
    /// Optional category context (e.g. "customer-support", "sales", "developer").
    pub category: Option<String>,
    /// Optional: whether to include dynamic variables in the output.
    pub include_variables: Option<bool>,
}

/// Response body for AI snippet generation.
#[derive(Debug, Clone, Serialize)]
pub struct AiGenerateResponse {
    /// The generated snippet content.
    pub content: String,
    /// Suggested trigger shortcut for this snippet.
    pub suggested_trigger: String,
}

/// Request body for AI snippet improvement.
#[derive(Debug, Clone, Deserialize)]
pub struct AiImproveRequest {
    /// The existing snippet content to improve.
    pub content: String,
    /// The improvement instruction (e.g. "make it more professional", "shorten it", "add empathy").
    pub instruction: String,
}

/// Response body for AI snippet improvement.
#[derive(Debug, Clone, Serialize)]
pub struct AiImproveResponse {
    /// The improved snippet content.
    pub content: String,
}

// ─── Gemini API Types ───────────────────────────────────────────────────────────

/// Gemini generateContent request structure.
#[derive(Debug, Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    generation_config: GeminiGenerationConfig,
}

#[derive(Debug, Serialize)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiPart {
    text: String,
}

#[derive(Debug, Serialize)]
struct GeminiGenerationConfig {
    temperature: f32,
    max_output_tokens: u32,
}

/// Gemini generateContent response structure.
#[derive(Debug, Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiResponseContent>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponseContent {
    parts: Option<Vec<GeminiPart>>,
}

// ─── Internal Row Types ─────────────────────────────────────────────────────────

#[derive(Debug, sqlx::FromRow)]
struct UserSubscriptionRow {
    pub tier: String,
    pub status: String,
}

// ─── Service ────────────────────────────────────────────────────────────────────

/// AI service handling snippet generation, improvement, and expansion via Gemini.
pub struct AiService {
    pool: PgPool,
    config: Arc<AppConfig>,
    http_client: Client,
    /// Concurrency semaphore limiting parallel AI provider calls.
    semaphore: Arc<Semaphore>,
    /// In-memory quota tracker: user_id → sorted timestamps of recent requests.
    quota_tracker: DashMap<Uuid, VecDeque<DateTime<Utc>>>,
}

impl AiService {
    /// Create a new AiService instance.
    pub fn new(pool: PgPool, config: Arc<AppConfig>) -> Self {
        let max_concurrent = config.ai_max_concurrent_requests;

        let http_client = Client::builder()
            .timeout(GEMINI_TIMEOUT)
            .build()
            .expect("Failed to build reqwest client");

        Self {
            pool,
            config,
            http_client,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            quota_tracker: DashMap::new(),
        }
    }

    // ─── Expand (existing endpoint) ─────────────────────────────────────────────

    /// Expand a trigger using the AI model. Validates inputs, checks quota,
    /// acquires concurrency permit, calls Gemini, and returns the expanded text.
    pub async fn expand(
        &self,
        user_id: Uuid,
        request: AiExpandRequest,
    ) -> Result<AiExpandResponse, AppError> {
        self.validate_expand_inputs(&request)?;
        self.check_quota(user_id).await?;
        let _permit = self.acquire_permit().await?;

        let system_prompt = format!(
            "You are a text expansion assistant for a snippet tool called urSnip. \
             The user has a snippet trigger and a system prompt that describes how to expand it. \
             Return ONLY the expanded text, no explanation or formatting.\n\n\
             System prompt: {}\n\n\
             {}",
            request.system_prompt,
            request.context.as_deref().map(|c| format!("Context: {}", c)).unwrap_or_default()
        );

        let user_message = format!("Expand this trigger: {}", request.trigger);

        let expanded_text = self.call_gemini(&system_prompt, &user_message, 0.7, 2048).await?;
        self.record_usage(user_id);

        Ok(AiExpandResponse { expanded_text })
    }

    // ─── Generate (new endpoint) ────────────────────────────────────────────────

    /// Generate a new snippet from a description.
    pub async fn generate(
        &self,
        user_id: Uuid,
        request: AiGenerateRequest,
    ) -> Result<AiGenerateResponse, AppError> {
        self.validate_generate_inputs(&request)?;
        self.check_quota(user_id).await?;
        let _permit = self.acquire_permit().await?;

        let tone_hint = request.tone.as_deref().unwrap_or("professional");
        let category_hint = request.category.as_deref().unwrap_or("general");
        let variables_hint = if request.include_variables.unwrap_or(false) {
            "You may include dynamic variables like {date}, {clipboard}, {input:Name}, {cursor} where appropriate."
        } else {
            "Do NOT include any dynamic variables or placeholders with curly braces."
        };

        let system_prompt = format!(
            "You are a snippet generation assistant for urSnip, a text expansion tool. \
             Generate a reusable text snippet based on the user's description.\n\n\
             Rules:\n\
             - Return ONLY a JSON object with two fields: \"content\" (the snippet text) and \"suggested_trigger\" (a short /trigger shortcut, max 12 chars, lowercase, no spaces).\n\
             - The snippet should be in a {} tone.\n\
             - Category context: {}.\n\
             - {}\n\
             - Do not include any markdown formatting or code fences in your response. Return raw JSON only.",
            tone_hint, category_hint, variables_hint
        );

        let user_message = format!("Generate a snippet for: {}", request.description);

        let raw_response = self.call_gemini(&system_prompt, &user_message, 0.8, 2048).await?;
        self.record_usage(user_id);

        // Parse the JSON response from Gemini
        let parsed = self.parse_generate_response(&raw_response)?;

        Ok(parsed)
    }

    // ─── Improve (new endpoint) ─────────────────────────────────────────────────

    /// Improve/rewrite an existing snippet based on an instruction.
    pub async fn improve(
        &self,
        user_id: Uuid,
        request: AiImproveRequest,
    ) -> Result<AiImproveResponse, AppError> {
        self.validate_improve_inputs(&request)?;
        self.check_quota(user_id).await?;
        let _permit = self.acquire_permit().await?;

        let system_prompt =
            "You are a text improvement assistant for urSnip, a text expansion tool. \
             The user will provide existing snippet content and an instruction for how to improve it. \
             Return ONLY the improved text. Do not add any explanation, commentary, or formatting. \
             Preserve any dynamic variables (e.g. {date}, {clipboard}, {input:Name}) that exist in the original.";

        let user_message = format!(
            "Original snippet:\n---\n{}\n---\n\nInstruction: {}",
            request.content, request.instruction
        );

        let improved = self.call_gemini(system_prompt, &user_message, 0.7, 4096).await?;
        self.record_usage(user_id);

        Ok(AiImproveResponse { content: improved })
    }

    // ─── Gemini API Communication ───────────────────────────────────────────────

    /// Call the Gemini API with a system instruction and user message.
    async fn call_gemini(
        &self,
        system_prompt: &str,
        user_message: &str,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, AppError> {
        let api_key = &self.config.gemini_api_key;
        if api_key.is_empty() {
            warn!("Gemini API key is not configured");
            return Err(AppError::AiProviderUnavailable);
        }

        let url = format!(
            "{}/{}:generateContent?key={}",
            GEMINI_API_BASE, GEMINI_MODEL, api_key
        );

        let gemini_request = GeminiRequest {
            contents: vec![GeminiContent {
                role: Some("user".to_string()),
                parts: vec![GeminiPart {
                    text: user_message.to_string(),
                }],
            }],
            system_instruction: Some(GeminiContent {
                role: None,
                parts: vec![GeminiPart {
                    text: system_prompt.to_string(),
                }],
            }),
            generation_config: GeminiGenerationConfig {
                temperature,
                max_output_tokens: max_tokens,
            },
        };

        let response = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&gemini_request)
            .send()
            .await
            .map_err(|e| {
                warn!("Gemini API request failed: {}", e);
                AppError::AiProviderUnavailable
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!("Gemini API returned error status {}: {}", status, body);
            return Err(AppError::AiProviderUnavailable);
        }

        let gemini_response: GeminiResponse = response.json().await.map_err(|e| {
            warn!("Gemini API returned invalid JSON: {}", e);
            AppError::AiProviderInvalidResponse
        })?;

        // Extract text from the first candidate
        let text = gemini_response
            .candidates
            .and_then(|c| c.into_iter().next())
            .and_then(|c| c.content)
            .and_then(|c| c.parts)
            .and_then(|p| p.into_iter().next())
            .map(|p| p.text)
            .unwrap_or_default();

        if text.is_empty() {
            warn!("Gemini API returned empty response");
            return Err(AppError::AiProviderInvalidResponse);
        }

        Ok(text.trim().to_string())
    }

    // ─── Response Parsing ───────────────────────────────────────────────────────

    /// Parse Gemini's raw text response for the generate endpoint into structured output.
    fn parse_generate_response(&self, raw: &str) -> Result<AiGenerateResponse, AppError> {
        // Try to parse as JSON directly
        #[derive(Deserialize)]
        struct GenerateJson {
            content: Option<String>,
            suggested_trigger: Option<String>,
        }

        // Strip potential markdown code fences
        let cleaned = raw
            .trim()
            .strip_prefix("```json")
            .or_else(|| raw.trim().strip_prefix("```"))
            .unwrap_or(raw.trim());
        let cleaned = cleaned
            .strip_suffix("```")
            .unwrap_or(cleaned)
            .trim();

        if let Ok(parsed) = serde_json::from_str::<GenerateJson>(cleaned) {
            let content = parsed.content.unwrap_or_else(|| cleaned.to_string());
            let suggested_trigger = parsed
                .suggested_trigger
                .unwrap_or_else(|| "/snippet".to_string());

            return Ok(AiGenerateResponse {
                content,
                suggested_trigger,
            });
        }

        // Fallback: treat the entire response as the content
        Ok(AiGenerateResponse {
            content: raw.trim().to_string(),
            suggested_trigger: "/snippet".to_string(),
        })
    }

    // ─── Input Validation ───────────────────────────────────────────────────────

    /// Validate the AI expand request fields.
    fn validate_expand_inputs(&self, request: &AiExpandRequest) -> Result<(), AppError> {
        if request.trigger.is_empty() {
            return Err(AppError::InvalidRequestBody);
        }
        if request.system_prompt.is_empty() {
            return Err(AppError::InvalidRequestBody);
        }
        if request.trigger.chars().count() > MAX_TRIGGER_LENGTH {
            return Err(AppError::TriggerTooLong);
        }
        if request.system_prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
            return Err(AppError::SystemPromptTooLong);
        }
        if let Some(ref context) = request.context {
            if context.len() > MAX_CONTEXT_BYTES {
                return Err(AppError::ContextTooLong);
            }
        }
        Ok(())
    }

    /// Validate the AI generate request fields.
    fn validate_generate_inputs(&self, request: &AiGenerateRequest) -> Result<(), AppError> {
        if request.description.is_empty() {
            return Err(AppError::InvalidRequestBody);
        }
        if request.description.len() > MAX_DESCRIPTION_BYTES {
            return Err(AppError::InvalidRequestBody);
        }
        Ok(())
    }

    /// Validate the AI improve request fields.
    fn validate_improve_inputs(&self, request: &AiImproveRequest) -> Result<(), AppError> {
        if request.content.is_empty() {
            return Err(AppError::InvalidRequestBody);
        }
        if request.instruction.is_empty() {
            return Err(AppError::InvalidRequestBody);
        }
        if request.content.len() > MAX_CONTENT_BYTES {
            return Err(AppError::InvalidRequestBody);
        }
        if request.instruction.len() > MAX_DESCRIPTION_BYTES {
            return Err(AppError::InvalidRequestBody);
        }
        Ok(())
    }

    // ─── Quota Enforcement ──────────────────────────────────────────────────────

    /// Check whether the user has remaining AI quota in the 24h rolling window.
    async fn check_quota(&self, user_id: Uuid) -> Result<(), AppError> {
        let limit = self.get_quota_limit(user_id).await?;
        let now = Utc::now();
        let window_start = now - chrono::Duration::hours(24);

        let current_usage = self.count_usage_in_window(user_id, window_start);

        if current_usage >= limit {
            return Err(AppError::AiQuotaExceeded);
        }

        Ok(())
    }

    /// Determine the quota limit for a user based on their subscription tier/status.
    async fn get_quota_limit(&self, user_id: Uuid) -> Result<usize, AppError> {
        let row = sqlx::query_as::<_, UserSubscriptionRow>(
            r#"
            SELECT s.tier, s.status
            FROM subscriptions s
            INNER JOIN workspaces w ON w.id = s.workspace_id
            WHERE w.owner_id = $1
              AND w.type = 'individual'
            LIMIT 1
            "#,
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        let Some(row) = row else {
            return Ok(FREE_TIER_QUOTA);
        };

        let tier = match row.tier.as_str() {
            "pro" | "teams" => Tier::Pro,
            _ => Tier::Free,
        };

        let status = match row.status.as_str() {
            "active" => SubscriptionStatus::Active,
            "past_due" => SubscriptionStatus::PastDue,
            "cancelled" => SubscriptionStatus::Cancelled,
            "pending_payment" => SubscriptionStatus::PendingPayment,
            "deactivated" => SubscriptionStatus::Deactivated,
            _ => SubscriptionStatus::Active,
        };

        match (tier, status) {
            (Tier::Pro | Tier::Teams, SubscriptionStatus::Active) => Ok(PAID_TIER_QUOTA),
            (Tier::Pro | Tier::Teams, SubscriptionStatus::PastDue) => Ok(PAID_TIER_QUOTA),
            (_, SubscriptionStatus::Cancelled | SubscriptionStatus::Deactivated) => {
                Ok(FREE_TIER_QUOTA)
            }
            _ => Ok(FREE_TIER_QUOTA),
        }
    }

    /// Count the number of requests a user has made within the given window.
    fn count_usage_in_window(&self, user_id: Uuid, window_start: DateTime<Utc>) -> usize {
        let entry = self.quota_tracker.get(&user_id);
        match entry {
            Some(timestamps) => timestamps.iter().filter(|ts| **ts >= window_start).count(),
            None => 0,
        }
    }

    /// Record a successful AI request for quota tracking.
    fn record_usage(&self, user_id: Uuid) {
        let now = Utc::now();
        let window_start = now - chrono::Duration::hours(24);

        let mut entry = self.quota_tracker.entry(user_id).or_insert_with(VecDeque::new);

        while let Some(front) = entry.front() {
            if *front < window_start {
                entry.pop_front();
            } else {
                break;
            }
        }

        entry.push_back(now);
    }

    // ─── Concurrency Control ────────────────────────────────────────────────────

    /// Acquire a semaphore permit with timeout.
    async fn acquire_permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, AppError> {
        let available = self.semaphore.available_permits();

        if available == 0 {
            match tokio::time::timeout(SEMAPHORE_ACQUIRE_TIMEOUT, self.semaphore.acquire()).await {
                Ok(Ok(permit)) => Ok(permit),
                Ok(Err(_)) => Err(AppError::AiServiceBusy),
                Err(_) => {
                    warn!("AI service busy: semaphore acquire timed out");
                    Err(AppError::AiServiceBusy)
                }
            }
        } else {
            match self.semaphore.acquire().await {
                Ok(permit) => Ok(permit),
                Err(_) => Err(AppError::AiServiceBusy),
            }
        }
    }
}
