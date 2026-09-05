use std::sync::Arc;

use lettre::{
    message::{header::ContentType, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};

use tracing::{debug, error, info};
use crate::config::AppConfig;

use super::service::{EmailMessage, EmailProvider};

/// SMTP-based email provider using `lettre` with TLS support.
pub struct SmtpProvider {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    host: String,
    port: u16,
    from_address: String,
    from_name: String,
}

impl SmtpProvider {
    /// Create a new `SmtpProvider` from the application config.
    ///
    /// Panics if the required SMTP config fields are missing (host, port, user, password).
    /// Uses implicit TLS (SMTPS) on port 465, STARTTLS on all other ports.
    pub fn new(config: &Arc<AppConfig>) -> Self {
        let host = config
            .email_smtp_host
            .as_deref()
            .expect("EMAIL_SMTP_HOST is required for SMTP provider");
        let port = config
            .email_smtp_port
            .expect("EMAIL_SMTP_PORT is required for SMTP provider");
        let user = config
            .email_smtp_user
            .as_deref()
            .expect("EMAIL_SMTP_USER is required for SMTP provider");
        let password = config
            .email_smtp_password
            .as_deref()
            .expect("EMAIL_SMTP_PASSWORD is required for SMTP provider");

        let credentials = Credentials::new(user.to_string(), password.to_string());

        let tls_mode = if port == 465 { "implicit TLS (SMTPS)" } else { "STARTTLS" };

        let transport = if port == 465 {
            // Port 465: implicit TLS (SMTPS)
            AsyncSmtpTransport::<Tokio1Executor>::relay(host)
                .expect("Failed to create SMTP relay transport")
                .port(port)
                .credentials(credentials)
                .build()
        } else {
            // Other ports (587, 25, etc.): STARTTLS
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                .expect("Failed to create SMTP STARTTLS transport")
                .port(port)
                .credentials(credentials)
                .build()
        };

        info!(
            host = %host,
            port = port,
            tls = tls_mode,
            user = %user,
            from = %config.email_from_address,
            "SMTP email provider initialized"
        );

        Self {
            transport,
            host: host.to_string(),
            port,
            from_address: config.email_from_address.clone(),
            from_name: config.email_from_name.clone(),
        }
    }
}

#[async_trait::async_trait]
impl EmailProvider for SmtpProvider {
    async fn send(
        &self,
        message: &EmailMessage,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(
            to = %message.to,
            subject = %message.subject,
            host = %self.host,
            port = self.port,
            "Building SMTP message"
        );

        let from = format!("{} <{}>", self.from_name, self.from_address)
            .parse()
            .map_err(|e| {
                error!(from = %self.from_address, error = %e, "Invalid SMTP from address");
                format!("Invalid from address: {}", e)
            })?;

        let to = message
            .to
            .parse()
            .map_err(|e| {
                error!(to = %message.to, error = %e, "Invalid SMTP recipient address");
                format!("Invalid to address: {}", e)
            })?;

        let email = Message::builder()
            .from(from)
            .to(to)
            .subject(&message.subject)
            .multipart(
                MultiPart::alternative()
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_PLAIN)
                            .body(message.text_body.clone()),
                    )
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_HTML)
                            .body(message.html_body.clone()),
                    ),
            )
            .map_err(|e| {
                error!(error = %e, "Failed to build SMTP MIME message");
                e
            })?;

        info!(
            to = %message.to,
            subject = %message.subject,
            host = %self.host,
            port = self.port,
            "Connecting to SMTP server and sending email"
        );

        match self.transport.send(email).await {
            Ok(response) => {
                info!(
                    to = %message.to,
                    host = %self.host,
                    port = self.port,
                    smtp_code = ?response.code(),
                    smtp_message = ?response.message().collect::<Vec<_>>(),
                    "SMTP server accepted the email"
                );
                Ok(())
            }
            Err(e) => {
                error!(
                    to = %message.to,
                    host = %self.host,
                    port = self.port,
                    error = %e,
                    error_debug = ?e,
                    "SMTP send failed (check credentials, TLS/port, verified sender domain, and firewall)"
                );
                Err(Box::new(e))
            }
        }
    }
}