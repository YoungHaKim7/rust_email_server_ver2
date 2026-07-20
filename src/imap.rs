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
    storage: &Arc<MaildirStorage>,
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

            info!("📁 User {} selecting mailbox '{}' (read-only: {})",
                  session.username.as_ref().unwrap_or(&"unknown".to_string()),
                  mailbox_name,
                  read_only);

            // Get actual email counts from storage
            let stored_emails = storage.list_emails()
                .unwrap_or_else(|e| {
                    warn!("⚠️  Failed to list emails from storage: {}", e);
                    Vec::new()
                });

            let message_count = stored_emails.len();
            let recent_count = stored_emails.iter().filter(|e| !e.is_read).count();
            let uid_next = (message_count + 1) as u32;

            session.selected_mailbox = Some(MailboxSelection {
                name: mailbox_name.clone(),
                read_only,
                message_count,
                recent_count,
                uid_validity: 1,
                uid_next,
                flags: vec!["\\Answered".to_string(), "\\Flagged".to_string(), "\\Deleted".to_string(), "\\Seen".to_string(), "\\Draft".to_string()],
            });

            session.state = ImapState::Selected;

            // Return standard SELECT response with actual counts
            let response = format!(
                "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
                 * OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted\r\n\
                 * {} EXISTS\r\n\
                 * {} RECENT\r\n\
                 * OK [UIDVALIDITY 1] UIDs valid\r\n\
                 * OK [UIDNEXT {}] Predicted next UID\r\n\
                 {} OK {} completed\r\n",
                message_count, recent_count, uid_next, tag, if read_only { "EXAMINE" } else { "SELECT" }
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

            let mailbox_name = args[0];

            // Get actual statistics from storage
            let stored_emails = match storage.list_emails() {
                Ok(emails) => emails,
                Err(e) => {
                    warn!("⚠️  Failed to get email statistics: {}", e);
                    return format!("* STATUS {} (MESSAGES 0 RECENT 0 UIDNEXT 1 UIDVALIDITY 1 UNSEEN 0)\r\n{} OK STATUS completed\r\n", mailbox_name, tag);
                }
            };

            let messages = stored_emails.len();
            let recent = stored_emails.iter().filter(|e| !e.is_read).count();
            let unseen = recent; // For now, unseen = recent (new/ unread emails)
            let uid_next = (messages + 1) as u32;

            format!("* STATUS {} (MESSAGES {} RECENT {} UIDNEXT {} UIDVALIDITY 1 UNSEEN {})\r\n{} OK STATUS completed\r\n",
                mailbox_name, messages, recent, uid_next, unseen, tag)
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
    storage: &Arc<MaildirStorage>,
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

            // Get actual email counts from storage
            let stored_emails = storage.list_emails()
                .unwrap_or_else(|e| {
                    warn!("⚠️  Failed to list emails from storage: {}", e);
                    Vec::new()
                });

            let message_count = stored_emails.len();
            let recent_count = stored_emails.iter().filter(|e| !e.is_read).count();
            let uid_next = (message_count + 1) as u32;

            session.selected_mailbox = Some(MailboxSelection {
                name: mailbox_name.clone(),
                read_only,
                message_count,
                recent_count,
                uid_validity: 1,
                uid_next,
                flags: vec!["\\Answered".to_string(), "\\Flagged".to_string(), "\\Deleted".to_string(), "\\Seen".to_string(), "\\Draft".to_string()],
            });

            let response = format!(
                "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
                 * OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted\r\n\
                 * {} EXISTS\r\n\
                 * {} RECENT\r\n\
                 * OK [UIDVALIDITY 1] UIDs valid\r\n\
                 * OK [UIDNEXT {}] Predicted next UID\r\n\
                 {} OK {} completed\r\n",
                message_count, recent_count, uid_next, tag, if read_only { "EXAMINE" } else { "SELECT" }
            );

            response
        }
        "SEARCH" => {
            // Get all emails from storage
            let stored_emails = match storage.list_emails() {
                Ok(emails) => emails,
                Err(e) => {
                    warn!("⚠️  Failed to list emails for SEARCH: {}", e);
                    return format!("* SEARCH\r\n{} OK SEARCH completed\r\n", tag);
                }
            };

            let mut matching_sequence_numbers = Vec::new();

            // For now, implement basic search for ALL (returns all messages)
            // TODO: Implement proper search criteria parsing (FROM, TO, SUBJECT, etc.)
            for (index, stored_email) in stored_emails.iter().enumerate() {
                let seq_num = (index + 1) as u32;

                // Check if search criteria are provided, otherwise return all (implicit ALL)
                if args.is_empty() || args.iter().any(|a| a.to_uppercase() == "ALL") {
                    matching_sequence_numbers.push(seq_num);
                } else {
                    // Basic implementation: check if search criteria match any part of email
                    let email_content = match storage.read_email(&stored_email.filename) {
                        Ok(content) => content,
                        Err(_) => continue,
                    };

                    let email = match crate::email::EmailMessage::parse(&email_content) {
                        Ok(parsed) => parsed,
                        Err(_) => continue,
                    };

                    let mut matches = false;
                    for criteria in args.iter() {
                        let criteria_upper = criteria.to_uppercase();

                        match criteria_upper.as_str() {
                            "ANSWERED" | "DELETED" | "FLAGGED" | "SEEN" | "DRAFT" | "RECENT" => {
                                // Flag-based searches - would need proper flag storage
                            }
                            "NEW" => {
                                // NEW = RECENT + not SEEN
                                if !stored_email.is_read {
                                    matches = true;
                                }
                            }
                            "OLD" => {
                                // OLD = not RECENT
                                if stored_email.is_read {
                                    matches = true;
                                }
                            }
                            "UNSEEN" => {
                                if !stored_email.is_read {
                                    matches = true;
                                }
                            }
                            _ => {
                                // Try to match against email content
                                let search_text = criteria.to_lowercase();
                                let email_lower = email.raw.to_lowercase();

                                if email_lower.contains(&search_text) {
                                    matches = true;
                                }
                            }
                        }

                        if matches {
                            break;
                        }
                    }

                    if matches {
                        matching_sequence_numbers.push(seq_num);
                    }
                }
            }

            // Format SEARCH response
            let search_results = matching_sequence_numbers.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" ");

            format!("* SEARCH {}\r\n{} OK SEARCH completed\r\n", search_results, tag)
        }
        "FETCH" => {
            if args.is_empty() {
                return format!("{} BAD Missing FETCH arguments\r\n", tag);
            }

            // Parse FETCH command: FETCH <seq-set> <data-item>
            let seq_set = args[0];
            let raw_data_items = if args.len() > 1 { &args[1..] } else { &[] };

            // Strip parentheses from data items (e.g., "(BODY[])" -> "BODY[]")
            let data_items: Vec<String> = raw_data_items.iter()
                .map(|item| item.trim_matches('(').trim_matches(')').trim().to_string())
                .collect();

            debug!("📥 FETCH request: sequence set={}, raw items={:?}, processed={:?}", seq_set, raw_data_items, data_items);

            // Get stored emails to determine actual message counts
            let stored_emails = match storage.list_emails() {
                Ok(emails) => emails,
                Err(e) => {
                    warn!("⚠️  Failed to list emails for FETCH: {}", e);
                    return format!("{} NO Failed to retrieve emails\r\n", tag);
                }
            };

            // Parse sequence set and fetch requested emails
            let mut response_lines = Vec::new();
            let total_messages = stored_emails.len();

            if total_messages == 0 {
                return format!("{} OK FETCH completed\r\n", tag);
            }

            // Parse the sequence set (supports single numbers and ranges like 1:3)
            let sequence_numbers = parse_sequence_set(seq_set, total_messages);
            let sequence_count = sequence_numbers.len();

            for seq_num in sequence_numbers {
                if seq_num == 0 || seq_num > total_messages as u32 {
                    response_lines.push(format!("{} BAD Invalid message sequence number {}\r\n", tag, seq_num));
                    continue;
                }

                // Convert sequence number to array index (1-based to 0-based)
                let email_index = (seq_num - 1) as usize;
                let stored_email = &stored_emails[email_index];

                // Read actual email content from storage
                let email_content = match storage.read_email(&stored_email.filename) {
                    Ok(content) => content.to_string(),
                    Err(e) => {
                        warn!("⚠️  Failed to read email {}: {}", stored_email.filename, e);
                        continue;
                    }
                };

                // Parse the email for structured data
                let email = match crate::email::EmailMessage::parse(&email_content) {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        warn!("⚠️  Failed to parse email {}: {}", stored_email.filename, e);
                        continue;
                    }
                };

                // Process each data item requested
                let mut fetch_data = Vec::new();
                for item in &data_items {
                    let item_upper = item.to_uppercase();

                    match item_upper.as_str() {
                        "BODY" | "BODY[]" => {
                            // Full email content
                            let size = email.raw.len();
                            fetch_data.push(format!("BODY[] {{{}}}\r\n{}", size, email.raw));
                        }
                        "BODY.PEEK[]" | "BODY.PEEK" => {
                            // Full email content without setting Seen flag
                            let size = email.raw.len();
                            fetch_data.push(format!("BODY.PEEK[] {{{}}}\r\n{}", size, email.raw));
                        }
                        "RFC822" => {
                            // Alias for BODY[]
                            let size = email.raw.len();
                            fetch_data.push(format!("RFC822 {{{}}}\r\n{}", size, email.raw));
                        }
                        "RFC822.HEADER" => {
                            // Just headers
                            let headers_end = email.raw.find("\r\n\r\n").unwrap_or(email.raw.len());
                            let headers = &email.raw[..headers_end];
                            let size = headers.len();
                            fetch_data.push(format!("RFC822.HEADER {{{}}}\r\n{}", size, headers));
                        }
                        "RFC822.SIZE" => {
                            fetch_data.push(format!("RFC822.SIZE {}", email.raw.len()));
                        }
                        "UID" => {
                            fetch_data.push(format!("UID {}", seq_num));
                        }
                        "FLAGS" => {
                            let flags = if stored_email.is_read {
                                "\\Seen"
                            } else {
                                ""
                            };
                            fetch_data.push(format!("FLAGS ({})", flags));
                        }
                        "INTERNALDATE" => {
                            let date_str = stored_email.modified_at.format("%d-%b-%Y %H:%M:%S %z");
                            fetch_data.push(format!("INTERNALDATE \"{}\"", date_str));
                        }
                        "ENVELOPE" => {
                            let envelope = format_envelope(&email);
                            fetch_data.push(format!("ENVELOPE ({})", envelope));
                        }
                        "BODYSTRUCTURE" => {
                            let body_struct = format_body_structure(&email);
                            fetch_data.push(format!("BODYSTRUCTURE ({})", body_struct));
                        }
                        "ALL" => {
                            // Macro for FLAGS, INTERNALDATE, RFC822.SIZE, ENVELOPE
                            let flags = if stored_email.is_read { "\\Seen" } else { "" };
                            let date_str = stored_email.modified_at.format("%d-%b-%Y %H:%M:%S %z");
                            let envelope = format_envelope(&email);
                            fetch_data.push(format!("FLAGS ({})", flags));
                            fetch_data.push(format!("INTERNALDATE \"{}\"", date_str));
                            fetch_data.push(format!("RFC822.SIZE {}", email.raw.len()));
                            fetch_data.push(format!("ENVELOPE ({})", envelope));
                        }
                        "FULL" => {
                            // Macro for FLAGS, INTERNALDATE, RFC822.SIZE, ENVELOPE, BODY
                            let flags = if stored_email.is_read { "\\Seen" } else { "" };
                            let date_str = stored_email.modified_at.format("%d-%b-%Y %H:%M:%S %z");
                            let envelope = format_envelope(&email);
                            let body_struct = format_body_structure(&email);
                            fetch_data.push(format!("FLAGS ({})", flags));
                            fetch_data.push(format!("INTERNALDATE \"{}\"", date_str));
                            fetch_data.push(format!("RFC822.SIZE {}", email.raw.len()));
                            fetch_data.push(format!("ENVELOPE ({})", envelope));
                            fetch_data.push(format!("BODYSTRUCTURE ({})", body_struct));
                        }
                        "FAST" => {
                            // Macro for FLAGS, INTERNALDATE, RFC822.SIZE
                            let flags = if stored_email.is_read { "\\Seen" } else { "" };
                            let date_str = stored_email.modified_at.format("%d-%b-%Y %H:%M:%S %z");
                            fetch_data.push(format!("FLAGS ({})", flags));
                            fetch_data.push(format!("INTERNALDATE \"{}\"", date_str));
                            fetch_data.push(format!("RFC822.SIZE {}", email.raw.len()));
                        }
                        _ => {
                            // Unknown data item - try to pass through
                            fetch_data.push(format!("{}", item));
                        }
                    }
                }

                // Build the FETCH response line
                let fetch_response = if fetch_data.is_empty() {
                    format!("{} FETCH ({})\r\n", seq_num, data_items.join(" "))
                } else {
                    format!("{} FETCH ({})\r\n", seq_num, fetch_data.join(" "))
                };

                response_lines.push(fetch_response);
            }

            // Combine all response lines with the completion tag
            let response = if response_lines.is_empty() {
                format!("{} OK FETCH completed\r\n", tag)
            } else {
                format!("{}{} OK FETCH completed\r\n", response_lines.join(""), tag)
            };

            debug!("📤 FETCH response prepared for {} messages", sequence_count);
            response
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

