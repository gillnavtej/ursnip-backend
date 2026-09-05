//! Snippet usage analytics — recording expansions and querying stats.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

// ─── Request / Response DTOs ────────────────────────────────────────────────────

/// Request to record a snippet expansion event.
#[derive(Debug, Deserialize)]
pub struct RecordUsageRequest {
    pub workspace_id: Uuid,
    pub device_id: Option<Uuid>,
}

/// Daily usage data point.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DailyUsage {
    pub date: DateTime<Utc>,
    pub count: i64,
}

/// Analytics response for a single snippet.
#[derive(Debug, Serialize)]
pub struct SnippetAnalyticsResponse {
    pub snippet_id: Uuid,
    pub total_uses: i64,
    pub uses_today: i64,
    pub uses_this_week: i64,
    pub uses_this_month: i64,
    pub daily_usage: Vec<DailyUsage>,
}

// ─── Service Functions ──────────────────────────────────────────────────────────

/// Record a snippet expansion event.
pub async fn record_usage(
    pool: &PgPool,
    snippet_id: Uuid,
    workspace_id: Uuid,
    user_id: Uuid,
    device_id: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        INSERT INTO snippet_usages (snippet_id, workspace_id, user_id, device_id)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(snippet_id)
    .bind(workspace_id)
    .bind(user_id)
    .bind(device_id)
    .execute(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    Ok(())
}

/// Get analytics for a specific snippet (last 30 days daily breakdown + totals).
pub async fn get_snippet_analytics(
    pool: &PgPool,
    snippet_id: Uuid,
) -> Result<SnippetAnalyticsResponse, AppError> {
    let now = Utc::now();
    let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
    let week_start = (now - Duration::days(7)).date_naive().and_hms_opt(0, 0, 0).unwrap();
    let month_start = (now - Duration::days(30)).date_naive().and_hms_opt(0, 0, 0).unwrap();

    // Total uses
    let total_uses: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM snippet_usages WHERE snippet_id = $1",
    )
    .bind(snippet_id)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Uses today
    let uses_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM snippet_usages WHERE snippet_id = $1 AND expanded_at >= $2::date::timestamptz",
    )
    .bind(snippet_id)
    .bind(today_start)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Uses this week
    let uses_this_week: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM snippet_usages WHERE snippet_id = $1 AND expanded_at >= $2::date::timestamptz",
    )
    .bind(snippet_id)
    .bind(week_start)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Uses this month
    let uses_this_month: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM snippet_usages WHERE snippet_id = $1 AND expanded_at >= $2::date::timestamptz",
    )
    .bind(snippet_id)
    .bind(month_start)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Daily breakdown (last 30 days)
    let daily_usage = sqlx::query_as::<_, DailyUsage>(
        r#"
        SELECT date_trunc('day', expanded_at) AS date, COUNT(*) AS count
        FROM snippet_usages
        WHERE snippet_id = $1 AND expanded_at >= $2::date::timestamptz
        GROUP BY date_trunc('day', expanded_at)
        ORDER BY date ASC
        "#,
    )
    .bind(snippet_id)
    .bind(month_start)
    .fetch_all(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    Ok(SnippetAnalyticsResponse {
        snippet_id,
        total_uses,
        uses_today,
        uses_this_week,
        uses_this_month,
        daily_usage,
    })
}

// ─── Platform Analytics ─────────────────────────────────────────────────────────

/// Full analytics response for the analytics dashboard.
#[derive(Debug, Serialize)]
pub struct PlatformAnalyticsResponse {
    pub total_expansions_month: i64,
    pub total_expansions_today: i64,
    pub peak_hour: String,
    pub avg_per_user: i64,
    pub time_saved_seconds: i64,
    pub total_chars_expanded: i64,
    pub daily_expansions: Vec<DailyExpansionsByType>,
    pub hourly_pattern: Vec<HourlyUsage>,
    pub top_snippets: Vec<TopSnippetUsage>,
    pub platform_distribution: Vec<PlatformShare>,
}

/// Daily expansions broken down by snippet type.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DailyExpansionsByType {
    pub date: Option<DateTime<Utc>>,
    #[serde(serialize_with = "serialize_opt_i64")]
    pub static_count: Option<i64>,
    #[serde(serialize_with = "serialize_opt_i64")]
    pub dynamic_count: Option<i64>,
    #[serde(serialize_with = "serialize_opt_i64")]
    pub ai_count: Option<i64>,
}

