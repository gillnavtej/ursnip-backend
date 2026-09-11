use ursnip_backend::config;
use ursnip_backend::db;
use ursnip_backend::email;
use ursnip_backend::logging;
use ursnip_backend::middleware;
use ursnip_backend::router;
use ursnip_backend::scheduler;
use ursnip_backend::sync;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use config::AppConfig;
use email::EmailService;
use middleware::rate_limit::RateLimiter;
use router::{AppState, build_router};
use scheduler::service::SchedulerService;
use sync::session_registry::SessionRegistry;
use tokio::signal;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() {
    // ── 1. Load environment & config ────────────────────────────────────────
    dotenvy::dotenv().ok();

    // ── 2. Initialize structured JSON logging ───────────────────────────────
    let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
    logging::init_logging(&log_level);

    info!("ursnip-backend starting up");

    // Load configuration from environment
    let config = AppConfig::from_env();
    let config = Arc::new(config);
    let port = config.port;
    let shutdown_timeout_secs = config.shutdown_timeout_secs;

    // ── 3. Create PgPool and run pending migrations ─────────────────────────
    let pool = db::init_pool(&config).await;

    info!("Running database migrations");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .unwrap_or_else(|e| {
            error!("FATAL: database migration failed: {}", e);
            std::process::exit(1);
        });
    info!("Database migrations complete");

    // ── 3b. Seed admin password (hash placeholder if still present) ─────────
    seed_admin_password(&pool, &config).await;

    // ── 4. Initialize all services ──────────────────────────────────────────
    let session_registry = Arc::new(SessionRegistry::new(config.ws_max_connections));

    // Email service (warmup — just create the service; no connection test needed)
    let _email_service = EmailService::new(config.clone());
    info!("Email service initialized");

    // ── 5. Start background scheduler ───────────────────────────────────────
    let scheduler = Arc::new(SchedulerService::new(pool.clone(), shutdown_timeout_secs));
    let (scheduler_shutdown_tx, scheduler_handle) = scheduler.clone().start();
    info!("Background scheduler started");

    // ── 6. Set readiness flag ───────────────────────────────────────────────
    let ready = Arc::new(AtomicBool::new(false));

    // All init steps complete — mark ready
    ready.store(true, Ordering::Release);
    info!("All readiness checks passed, service is ready");

    // ── 7. Initialize rate limiter and build router ─────────────────────────
    let rate_limiter = RateLimiter::new(&config.trusted_proxy_cidrs);

    let state = AppState {
        pool: pool.clone(),
        config: config.clone(),
        rate_limiter,
        ready: ready.clone(),
        session_registry: session_registry.clone(),
    };

    let app = build_router(state);

    // ── 8. Bind TCP listener ────────────────────────────────────────────────
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .unwrap_or_else(|e| {
            error!("FATAL: failed to bind TCP listener on port {}: {}", port, e);
            std::process::exit(1);
        });

    info!("Listening on 0.0.0.0:{}", port);

    // ── 9. Serve with graceful shutdown ─────────────────────────────────────
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(
        shutdown_timeout_secs,
        session_registry,
        scheduler_shutdown_tx,
        scheduler_handle,
        pool,
    ))
    .await
    .unwrap_or_else(|e| {
        error!("Server error: {}", e);
        std::process::exit(1);
    });

    info!("shutdown complete");
}