/// Parse IMAP sequence set into individual sequence numbers
fn parse_sequence_set(seq_set: &str, total_messages: usize) -> Vec<u32> {
    let mut sequence_numbers = Vec::new();

    // Split by commas for multiple ranges/numbers
    for part in seq_set.split(',') {
        let part = part.trim();

        if part.contains(':') {
            // Range like "1:3" or "3:1"
            let range_parts: Vec<&str> = part.split(':').collect();
            if range_parts.len() == 2 {
                let start: u32 = range_parts[0].parse().unwrap_or(1);
                let end: u32 = range_parts[1].parse().unwrap_or(total_messages as u32);

                if start <= end {
                    for seq in start..=end {
                        if seq <= total_messages as u32 {
                            sequence_numbers.push(seq);
                        }
                    }
                } else {
                    // Reverse range like "5:3"
                    for seq in (end..=start).rev() {
                        if seq <= total_messages as u32 {
                            sequence_numbers.push(seq);
                        }
                    }
                }
            }
        } else if part == "*" {
            // Asterisk means last message
            if total_messages > 0 {
                sequence_numbers.push(total_messages as u32);
            }
        } else {
            // Single number
            if let Ok(seq) = part.parse::<u32>() {
                if seq <= total_messages as u32 {
                    sequence_numbers.push(seq);
                }
            }
        }
    }

    sequence_numbers.sort();
    sequence_numbers.dedup();
    sequence_numbers
}

