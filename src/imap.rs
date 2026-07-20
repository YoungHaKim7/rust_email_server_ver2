use anyhow::Result;
use crate::config::ServerConfig;
use crate::connection::ConnectionManager;
use crate::storage::MaildirStorage;
use crate::auth::{AuthManager, AuthState, AuthResult, SaslMechanism, UserDatabase};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

/// IMAP server state machine
#[derive(Debug, PartialEq, Clone)]
enum ImapState {
    NotAuthenticated,  // Initial state, before login
    Authenticated,     // After successful LOGIN/AUTHENTICATE
    Selected,          // After SELECT/EXAMINE a mailbox
    Logout,            // Session terminating
}

/// IMAP mailbox selection state
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct MailboxSelection {
    name: String,
    read_only: bool,
    message_count: usize,
    recent_count: usize,
    uid_validity: u32,
    uid_next: u32,
    flags: Vec<String>,
}

/// IMAP session context
struct ImapSession {
    state: ImapState,
    remote_addr: SocketAddr,
    config: ServerConfig,
    auth_state: AuthState,
    username: Option<String>,
    selected_mailbox: Option<MailboxSelection>,
    command_count: u32,
    is_encrypted: bool,
    auth_mechanism: Option<SaslMechanism>,
    login_username: Option<String>,
}

impl ImapSession {
    fn new(remote_addr: SocketAddr, config: ServerConfig) -> Self {
        Self {
            state: ImapState::NotAuthenticated,
            remote_addr,
            config,
            auth_state: AuthState::NotAuthenticated,
            username: None,
            selected_mailbox: None,
            command_count: 0,
            is_encrypted: false,
            auth_mechanism: None,
            login_username: None,
        }
    }

    #[allow(dead_code)]
    fn is_authenticated(&self) -> bool {
        matches!(self.auth_state, AuthState::Authenticated(_))
    }

    #[allow(dead_code)]
    fn reset(&mut self) {
        self.state = if matches!(self.auth_state, AuthState::Authenticated(_)) {
            ImapState::Authenticated
        } else {
            ImapState::NotAuthenticated
        };
        self.selected_mailbox = None;
    }
}

