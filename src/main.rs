use anyhow::Result;
use rust_email_server_ver2::config::ServerConfig;
use rust_email_server_ver2::{start_imap_server, start_smtp_server};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};
use tracing_appender::rolling;

#[tokio::main]
async fn main() -> Result<()> {
    println!("🚀 Starting Rust Email Server...");
    println!("📧 Enhanced SMTP & IMAP Server with Logging & Connection Management");
    println!("📝 Loading configuration and initializing services...\r\n");

    // Load configuration
    let config = ServerConfig::load()?;

    // Initialize tracing subscriber for logging
    let log_level = &config.logging.level;
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(log_level));

    // Create log directory in mail_storage/tmp
    let log_path = "mail_storage/tmp";
    let file_appender = rolling::daily(log_path, "server.log");

    // Initialize logging with both console and file output
    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_writer(std::io::stdout)
        )
        .with(
            fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false) // Disable ANSI colors in log files
        )
        .init();

    tracing::info!("🚀 Rust Email Server starting...");
    tracing::info!("📊 Logging level: {}", log_level);
    tracing::info!("📝 Configuration loaded successfully");

    // Start both SMTP and IMAP servers concurrently
    let imap_config = config.clone();

    let smtp_handle = tokio::spawn(async move {
        if let Err(e) = start_smtp_server().await {
            eprintln!("❌ SMTP server error: {}", e);
        }
    });

    let imap_handle = tokio::spawn(async move {
        if let Err(e) = start_imap_server(imap_config).await {
            eprintln!("❌ IMAP server error: {}", e);
        }
    });

    // Wait for both servers
    let (smtp_result, imap_result) = tokio::join!(smtp_handle, imap_handle);

    if let Err(e) = smtp_result {
        eprintln!("❌ SMTP server task failed: {}", e);
    }

    if let Err(e) = imap_result {
        eprintln!("❌ IMAP server task failed: {}", e);
    }

    Ok(())
}