/// Format email envelope for IMAP response
fn format_envelope(email: &crate::email::EmailMessage) -> String {
    let from = email.headers.from.as_deref().unwrap_or("NIL");
    let subject = email.headers.subject.as_deref().unwrap_or("NIL");
    let date = email.headers.date
        .map(|d| format!("\"{}\"", d.format("%d-%b-%Y %H:%M:%S %z")))
        .unwrap_or("NIL".to_string());
    let message_id = email.headers.message_id.as_deref().unwrap_or("NIL");

    format!("{} {} {} {} {} {} {}",
        date, message_id, from, format_address_list(&email.headers.to),
        format_address_list(&email.headers.cc), "NIL", subject)
}

/// Format address list for IMAP response
fn format_address_list(addresses: &[String]) -> String {
    if addresses.is_empty() {
        return "NIL".to_string();
    }

    let formatted: Vec<String> = addresses.iter().map(|addr| {
        format!("((NIL NIL \"{}\" NIL))", addr.replace("\"", "\\\""))
    }).collect();

    formatted.join(" ")
}

/// Format body structure for IMAP response
fn format_body_structure(email: &crate::email::EmailMessage) -> String {
    let content_type = &email.body.content_type;
    let content_type_parts: Vec<&str> = content_type.split('/').collect();
    let main_type = content_type_parts.first().unwrap_or(&"text");
    let sub_type = content_type_parts.get(1).unwrap_or(&"plain");
    let charset = if content_type.contains("charset=") {
        let charset_part = content_type.split("charset=").nth(1).unwrap_or("utf-8");
        let charset = charset_part.split(';').next().unwrap_or("utf-8").trim();
        format!("\"{}\"", charset)
    } else {
        "\"utf-8\"".to_string()
    };

    format!("\"{}\" {} NIL NIL NIL {} {} NIL NIL NIL",
        main_type, sub_type, charset, email.raw.len())
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