fn serialize_opt_i64<S: serde::Serializer>(val: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_i64(val.unwrap_or(0))
}

/// Hourly usage pattern.
#[derive(Debug, Clone, Serialize)]
pub struct HourlyUsage {
    pub hour: i32,
    pub triggers: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct HourlyRow {
    pub hour: Option<i32>,
    pub count: i64,
}

/// Top snippet by usage count.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TopSnippetUsage {
    pub trigger: String,
    pub snippet_type: String,
    pub uses: i64,
}

/// Platform distribution share.
#[derive(Debug, Clone, Serialize)]
pub struct PlatformShare {
    pub name: String,
    pub value: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct PlatformRow {
    pub platform: Option<String>,
    pub count: i64,
}

/// Get full platform analytics for the dashboard.
pub async fn get_platform_analytics(
    pool: &PgPool,
) -> Result<PlatformAnalyticsResponse, AppError> {
    let now = Utc::now();
    let month_start = (now - Duration::days(30)).date_naive().and_hms_opt(0, 0, 0).unwrap();
    let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();

    // Total expansions this month
    let total_expansions_month: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM snippet_usages WHERE expanded_at >= $1::date::timestamptz",
    )
    .bind(month_start)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Total expansions today
    let total_expansions_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM snippet_usages WHERE expanded_at >= $1::date::timestamptz",
    )
    .bind(today_start)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Peak hour (last 30 days)
    let peak_hour_row = sqlx::query_as::<_, HourlyRow>(
        r#"
        SELECT EXTRACT(HOUR FROM expanded_at)::int AS hour, COUNT(*) AS count
        FROM snippet_usages
        WHERE expanded_at >= $1::date::timestamptz
        GROUP BY EXTRACT(HOUR FROM expanded_at)
        ORDER BY count DESC
        LIMIT 1
        "#,
    )
    .bind(month_start)
    .fetch_optional(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let peak_hour = match peak_hour_row {
        Some(row) => format!("{}:00", row.hour.unwrap_or(0)),
        None => "—".to_string(),
    };

    // Avg expansions per active user (users who have at least 1 expansion this month)
    let active_users: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT user_id) FROM snippet_usages WHERE expanded_at >= $1::date::timestamptz",
    )
    .bind(month_start)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let avg_per_user = if active_users > 0 {
        total_expansions_month / active_users
    } else {
        0
    };

    // Daily expansions by type (last 30 days)
    let daily_expansions = sqlx::query_as::<_, DailyExpansionsByType>(
        r#"
        SELECT
            date_trunc('day', su.expanded_at) AS date,
            COUNT(*) FILTER (WHERE s.snippet_type = 'static') AS static_count,
            COUNT(*) FILTER (WHERE s.snippet_type = 'dynamic') AS dynamic_count,
            COUNT(*) FILTER (WHERE s.snippet_type = 'ai') AS ai_count
        FROM snippet_usages su
        JOIN snippets s ON s.id = su.snippet_id
        WHERE su.expanded_at >= $1::date::timestamptz
        GROUP BY date_trunc('day', su.expanded_at)
        ORDER BY date ASC
        "#,
    )
    .bind(month_start)
    .fetch_all(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Hourly pattern (all hours)
    let hourly_rows = sqlx::query_as::<_, HourlyRow>(
        r#"
        SELECT EXTRACT(HOUR FROM expanded_at)::int AS hour, COUNT(*) AS count
        FROM snippet_usages
        WHERE expanded_at >= $1::date::timestamptz
        GROUP BY EXTRACT(HOUR FROM expanded_at)
        ORDER BY hour ASC
        "#,
    )
    .bind(month_start)
    .fetch_all(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Fill all 24 hours
    let mut hourly_pattern: Vec<HourlyUsage> = (0..24)
        .map(|h| HourlyUsage { hour: h, triggers: 0 })
        .collect();
    for row in hourly_rows {
        let h = row.hour.unwrap_or(0) as usize;
        if h < 24 {
            hourly_pattern[h].triggers = row.count;
        }
    }

    // Top snippets
    let top_snippets = sqlx::query_as::<_, TopSnippetUsage>(
        r#"
        SELECT s.trigger, s.snippet_type, COUNT(*) AS uses
        FROM snippet_usages su
        JOIN snippets s ON s.id = su.snippet_id
        WHERE su.expanded_at >= $1::date::timestamptz
        GROUP BY s.trigger, s.snippet_type
        ORDER BY uses DESC
        LIMIT 10
        "#,
    )
    .bind(month_start)
    .fetch_all(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Platform distribution (from devices table)
    let platform_rows = sqlx::query_as::<_, PlatformRow>(
        r#"
        SELECT d.platform, COUNT(DISTINCT d.user_id) AS count
        FROM devices d
        WHERE d.is_active = true
        GROUP BY d.platform
        ORDER BY count DESC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let platform_distribution: Vec<PlatformShare> = platform_rows
        .into_iter()
        .map(|r| PlatformShare {
            name: r.platform.unwrap_or_else(|| "Unknown".to_string()),
            value: r.count,
        })
        .collect();

    // Time saved (platform-wide)
    let total_chars_expanded: i64 = sqlx::query_scalar::<_, Option<i64>>(
        r#"
        SELECT COALESCE(SUM(LENGTH(s.content)), 0)
        FROM snippet_usages su
        JOIN snippets s ON s.id = su.snippet_id
        WHERE su.expanded_at >= $1::date::timestamptz
        "#,
    )
    .bind(month_start)
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::InternalError)?
    .unwrap_or(0);

    let time_saved_seconds = total_chars_expanded / 3;

    Ok(PlatformAnalyticsResponse {
        total_expansions_month,
        total_expansions_today,
        peak_hour,
        avg_per_user,
        time_saved_seconds,
        total_chars_expanded,
        daily_expansions,
        hourly_pattern,
        top_snippets,
        platform_distribution,
    })
}

// ─── User-Scoped Analytics ──────────────────────────────────────────────────────

/// Get analytics scoped to a specific user (across all their workspaces).
/// Only shows data from snippet_usages where user_id matches.
pub async fn get_user_analytics(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<PlatformAnalyticsResponse, AppError> {
    let now = Utc::now();
    let month_start = now - Duration::days(30);
    let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

    // Total expansions this month (user's own)
    let total_expansions_month: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM snippet_usages WHERE user_id = $1 AND expanded_at >= $2",
    )
    .bind(user_id)
    .bind(month_start)
    .fetch_one(pool)
    .await
    .map_err(|e| { eprintln!("[ANALYTICS ERROR] total_month: {e:?}"); AppError::InternalError })?
    .unwrap_or(0);

    // Total expansions today (user's own)
    let total_expansions_today: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM snippet_usages WHERE user_id = $1 AND expanded_at >= $2",
    )
    .bind(user_id)
    .bind(today_start)
    .fetch_one(pool)
    .await
    .map_err(|e| { eprintln!("[ANALYTICS ERROR] total_today: {e:?}"); AppError::InternalError })?
    .unwrap_or(0);

    // Peak hour (user's own usage)
    let peak_hour_row = sqlx::query_as::<_, HourlyRow>(
        r#"
        SELECT EXTRACT(HOUR FROM expanded_at)::int AS hour, COUNT(*) AS count
        FROM snippet_usages
        WHERE user_id = $1 AND expanded_at >= $2
        GROUP BY EXTRACT(HOUR FROM expanded_at)
        ORDER BY count DESC
        LIMIT 1
        "#,
    )
    .bind(user_id)
    .bind(month_start)
    .fetch_optional(pool)
    .await
    .map_err(|e| { eprintln!("[ANALYTICS ERROR] peak_hour: {e:?}"); AppError::InternalError })?;

    let peak_hour = match peak_hour_row {
        Some(row) => format!("{}:00", row.hour.unwrap_or(0)),
        None => "—".to_string(),
    };

    // Avg per day (user's own: total / 30)
    let avg_per_user = if total_expansions_month > 0 {
        total_expansions_month / 30
    } else {
        0
    };

    // Daily expansions by type (user's own)
    let daily_expansions = sqlx::query_as::<_, DailyExpansionsByType>(
        r#"
        SELECT
            date_trunc('day', su.expanded_at) AS date,
            COUNT(*) FILTER (WHERE s.snippet_type = 'static') AS static_count,
            COUNT(*) FILTER (WHERE s.snippet_type = 'dynamic') AS dynamic_count,
            COUNT(*) FILTER (WHERE s.snippet_type = 'ai') AS ai_count
        FROM snippet_usages su
        JOIN snippets s ON s.id = su.snippet_id
        WHERE su.user_id = $1 AND su.expanded_at >= $2
        GROUP BY date_trunc('day', su.expanded_at)
        ORDER BY date ASC
        "#,
    )
    .bind(user_id)
    .bind(month_start)
    .fetch_all(pool)
    .await
    .map_err(|e| { eprintln!("[ANALYTICS ERROR] daily: {e:?}"); AppError::InternalError })?;

    // Hourly pattern (user's own)
    let hourly_rows = sqlx::query_as::<_, HourlyRow>(
        r#"
        SELECT EXTRACT(HOUR FROM expanded_at)::int AS hour, COUNT(*) AS count
        FROM snippet_usages
        WHERE user_id = $1 AND expanded_at >= $2
        GROUP BY EXTRACT(HOUR FROM expanded_at)
        ORDER BY hour ASC
        "#,
    )
    .bind(user_id)
    .bind(month_start)
    .fetch_all(pool)
    .await
    .map_err(|e| { eprintln!("[ANALYTICS ERROR] hourly: {e:?}"); AppError::InternalError })?;

    let mut hourly_pattern: Vec<HourlyUsage> = (0..24)
        .map(|h| HourlyUsage { hour: h, triggers: 0 })
        .collect();
    for row in hourly_rows {
        let h = row.hour.unwrap_or(0) as usize;
        if h < 24 {
            hourly_pattern[h].triggers = row.count;
        }
    }

    // Top snippets (user's own)
    let top_snippets = sqlx::query_as::<_, TopSnippetUsage>(
        r#"
        SELECT s.trigger, s.snippet_type, COUNT(*) AS uses
        FROM snippet_usages su
        JOIN snippets s ON s.id = su.snippet_id
        WHERE su.user_id = $1 AND su.expanded_at >= $2
        GROUP BY s.trigger, s.snippet_type
        ORDER BY uses DESC
        LIMIT 10
        "#,
    )
    .bind(user_id)
    .bind(month_start)
    .fetch_all(pool)
    .await
    .map_err(|e| { eprintln!("[ANALYTICS ERROR] top_snippets: {e:?}"); AppError::InternalError })?;

    // Platform distribution (user's own devices)
    let platform_rows = sqlx::query_as::<_, PlatformRow>(
        r#"
        SELECT platform, COUNT(*) AS count
        FROM devices
        WHERE user_id = $1
        GROUP BY platform
        ORDER BY count DESC
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|e| { eprintln!("[ANALYTICS ERROR] platforms: {e:?}"); AppError::InternalError })?;

    let platform_distribution: Vec<PlatformShare> = platform_rows
        .into_iter()
        .map(|r| PlatformShare {
            name: r.platform.unwrap_or_else(|| "Unknown".to_string()),
            value: r.count,
        })
        .collect();

    // Time saved: total characters expanded / average typing speed (3.3 chars/sec ≈ 40 WPM)
    let total_chars_expanded: i64 = sqlx::query_scalar::<_, Option<i64>>(
        r#"
        SELECT COALESCE(SUM(LENGTH(s.content)), 0)
        FROM snippet_usages su
        JOIN snippets s ON s.id = su.snippet_id
        WHERE su.user_id = $1 AND su.expanded_at >= $2
        "#,
    )
    .bind(user_id)
    .bind(month_start)
    .fetch_one(pool)
    .await
    .map_err(|e| { eprintln!("[ANALYTICS ERROR] time_saved: {e:?}"); AppError::InternalError })?
    .unwrap_or(0);

    // 3.3 chars/sec average typing speed (40 WPM)
    let time_saved_seconds = total_chars_expanded / 3;

    Ok(PlatformAnalyticsResponse {
        total_expansions_month,
        total_expansions_today,
        peak_hour,
        avg_per_user,
        time_saved_seconds,
        total_chars_expanded,
        daily_expansions,
        hourly_pattern,
        top_snippets,
        platform_distribution,
    })
}
