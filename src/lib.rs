pub mod config;
pub mod connection;
pub mod email;
pub mod storage;
pub mod tls;
pub mod auth;
pub mod imap;

// Re-export public API functions
pub use imap::start_imap_server;

use anyhow::Result;
use config::ServerConfig;
use connection::ConnectionManager;
use email::EmailMessage;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use storage::MaildirStorage;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};
use auth::{AuthManager, AuthState, AuthResult, SaslMechanism, UserDatabase};

/// SMTP server state machine
#[derive(Debug, PartialEq, Clone)]
enum SmtpState {
    Greeting,
    Helo,
    StartTls,     // Added for STARTTLS support
    Auth,         // Added for AUTH support
    Authenticated, // After successful AUTH
    MailFrom,
    RcptTo,
    Data,
    Quit,
}

/// SMTP session context
struct SmtpSession {
    state: SmtpState,
    remote_addr: SocketAddr,
    mail_from: Option<String>,
    rcpt_to: Vec<String>,
    data_buffer: Vec<String>,
    config: ServerConfig,
    bytes_received: usize,
    is_encrypted: bool, // Track if connection is encrypted
    auth_state: AuthState, // Authentication state
    auth_mechanism: Option<SaslMechanism>, // Current AUTH mechanism in progress
    username: Option<String>, // Authenticated username
    login_username: Option<String>, // Temporary storage for LOGIN username
}

impl SmtpSession {
    fn new(remote_addr: SocketAddr, config: ServerConfig) -> Self {
        Self {
            state: SmtpState::Greeting,
            remote_addr,
            mail_from: None,
            rcpt_to: Vec::new(),
            data_buffer: Vec::new(),
            config,
            bytes_received: 0,
            is_encrypted: false,
            auth_state: AuthState::NotAuthenticated,
            auth_mechanism: None,
            username: None,
            login_username: None,
        }
    }

    fn reset(&mut self) {
        self.state = if matches!(self.auth_state, AuthState::Authenticated(_)) {
            SmtpState::Authenticated
        } else {
            SmtpState::Helo
        };
        self.mail_from = None;
        self.rcpt_to.clear();
        self.data_buffer.clear();
        self.bytes_received = 0;
        // Don't reset auth state on RSET
    }

    fn is_authenticated(&self) -> bool {
        matches!(self.auth_state, AuthState::Authenticated(_))
    }

    fn require_auth(&self) -> bool {
        // Respect the config setting for authentication requirement
        self.config.auth.require_auth
    }

    fn can_add_recipient(&self) -> bool {
        self.rcpt_to.len() < self.config.limits.max_recipients
    }
}

/// Handle individual SMTP session with timeout and proper error handling
async fn handle_smtp_session(
    mut socket: TcpStream,
    addr: SocketAddr,
    config: ServerConfig,
    connection_manager: Arc<ConnectionManager>,
    storage: Arc<MaildirStorage>,
) -> Result<()> {
    let start_time = std::time::Instant::now();
    info!("📨 New SMTP session from {}", addr);

    let (reader, mut writer) = socket.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Send greeting
    let greeting = format!(
        "220 {} ESMTP Rust Email Server\r\n",
        config.server.hostname
    );
    writer.write_all(greeting.as_bytes()).await?;

    let mut session = SmtpSession::new(addr, config.clone());

    loop {
        // Check timeout
        if start_time.elapsed() > config.connection_timeout() {
            warn!("⏰ Connection timeout for {}", addr);
            writer
                .write_all(b"421 4.4.2 Connection timed out\r\n")
                .await?;
            return Ok(());
        }

        line.clear();

        // Read line with timeout
        let read_result = tokio::time::timeout(
            Duration::from_secs(30),
            reader.read_line(&mut line),
        )
        .await;

        match read_result {
            Ok(Ok(0)) => {
                info!("📭 Client {} disconnected gracefully", addr);
                return Ok(());
            }
            Ok(Ok(bytes_read)) => {
                session.bytes_received += bytes_read;

                // Check message size limit
                if session.bytes_received > session.config.limits.max_message_size {
                    warn!("📏 Message size exceeded for {}", addr);
                    writer
                        .write_all(b"552 5.3.4 Message size exceeds fixed maximum message size\r\n")
                        .await?;
                    return Ok(());
                }

                let command = line.trim();

                if config.logging.log_commands {
                    debug!("📥 Received from {}: {}", addr, command);
                }

                let response = process_command(&mut session, command, &connection_manager, &storage);

                if !response.is_empty() {
                    writer.write_all(response.as_bytes()).await?;

                    if config.logging.log_commands {
                        debug!("📤 Sent to {}: {}", addr, response.trim());
                    }
                }

                if session.state == SmtpState::Quit {
                    info!("✅ Session with {} completed successfully", addr);
                    return Ok(());
                }
            }
            Ok(Err(e)) => {
                error!("❌ Read error from {}: {}", addr, e);
                return Err(e.into());
            }
            Err(_) => {
                warn!("⏰ Read timeout for {}", addr);
                writer
                    .write_all(b"421 4.4.2 Connection timed out\r\n")
                    .await?;
                return Ok(());
            }
        }
    }
}