/// Handle individual IMAP session
async fn handle_imap_session(
    mut socket: TcpStream,
    addr: SocketAddr,
    config: ServerConfig,
    connection_manager: Arc<ConnectionManager>,
    storage: Arc<MaildirStorage>,
) -> Result<()> {
    let start_time = std::time::Instant::now();
    info!("📨 New IMAP session from {}", addr);

    let (reader, mut writer) = socket.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Send IMAP greeting
    let greeting = format!(
        "* OK [CAPABILITY IMAP4rev1 STARTTLS AUTH=PLAIN AUTH=LOGIN] {} IMAP Server Ready\r\n",
        config.server.hostname
    );
    writer.write_all(greeting.as_bytes()).await?;

    let mut session = ImapSession::new(addr, config.clone());

    loop {
        // Check timeout
        if start_time.elapsed() > config.connection_timeout() {
            warn!("⏰ Connection timeout for {}", addr);
            writer
                .write_all(b"* BYE Connection timed out\r\n")
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
            Ok(Ok(_bytes_read)) => {
                session.command_count += 1;

                let command = line.trim();

                if config.logging.log_commands {
                    debug!("📥 Received from {}: {}", addr, command);
                }

                let response = process_imap_command(&mut session, command, &connection_manager, &storage);

                if !response.is_empty() {
                    writer.write_all(response.as_bytes()).await?;

                    if config.logging.log_commands {
                        debug!("📤 Sent to {}: {}", addr, response.trim());
                    }
                }

                if session.state == ImapState::Logout {
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
                    .write_all(b"* BYE Connection timed out\r\n")
                    .await?;
                return Ok(());
            }
        }
    }
}

/// Process individual IMAP commands
fn process_imap_command(
    session: &mut ImapSession,
    command: &str,
    connection_manager: &ConnectionManager,
    storage: &Arc<MaildirStorage>
) -> String {
    // Parse IMAP command: TAG COMMAND ARGUMENTS
    let parts: Vec<&str> = command.split_whitespace().collect();

    if parts.is_empty() {
        return "* BAD Null command\r\n".to_string();
    }

    let tag = parts[0];
    let cmd = if parts.len() > 1 { parts[1].to_uppercase() } else { "".to_string() };

    // Check line length limit
    if command.len() > session.config.limits.max_line_length {
        return format!("{} BAD Line too long\r\n", tag);
    }

    debug!("🔍 Processing command: TAG={}, CMD={}, STATE={:?}", tag, cmd, session.state);

    let response = match session.state {
        ImapState::NotAuthenticated => process_not_authenticated_state(session, tag, &cmd, &parts[2..], connection_manager, storage),
        ImapState::Authenticated => process_authenticated_state(session, tag, &cmd, &parts[2..], connection_manager, storage),
        ImapState::Selected => process_selected_state(session, tag, &cmd, &parts[2..], connection_manager, storage),
        ImapState::Logout => format!("{} BAD Already in LOGOUT state\r\n", tag),
    };

    response
}

/// Process commands in NotAuthenticated state
fn process_not_authenticated_state(
    session: &mut ImapSession,
    tag: &str,
    cmd: &str,
    args: &[&str],
    _connection_manager: &ConnectionManager,
    _storage: &Arc<MaildirStorage>,
) -> String {
    match cmd {
        "CAPABILITY" => {
            let capabilities = if session.is_encrypted {
                "IMAP4rev1 AUTH=PLAIN AUTH=LOGIN"
            } else {
                "IMAP4rev1 STARTTLS AUTH=PLAIN AUTH=LOGIN"
            };
            format!("* CAPABILITY {}\r\n{} OK CAPABILITY completed\r\n", capabilities, tag)
        }
        "STARTTLS" => {
            if session.config.is_tls_enabled() && !session.is_encrypted {
                session.state = ImapState::NotAuthenticated; // State remains, will upgrade to TLS
                format!("{} OK Begin TLS negotiation\r\n", tag)
            } else if session.is_encrypted {
                format!("{} BAD TLS already active\r\n", tag)
            } else {
                format!("{} BAD TLS not available\r\n", tag)
            }
        }
        "AUTHENTICATE" => {
            if args.is_empty() {
                return format!("{} BAD Missing AUTHENTICATE mechanism\r\n", tag);
            }

            let mechanism = args[0].to_uppercase();
            if let Some(sasl_mech) = SaslMechanism::from_str(&mechanism) {
                session.auth_mechanism = Some(sasl_mech.clone());

                match sasl_mech {
                    SaslMechanism::Plain => {
                        // AUTHENTICATE PLAIN expects continuation response
                        format!("+ \r\n")
                    }
                    SaslMechanism::Login => {
                        session.login_username = None;
                        let auth_manager = create_auth_manager();
                        let username_prompt = auth_manager.encode_login_response("User Name");
                        format!("+ {}\r\n", username_prompt)
                    }
                }
            } else {
                format!("{} NO Unsupported authentication mechanism\r\n", tag)
            }
        }
        "LOGIN" => {
            if args.len() < 2 {
                return format!("{} BAD Missing LOGIN arguments\r\n", tag);
            }

            let username = args[0];
            let password = args[1];
            let client_ip = session.remote_addr.ip().to_string();
            let auth_manager = create_auth_manager();

            match auth_manager.authenticate(username, password, &client_ip) {
                AuthResult::Success => {
                    session.state = ImapState::Authenticated;
                    session.auth_state = AuthState::Authenticated(username.to_string());
                    session.username = Some(username.to_string());
                    info!("✅ User {} authenticated via IMAP LOGIN", username);
                    format!("{} OK LOGIN completed\r\n", tag)
                }
                AuthResult::InvalidCredentials => {
                    warn!("❌ Invalid credentials for IMAP LOGIN user '{}'", username);
                    format!("{} NO LOGIN failed\r\n", tag)
                }
                AuthResult::TooManyAttempts => {
                    format!("{} NO Too many authentication failures\r\n", tag)
                }
                _ => {
                    format!("{} NO Authentication failed\r\n", tag)
                }
            }
        }
        "LOGOUT" => {
            session.state = ImapState::Logout;
            format!("* BYE IMAP server logging out\r\n{} OK LOGOUT completed\r\n", tag)
        }
        "NOOP" => {
            format!("{} OK NOOP completed\r\n", tag)
        }
        _ => {
            warn!("❌ Invalid command in NotAuthenticated state: {}", cmd);
            format!("{} BAD Command not recognized in this state\r\n", tag)
        }
    }
}

/// Process commands in Authenticated state
fn process_authenticated_state(
    session: &mut ImapSession,
    tag: &str,
    cmd: &str,
    args: &[&str],
    _connection_manager: &ConnectionManager,
    _storage: &Arc<MaildirStorage>,
) -> String {
    match cmd {
        "CAPABILITY" => {
            let capabilities = if session.is_encrypted {
                "IMAP4rev1 AUTH=PLAIN AUTH=LOGIN"
            } else {
                "IMAP4rev1 STARTTLS AUTH=PLAIN AUTH=LOGIN"
            };
            format!("* CAPABILITY {}\r\n{} OK CAPABILITY completed\r\n", capabilities, tag)
        }
        "SELECT" | "EXAMINE" => {
            if args.is_empty() {
                return format!("{} BAD Missing mailbox name\r\n", tag);
            }

            let mailbox_name = args[0].to_string();
            let read_only = cmd == "EXAMINE";

            // For now, we'll just implement a basic SELECT with simulated mailbox data
            // In a full implementation, this would check the actual mailbox storage

            info!("📁 User {} selecting mailbox '{}' (read-only: {})",
                  session.username.as_ref().unwrap_or(&"unknown".to_string()),
                  mailbox_name,
                  read_only);

            session.selected_mailbox = Some(MailboxSelection {
                name: mailbox_name.clone(),
                read_only,
                message_count: 0, // Will be populated from actual storage
                recent_count: 0,
                uid_validity: 1,
                uid_next: 1,
                flags: vec!["\\Answered".to_string(), "\\Flagged".to_string(), "\\Deleted".to_string(), "\\Seen".to_string(), "\\Draft".to_string()],
            });

            session.state = ImapState::Selected;

            // Return standard SELECT response
            let response = format!(
                "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
                 * OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted\r\n\
                 * 0 EXISTS\r\n\
                 * 0 RECENT\r\n\
                 * OK [UIDVALIDITY 1] UIDs valid\r\n\
                 * OK [UIDNEXT 1] Predicted next UID\r\n\
                 {} OK {} completed\r\n",
                tag, if read_only { "EXAMINE" } else { "SELECT" }
            );

            response
        }
        "CREATE" => {
            if args.is_empty() {
                return format!("{} BAD Missing mailbox name\r\n", tag);
            }
            info!("📁 Creating mailbox: {}", args[0]);
            format!("{} OK CREATE completed\r\n", tag)
        }
        "DELETE" => {
            if args.is_empty() {
                return format!("{} BAD Missing mailbox name\r\n", tag);
            }
            info!("📁 Deleting mailbox: {}", args[0]);
            format!("{} OK DELETE completed\r\n", tag)
        }
        "RENAME" => {
            if args.len() < 2 {
                return format!("{} BAD Missing mailbox arguments\r\n", tag);
            }
            info!("📁 Renaming mailbox '{}' to '{}'", args[0], args[1]);
            format!("{} OK RENAME completed\r\n", tag)
        }
        "LIST" => {
            if args.len() < 2 {
                return format!("{} BAD Missing LIST arguments\r\n", tag);
            }
            // Basic LIST response - in full implementation would scan actual mailboxes
            format!("* LIST () \"/\" \"INBOX\"\r\n{} OK LIST completed\r\n", tag)
        }
        "LSUB" => {
            if args.len() < 2 {
                return format!("{} BAD Missing LSUB arguments\r\n", tag);
            }
            // Basic LSUB response - in full implementation would scan subscriptions
            format!("* LSUB () \"/\" \"INBOX\"\r\n{} OK LSUB completed\r\n", tag)
        }
        "STATUS" => {
            if args.len() < 2 {
                return format!("{} BAD Missing STATUS arguments\r\n", tag);
            }
            format!("* STATUS {} (MESSAGES 0 RECENT 0 UIDNEXT 1 UIDVALIDITY 1 UNSEEN 0)\r\n{} OK STATUS completed\r\n", args[0], tag)
        }
        "LOGOUT" => {
            session.state = ImapState::Logout;
            format!("* BYE IMAP server logging out\r\n{} OK LOGOUT completed\r\n", tag)
        }
        "NOOP" => {
            format!("{} OK NOOP completed\r\n", tag)
        }
        _ => {
            warn!("❌ Invalid command in Authenticated state: {}", cmd);
            format!("{} BAD Command not recognized in this state\r\n", tag)
        }
    }
}

/// Process commands in Selected state (mailbox selected)
fn process_selected_state(
    session: &mut ImapSession,
    tag: &str,
    cmd: &str,
    args: &[&str],
    _connection_manager: &ConnectionManager,
    _storage: &Arc<MaildirStorage>,
) -> String {
    match cmd {
        "CAPABILITY" => {
            let capabilities = if session.is_encrypted {
                "IMAP4rev1 AUTH=PLAIN AUTH=LOGIN"
            } else {
                "IMAP4rev1 STARTTLS AUTH=PLAIN AUTH=LOGIN"
            };
            format!("* CAPABILITY {}\r\n{} OK CAPABILITY completed\r\n", capabilities, tag)
        }
        "SELECT" | "EXAMINE" => {
            if args.is_empty() {
                return format!("{} BAD Missing mailbox name\r\n", tag);
            }

            // Deselect current mailbox and select new one
            let mailbox_name = args[0].to_string();
            let read_only = cmd == "EXAMINE";

            info!("📁 User {} switching to mailbox '{}'",
                  session.username.as_ref().unwrap_or(&"unknown".to_string()),
                  mailbox_name);

            session.selected_mailbox = Some(MailboxSelection {
                name: mailbox_name.clone(),
                read_only,
                message_count: 0,
                recent_count: 0,
                uid_validity: 1,
                uid_next: 1,
                flags: vec!["\\Answered".to_string(), "\\Flagged".to_string(), "\\Deleted".to_string(), "\\Seen".to_string(), "\\Draft".to_string()],
            });

            let response = format!(
                "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
                 * OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted\r\n\
                 * 0 EXISTS\r\n\
                 * 0 RECENT\r\n\
                 * OK [UIDVALIDITY 1] UIDs valid\r\n\
                 * OK [UIDNEXT 1] Predicted next UID\r\n\
                 {} OK {} completed\r\n",
                tag, if read_only { "EXAMINE" } else { "SELECT" }
            );

            response
        }
        "SEARCH" => {
            // For now, return empty search results
            format!("* SEARCH\r\n{} OK SEARCH completed\r\n", tag)
        }
        "FETCH" => {
            if args.is_empty() {
                return format!("{} BAD Missing FETCH arguments\r\n", tag);
            }

            // Parse FETCH command: FETCH <seq-set> <data-item>
            // For now, just return a basic response
            format!("{} OK FETCH completed\r\n", tag)
        }
        "STORE" => {
            if args.len() < 2 {
                return format!("{} BAD Missing STORE arguments\r\n", tag);
            }
            info!("📝 Storing flags: {}", args.join(" "));
            format!("{} OK STORE completed\r\n", tag)
        }
        "EXPUNGE" => {
            info!("🗑️  Expunging deleted emails");
            format!("{} OK EXPUNGE completed\r\n", tag)
        }
        "CLOSE" => {
            session.selected_mailbox = None;
            session.state = ImapState::Authenticated;
            format!("{} OK CLOSE completed\r\n", tag)
        }
        "LOGOUT" => {
            session.state = ImapState::Logout;
            format!("* BYE IMAP server logging out\r\n{} OK LOGOUT completed\r\n", tag)
        }
        "NOOP" => {
            format!("{} OK NOOP completed\r\n", tag)
        }
        _ => {
            warn!("❌ Invalid command in Selected state: {}", cmd);
            format!("{} BAD Command not recognized in this state\r\n", tag)
        }
    }
}

/// Create authentication manager with user database
fn create_auth_manager() -> AuthManager {
    let user_db = Arc::new(UserDatabase::new());
    AuthManager::new(user_db)
}

/// Start the IMAP server
pub async fn start_imap_server(config: ServerConfig) -> Result<()> {
    info!("🚀 Starting IMAP server...");

    // Initialize storage if enabled
    let storage = if config.is_storage_enabled() {
        let storage_path = config.storage_path().unwrap_or_else(|| "mail_storage".to_string());
        let path = std::path::PathBuf::from(&storage_path);
        match MaildirStorage::new(path) {
            Ok(storage) => {
                info!("💾 IMAP: Email storage enabled at: {}", storage_path);
                Some(Arc::new(storage))
            }
            Err(e) => {
                warn!("⚠️  Failed to initialize storage for IMAP: {}", e);
                None
            }
        }
    } else {
        info!("ℹ️  IMAP: Email storage disabled");
        None
    };

    // Create connection manager
    let connection_manager = Arc::new(ConnectionManager::new(config.clone()));

    // Bind to address (default IMAP port 143)
    let imap_bind_addr = config.imap_bind_address();
    let listener = TcpListener::bind(&imap_bind_addr).await?;
    info!("🚀 IMAP server listening on {}", imap_bind_addr);

    // Accept connections
    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                let manager = connection_manager.clone();
                let config = config.clone();
                let storage = storage.clone();

                // Check if we can accept the connection
                if !manager.can_accept_connection() {
                    warn!("⚠️  IMAP connection limit reached, rejecting {}", addr);
                    manager.increment_rejected();
                    continue;
                }

                manager.increment_total();
                manager.increment_active();

                let stats = manager.get_stats();
                info!("📨 IMAP: Accepted connection from {} (active: {})", addr, stats.active_connections);

                tokio::spawn(async move {
                    let start_time = std::time::Instant::now();

                    let storage_ref = if let Some(stg) = storage.as_ref() {
                        stg.clone()
                    } else {
                        Arc::new(MaildirStorage::dummy())
                    };

                    if let Err(e) = handle_imap_session(socket, addr, config.clone(), manager.clone(), storage_ref).await {
                        error!("❌ Error handling IMAP connection from {}: {}", addr, e);
                    }

                    let elapsed = start_time.elapsed();
                    info!("📭 IMAP connection {} closed after {:?}", addr, elapsed);
                });
            }
            Err(e) => {
                error!("❌ Error accepting IMAP connection: {}", e);
            }
        }
    }
}