/// Seed (or update) the super-admin user's email and password hash.
///
/// On every startup, checks if the admin user's `password_hash` is still the
/// migration placeholder (`PLACEHOLDER_REQUIRES_APP_INIT`). If so, hashes the
/// configured `SEED_ADMIN_PASSWORD` and updates the record. Also updates the
/// email to match `SEED_ADMIN_EMAIL` in case it changed.
/// Additionally ensures the admin has an individual workspace.
async fn seed_admin_password(pool: &sqlx::PgPool, config: &AppConfig) {
    use ursnip_backend::auth::password::hash_password;

    // Check if any admin user still has the placeholder hash
    let row: Option<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT id, password_hash FROM users WHERE role = 'admin' AND password_hash = 'PLACEHOLDER_REQUIRES_APP_INIT' LIMIT 1"
    )
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        warn!("Failed to check admin seed state: {}", e);
        None
    });

    if let Some((user_id, _)) = row {
        let hashed = hash_password(&config.seed_admin_password)
            .unwrap_or_else(|_| {
                error!("FATAL: failed to hash SEED_ADMIN_PASSWORD");
                std::process::exit(1);
            });

        sqlx::query(
            "UPDATE users SET password_hash = $1, email = $2 WHERE id = $3"
        )
        .bind(&hashed)
        .bind(&config.seed_admin_email)
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap_or_else(|e| {
            error!("FATAL: failed to seed admin password: {}", e);
            std::process::exit(1);
        });

        // Ensure admin has an individual workspace
        let has_workspace: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspaces WHERE owner_id = $1 AND type = 'individual')"
        )
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap_or(false);

        if !has_workspace {
            let workspace_id: uuid::Uuid = sqlx::query_scalar(
                "INSERT INTO workspaces (type, owner_id, name) VALUES ('individual', $1, 'Admin Workspace') RETURNING id"
            )
            .bind(user_id)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|e| {
                error!("FATAL: failed to create admin workspace: {}", e);
                std::process::exit(1);
            });

            let _ = sqlx::query(
                "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')"
            )
            .bind(workspace_id)
            .bind(user_id)
            .execute(pool)
            .await;

            let _ = sqlx::query(
                "INSERT INTO subscriptions (workspace_id, tier, status) VALUES ($1, 'pro', 'active')"
            )
            .bind(workspace_id)
            .execute(pool)
            .await;

            info!("Created individual workspace for admin user");
        }

        info!("Admin user seeded with configured SEED_ADMIN_EMAIL / SEED_ADMIN_PASSWORD");
    }
}

/// Graceful shutdown handler.
///
/// Listens for SIGTERM/SIGINT, then:
/// 1. Stops accepting new HTTP connections (handled by axum's graceful shutdown)
/// 2. Sends WebSocket Close frame (code 1001) to all active sessions
/// 3. Signals the background scheduler to stop
/// 4. Waits up to SHUTDOWN_TIMEOUT_SECS for in-flight requests and background tasks
/// 5. Closes the database pool
/// 6. Logs "shutdown complete" and returns (process exits 0)
async fn shutdown_signal(
    shutdown_timeout_secs: u64,
    session_registry: Arc<SessionRegistry>,
    scheduler_shutdown_tx: watch::Sender<bool>,
    scheduler_handle: JoinHandle<()>,
    pool: sqlx::PgPool,
) {
    let shutdown_timeout = Duration::from_secs(shutdown_timeout_secs);

    // Wait for SIGTERM or SIGINT
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received SIGINT, initiating graceful shutdown");
        }
        _ = terminate => {
            info!("Received SIGTERM, initiating graceful shutdown");
        }
    }

    // ── Step 1: Stop accepting new connections ──────────────────────────────
    // (Handled automatically by axum's with_graceful_shutdown — returning from
    // this future stops the listener.)

    // ── Step 2: Close all WebSocket sessions with code 1001 ─────────────────
    info!("Closing all WebSocket sessions");
    session_registry.close_all_sessions(1001, "Server shutting down".to_string());

    // ── Step 3: Signal scheduler to stop ────────────────────────────────────
    info!("Signaling background scheduler to stop");
    let _ = scheduler_shutdown_tx.send(true);

    // ── Step 4: Wait for scheduler tasks to complete (up to timeout) ────────
    match tokio::time::timeout(shutdown_timeout, scheduler_handle).await {
        Ok(Ok(())) => {
            info!("Scheduler shutdown complete");
        }
        Ok(Err(e)) => {
            warn!("Scheduler task panicked during shutdown: {}", e);
        }
        Err(_) => {
            warn!(
                "Scheduler did not stop within {} seconds, force-terminating",
                shutdown_timeout_secs
            );
        }
    }

    // ── Step 5: Close database pool ─────────────────────────────────────────
    info!("Closing database connection pool");
    pool.close().await;

    // ── Step 6: Done ────────────────────────────────────────────────────────
    // The "shutdown complete" log is emitted by main() after this function returns.
}