/// Process individual SMTP commands with enhanced validation
fn process_command(session: &mut SmtpSession, command: &str, connection_manager: &ConnectionManager, storage: &Arc<MaildirStorage>) -> String {
    // Special handling for DATA state - all lines are data, including empty ones
    // This must come BEFORE the empty command check
    if session.state == SmtpState::Data {
        if command == "." {
            debug!("📨 End of data marker received");
            // End of data - continue to processing below
        } else {
            // Store any non-terminating line as data, including empty lines
            if session.config.logging.log_commands {
                debug!("📨 Storing data line: '{}'", command);
            }
            session.data_buffer.push(command.to_string());
            return String::new(); // Don't respond during data collection
        }
    }

    let parts: Vec<&str> = command.split_whitespace().collect();

    if parts.is_empty() {
        return "500 5.5.2 Syntax error: command unrecognized\r\n".to_string();
    }

    // Check line length limit
    if command.len() > session.config.limits.max_line_length {
        return "500 5.5.2 Line too long\r\n".to_string();
    }

    let cmd = parts[0].to_uppercase();
    let response = match session.state {
        SmtpState::Greeting => match cmd.as_str() {
            "HELO" | "EHLO" => {
                session.state = SmtpState::Helo;
                if cmd == "EHLO" {
                    // Advertise STARTTLS support if TLS is enabled
                    let tls_advertisement = if session.config.is_tls_enabled() && !session.is_encrypted {
                        "250-STARTTLS\r\n".to_string()
                    } else {
                        String::new()
                    };

                    // Advertise AUTH support (always available)
                    let auth_advertisement = "250-AUTH PLAIN LOGIN\r\n".to_string();

                    format!(
                        "250-{}\r\n250-SIZE {}\r\n{}{}250 HELP\r\n",
                        session.config.server.hostname,
                        session.config.limits.max_message_size,
                        tls_advertisement,
                        auth_advertisement
                    )
                } else {
                    format!("250 {}\r\n", session.config.server.hostname)
                }
            }
            "QUIT" => {
                session.state = SmtpState::Quit;
                "221 2.0.0 Bye\r\n".to_string()
            }
            _ => "503 5.5.1 Bad sequence of commands\r\n".to_string(),
        },
        SmtpState::Helo => match cmd.as_str() {
            "AUTH" => {
                // Handle AUTH command
                if parts.len() < 2 {
                    return "501 5.5.4 Syntax: AUTH <mechanism>\r\n".to_string();
                }

                let mechanism = parts[1].to_uppercase();
                if let Some(sasl_mech) = SaslMechanism::from_str(&mechanism) {
                    session.state = SmtpState::Auth;
                    session.auth_mechanism = Some(sasl_mech.clone());

                    match sasl_mech {
                        SaslMechanism::Plain => {
                            // AUTH PLAIN can be sent with credentials immediately
                            if parts.len() >= 3 {
                                // Credentials provided with AUTH command
                                let credentials = parts[2];
                                let auth_manager = create_auth_manager();

                                match auth_manager.parse_plain_credentials(credentials) {
                                    Ok((username, password)) => {
                                        let client_ip = session.remote_addr.ip().to_string();
                                        match auth_manager.authenticate(&username, &password, &client_ip) {
                                            AuthResult::Success => {
                                                session.state = SmtpState::Authenticated;
                                                session.auth_state = AuthState::Authenticated(username.clone());
                                                session.username = Some(username.clone());
                                                info!("✅ User {} authenticated via AUTH PLAIN", username);
                                                "235 2.7.0 Authentication successful\r\n".to_string()
                                            }
                                            AuthResult::InvalidCredentials => {
                                                session.state = SmtpState::Helo;
                                                session.auth_state = AuthState::Failed(1);
                                                session.auth_mechanism = None;
                                                warn!("❌ Invalid credentials for AUTH PLAIN");
                                                "535 5.7.8 Authentication failed\r\n".to_string()
                                            }
                                            AuthResult::TooManyAttempts => {
                                                session.state = SmtpState::Helo;
                                                session.auth_state = AuthState::Failed(4);
                                                session.auth_mechanism = None;
                                                "421 4.7.1 Too many authentication failures\r\n".to_string()
                                            }
                                            _ => {
                                                session.state = SmtpState::Helo;
                                                session.auth_mechanism = None;
                                                "504 5.7.4 Unrecognized authentication type\r\n".to_string()
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        session.state = SmtpState::Helo;
                                        session.auth_mechanism = None;
                                        warn!("❌ Invalid AUTH PLAIN format: {}", e);
                                        "501 5.7.0 Invalid authentication format\r\n".to_string()
                                    }
                                }
                            } else {
                                // Request credentials (empty response for PLAIN)
                                "334 \r\n".to_string()
                            }
                        }
                        SaslMechanism::Login => {
                            // AUTH LOGIN is a two-step process
                            // First send username prompt, then password prompt
                            session.login_username = None;
                            // Send base64 encoded "Username:" prompt
                            let auth_manager = create_auth_manager();
                            let username_prompt = auth_manager.encode_login_response("User Name");
                            format!("334 {}\r\n", username_prompt)
                        }
                    }
                } else {
                    warn!("❌ Unsupported AUTH mechanism: {}", mechanism);
                    "504 5.7.4 Unrecognized authentication type\r\n".to_string()
                }
            }
            "STARTTLS" => {
                if session.config.is_tls_enabled() && !session.is_encrypted {
                    session.state = SmtpState::StartTls;
                    "220 2.0.0 Ready to start TLS\r\n".to_string()
                } else if session.is_encrypted {
                    "503 5.5.1 TLS already active\r\n".to_string()
                } else {
                    "503 5.5.1 TLS not available\r\n".to_string()
                }
            }
            "MAIL" => {
                // Require authentication for MAIL FROM
                if session.require_auth() && !session.is_authenticated() {
                    return "530 5.7.0 Authentication required\r\n".to_string();
                }

                if command.to_uppercase().starts_with("MAIL FROM:") {
                    session.state = SmtpState::MailFrom;
                    session.mail_from = extract_email(command);
                    "250 2.1.0 Ok\r\n".to_string()
                } else {
                    "501 5.5.4 Syntax error in parameters or arguments\r\n".to_string()
                }
            }
            "RSET" => {
                session.reset();
                "250 2.0.0 Ok\r\n".to_string()
            }
            "QUIT" => {
                session.state = SmtpState::Quit;
                "221 2.0.0 Bye\r\n".to_string()
            }
            _ => "503 5.5.1 Bad sequence of commands\r\n".to_string(),
        },
        SmtpState::MailFrom => match cmd.as_str() {
            "RCPT" => {
                if command.to_uppercase().starts_with("RCPT TO:") {
                    session.state = SmtpState::RcptTo;
                    if let Some(email) = extract_email(command) {
                        session.rcpt_to.push(email);
                        "250 2.1.5 Ok\r\n".to_string()
                    } else {
                        "501 5.5.4 Syntax error in parameters\r\n".to_string()
                    }
                } else {
                    "501 5.5.4 Syntax error in parameters or arguments\r\n".to_string()
                }
            }
            "RSET" => {
                session.reset();
                "250 2.0.0 Ok\r\n".to_string()
            }
            "QUIT" => {
                session.state = SmtpState::Quit;
                "221 2.0.0 Bye\r\n".to_string()
            }
            _ => "503 5.5.1 Bad sequence of commands\r\n".to_string(),
        },
        SmtpState::RcptTo => match cmd.as_str() {
            "RCPT" => {
                if !session.can_add_recipient() {
                    return "452 4.5.3 Too many recipients\r\n".to_string();
                }

                if command.to_uppercase().starts_with("RCPT TO:") {
                    if let Some(email) = extract_email(command) {
                        session.rcpt_to.push(email);
                        "250 2.1.5 Ok\r\n".to_string()
                    } else {
                        "501 5.5.4 Syntax error in parameters\r\n".to_string()
                    }
                } else {
                    "501 5.5.4 Syntax error in parameters or arguments\r\n".to_string()
                }
            }
            "DATA" => {
                session.state = SmtpState::Data;
                "354 End data with <CR><LF>.<CR><LF>\r\n".to_string()
            }
            "RSET" => {
                session.reset();
                "250 2.0.0 Ok\r\n".to_string()
            }
            "QUIT" => {
                session.state = SmtpState::Quit;
                "221 2.0.0 Bye\r\n".to_string()
            }
            _ => "503 5.5.1 Bad sequence of commands\r\n".to_string(),
        },
        SmtpState::Data => {
            if command == "." {
                // End of data - parse and process the email
                info!("📧 Email received from {}:", session.remote_addr);
                info!("  From: {:?}", session.mail_from);
                info!("  To: {:?}", session.rcpt_to);
                info!("  Body: {} lines", session.data_buffer.len());
                info!("  Size: {} bytes", session.bytes_received);

                // Reconstruct raw email with proper format - ensuring empty lines create proper separators
                let raw_email = session.data_buffer.join("\r\n");

                debug!("📧 Reconstructed email ({} bytes)", raw_email.len());
                debug!("📧 First 100 chars: '{}'", raw_email.chars().take(100).collect::<String>());

                // Parse the email
                match EmailMessage::parse(&raw_email) {
                    Ok(mut email) => {
                        // Override with SMTP envelope info if needed
                        if email.headers.from.is_none() {
                            email.headers.from = session.mail_from.clone();
                        }
                        if email.headers.to.is_empty() {
                            email.headers.to = session.rcpt_to.clone();
                        }

                        info!("✅ Email parsed successfully:");
                        info!("  📋 Summary: {}", email.summary());
                        info!("  📝 Body Preview: {}", email.body_preview(100));

                        // Save to storage
                        match storage.save_email(&email) {
                            Ok(filename) => {
                                info!("💾 Email saved to storage: {}", filename);
                            }
                            Err(e) => {
                                warn!("⚠️  Failed to save email to storage: {}", e);
                            }
                        }

                        // Track statistics
                        connection_manager.message_received();

                        session.reset();
                        "250 2.0.0 Ok: queued as ABC123\r\n".to_string()
                    }
                    Err(e) => {
                        error!("❌ Failed to parse email: {}", e);
                        error!("  Raw email data (first 300 chars): {}",
                               raw_email.chars().take(300).collect::<String>());
                        error!("  Data buffer length: {}", session.data_buffer.len());

                        session.reset();
                        "250 2.0.0 Ok: queued with parsing errors\r\n".to_string()
                    }
                }
            } else {
                // This should never be reached since we handle all non-"." lines above
                String::new()
            }
        },
        SmtpState::StartTls => {
            // In StartTls state, we should not process commands - waiting for TLS upgrade
            "503 5.5.1 TLS upgrade in progress\r\n".to_string()
        },
        SmtpState::Auth => {
            // Currently in authentication process - waiting for credentials
            match session.auth_mechanism {
                Some(SaslMechanism::Plain) => {
                    // Handle AUTH PLAIN continuation
                    let credentials = command.trim();
                    let auth_manager = create_auth_manager();

                    if credentials == "*" {
                        // Cancel authentication
                        session.state = SmtpState::Helo;
                        session.auth_state = AuthState::NotAuthenticated;
                        session.auth_mechanism = None;
                        return "501 5.7.0 Authentication cancelled\r\n".to_string();
                    }

                    match auth_manager.parse_plain_credentials(credentials) {
                        Ok((username, password)) => {
                            let client_ip = session.remote_addr.ip().to_string();
                            match auth_manager.authenticate(&username, &password, &client_ip) {
                                AuthResult::Success => {
                                    session.state = SmtpState::Authenticated;
                                    session.auth_state = AuthState::Authenticated(username.clone());
                                    session.username = Some(username.clone());
                                    session.auth_mechanism = None;
                                    info!("✅ User {} authenticated via AUTH PLAIN", username);
                                    "235 2.7.0 Authentication successful\r\n".to_string()
                                }
                                AuthResult::InvalidCredentials => {
                                    session.state = SmtpState::Helo;
                                    session.auth_state = AuthState::Failed(1);
                                    session.auth_mechanism = None;
                                    warn!("❌ Invalid credentials for AUTH PLAIN");
                                    "535 5.7.8 Authentication failed\r\n".to_string()
                                }
                                AuthResult::TooManyAttempts => {
                                    session.state = SmtpState::Helo;
                                    session.auth_state = AuthState::Failed(4);
                                    session.auth_mechanism = None;
                                    "421 4.7.1 Too many authentication failures\r\n".to_string()
                                }
                                _ => {
                                    session.state = SmtpState::Helo;
                                    session.auth_mechanism = None;
                                    "504 5.7.4 Unrecognized authentication type\r\n".to_string()
                                }
                            }
                        }
                        Err(e) => {
                            session.state = SmtpState::Helo;
                            session.auth_mechanism = None;
                            warn!("❌ Invalid AUTH PLAIN format: {}", e);
                            "501 5.7.0 Invalid authentication format\r\n".to_string()
                        }
                    }
                }
                Some(SaslMechanism::Login) => {
                    // Handle AUTH LOGIN continuation
                    let credentials = command.trim();
                    let auth_manager = create_auth_manager();

                    if credentials == "*" {
                        // Cancel authentication
                        session.state = SmtpState::Helo;
                        session.auth_state = AuthState::NotAuthenticated;
                        session.auth_mechanism = None;
                        session.login_username = None;
                        return "501 5.7.0 Authentication cancelled\r\n".to_string();
                    }

                    if session.login_username.is_none() {
                        // This should be the username
                        match auth_manager.parse_login_credentials(credentials) {
                            Ok(username) => {
                                session.login_username = Some(username.clone());
                                // Send password prompt
                                let password_prompt = auth_manager.encode_login_response("Password");
                                format!("334 {}\r\n", password_prompt)
                            }
                            Err(e) => {
                                session.state = SmtpState::Helo;
                                session.auth_mechanism = None;
                                warn!("❌ Invalid AUTH LOGIN username: {}", e);
                                "501 5.7.0 Invalid authentication format\r\n".to_string()
                            }
                        }
                    } else {
                        // This should be the password
                        match auth_manager.parse_login_credentials(credentials) {
                            Ok(password) => {
                                let username = session.login_username.clone().unwrap_or_default();
                                let client_ip = session.remote_addr.ip().to_string();

                                match auth_manager.authenticate(&username, &password, &client_ip) {
                                    AuthResult::Success => {
                                        session.state = SmtpState::Authenticated;
                                        session.auth_state = AuthState::Authenticated(username.clone());
                                        session.username = Some(username.clone());
                                        session.login_username = None;
                                        session.auth_mechanism = None;
                                        info!("✅ User {} authenticated via AUTH LOGIN", username);
                                        "235 2.7.0 Authentication successful\r\n".to_string()
                                    }
                                    AuthResult::InvalidCredentials => {
                                        session.state = SmtpState::Helo;
                                        session.auth_state = AuthState::Failed(1);
                                        session.login_username = None;
                                        session.auth_mechanism = None;
                                        warn!("❌ Invalid credentials for AUTH LOGIN");
                                        "535 5.7.8 Authentication failed\r\n".to_string()
                                    }
                                    AuthResult::TooManyAttempts => {
                                        session.state = SmtpState::Helo;
                                        session.auth_state = AuthState::Failed(4);
                                        session.login_username = None;
                                        session.auth_mechanism = None;
                                        "421 4.7.1 Too many authentication failures\r\n".to_string()
                                    }
                                    _ => {
                                        session.state = SmtpState::Helo;
                                        session.login_username = None;
                                        session.auth_mechanism = None;
                                        "504 5.7.4 Unrecognized authentication type\r\n".to_string()
                                    }
                                }
                            }
                            Err(e) => {
                                session.state = SmtpState::Helo;
                                session.login_username = None;
                                session.auth_mechanism = None;
                                warn!("❌ Invalid AUTH LOGIN password: {}", e);
                                "501 5.7.0 Invalid authentication format\r\n".to_string()
                            }
                        }
                    }
                }
                None => {
                    // No mechanism selected (shouldn't happen)
                    session.state = SmtpState::Helo;
                    "503 5.5.1 No authentication mechanism selected\r\n".to_string()
                }
            }
        },
        SmtpState::Authenticated => {
            // User is authenticated, allow mail commands
            match cmd.as_str() {
                "MAIL" => {
                    if command.to_uppercase().starts_with("MAIL FROM:") {
                        session.state = SmtpState::MailFrom;
                        session.mail_from = extract_email(command);
                        "250 2.1.0 Ok\r\n".to_string()
                    } else {
                        "501 5.5.4 Syntax error in parameters or arguments\r\n".to_string()
                    }
                }
                "RSET" => {
                    session.reset();
                    "250 2.0.0 Ok\r\n".to_string()
                }
                "QUIT" => {
                    session.state = SmtpState::Quit;
                    "221 2.0.0 Bye\r\n".to_string()
                }
                "NOOP" => {
                    "250 2.0.0 Ok\r\n".to_string()
                }
                _ => "503 5.5.1 Bad sequence of commands\r\n".to_string(),
            }
        },
        SmtpState::Quit => "221 2.0.0 Bye\r\n".to_string(),
    };

    response
}

/// Create authentication manager with user database
fn create_auth_manager() -> AuthManager {
    let user_db = Arc::new(UserDatabase::new());
    AuthManager::new(user_db)
}

/// Extract email address from command
fn extract_email(command: &str) -> Option<String> {
    // Handle both "MAIL FROM:<email>" and "MAIL FROM: <email>" formats
    let parts: Vec<&str> = command.split(':').collect();
    if parts.len() >= 2 {
        let email_part = parts[1].trim();
        // Remove angle brackets if present
        let email = email_part.trim_start_matches('<').trim_end_matches('>');

        // Basic email validation
        if email.contains('@') && email.len() > 3 {
            Some(email.to_string())
        } else {
            None
        }
    } else {
        None
    }
}

/// Start the enhanced SMTP server with connection management and TLS support
pub async fn start_smtp_server() -> Result<()> {
    // Load configuration
    let config = ServerConfig::load()?;
    info!("🔧 Loaded server configuration");

    // Initialize TLS if enabled
    if config.is_tls_enabled() {
        let cert_path = std::path::Path::new(&config.tls.certificate_path);
        let key_path = std::path::Path::new(&config.tls.private_key_path);

        match tls::TlsConfig::new(cert_path, key_path) {
            Ok(_tls_cfg) => {
                info!("🔐 TLS configuration loaded successfully");
                // TODO: Use TLS configuration for actual TLS connections
            }
            Err(e) => {
                warn!("⚠️  Failed to load TLS configuration: {}", e);
                warn!("📧 Server will run with STARTTLS command but no actual TLS");
            }
        }
    } else {
        info!("ℹ️  TLS disabled in configuration");
    }

    // Initialize storage if enabled
    let storage = if config.is_storage_enabled() {
        let storage_path = config.storage_path().unwrap_or_else(|| "mail_storage".to_string());
        let path = std::path::PathBuf::from(&storage_path);
        match MaildirStorage::new(path) {
            Ok(storage) => {
                info!("💾 Email storage enabled at: {}", storage_path);
                Some(Arc::new(storage))
            }
            Err(e) => {
                warn!("⚠️  Failed to initialize storage, continuing without it: {}", e);
                None
            }
        }
    } else {
        info!("ℹ️  Email storage disabled");
        None
    };

    // Create connection manager
    let connection_manager = Arc::new(ConnectionManager::new(config.clone()));

    // Bind to address
    let bind_addr = config.bind_address();
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("🚀 SMTP server listening on {}", bind_addr);
    info!("📊 Configuration:");
    info!("  Max connections: {}", config.server.max_connections);
    info!("  Max message size: {} MB", config.limits.max_message_size / 1_000_000);
    info!("  Connection timeout: {}s", config.server.connection_timeout_secs);
    if config.is_tls_enabled() {
        info!("  TLS: Enabled (STARTTLS support available)");
    }

    // Spawn stats reporter
    let stats_manager = connection_manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            stats_manager.print_stats();
        }
    });

    // Accept connections
    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                let manager = connection_manager.clone();
                let config = config.clone();
                let storage = storage.clone();

                // Check if we can accept the connection
                if !manager.can_accept_connection() {
                    warn!("⚠️  Connection limit reached, rejecting {}", addr);
                    manager.increment_rejected();
                    continue;
                }

                manager.increment_total();
                manager.increment_active();

                let stats = manager.get_stats();
                info!("📨 Accepted connection from {} (active: {})", addr, stats.active_connections);

                tokio::spawn(async move {
                    let start_time = std::time::Instant::now();

                    let storage_ref = if let Some(stg) = storage.as_ref() {
                        stg.clone()
                    } else {
                        // Create a dummy storage that doesn't persist emails if storage is disabled
                        Arc::new(MaildirStorage::dummy())
                    };

                    if let Err(e) = handle_smtp_session(socket, addr, config.clone(), manager.clone(), storage_ref).await {
                        error!("❌ Error handling connection from {}: {}", addr, e);
                    }

                    let elapsed = start_time.elapsed();
                    info!("📭 Connection {} closed after {:?}", addr, elapsed);

                    // The connection will be automatically cleaned up when ManagedConnection drops
                });
            }
            Err(e) => {
                error!("❌ Error accepting connection: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_smtp_session_creation() {
        let addr = "127.0.0.1:8080".parse().unwrap();
        let config = ServerConfig::default();
        let session = SmtpSession::new(addr, config);
        assert_eq!(session.state, SmtpState::Greeting);
        assert!(session.mail_from.is_none());
        assert!(session.rcpt_to.is_empty());
    }

    #[test]
    fn test_extract_email() {
        let email = extract_email("MAIL FROM:<test@example.com>");
        assert_eq!(email, Some("test@example.com".to_string()));

        let email2 = extract_email("RCPT TO: user@example.com");
        assert_eq!(email2, Some("user@example.com".to_string()));

        let email3 = extract_email("MAIL FROM: invalid");
        assert_eq!(email3, None);
    }

    #[test]
    fn test_session_limits() {
        let addr = "127.0.0.1:8080".parse().unwrap();
        let config = ServerConfig::default();
        let mut session = SmtpSession::new(addr, config.clone());

        // Test recipient limits
        assert!(session.can_add_recipient());
        for _ in 0..config.limits.max_recipients {
            session.rcpt_to.push("test@example.com".to_string());
        }
        assert!(!session.can_add_recipient());
    }
}
