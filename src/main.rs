use anyhow::Result;
use rust_email_server_ver2::config::ServerConfig;
use rust_email_server_ver2::{start_imap_server, start_smtp_server};

#[tokio::main]
async fn main() -> Result<()> {
    println!("🚀 Starting Rust Email Server...");
    println!("📧 Enhanced SMTP & IMAP Server with Logging & Connection Management");
    println!("📝 Loading configuration and initializing services...\r\n");

    // Load configuration
    let config = ServerConfig::load()?;

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
