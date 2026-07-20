use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    pub limits: LimitSettings,
    pub logging: LoggingSettings,
    pub storage: StorageSettings,
    pub tls: TlsSettings,
    pub auth: AuthSettings,
    pub imap: ImapSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub hostname: String,
    pub max_connections: usize,
    pub connection_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitSettings {
    pub max_message_size: usize,
    pub max_recipients: usize,
    pub max_line_length: usize,
    pub rate_limit_per_minute: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingSettings {
    pub level: String,
    pub log_connections: bool,
    pub log_commands: bool,
    pub log_errors: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSettings {
    pub enabled: bool,
    pub maildir_path: String,
    pub cleanup_interval_hours: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsSettings {
    pub enabled: bool,
    pub certificate_path: String,
    pub private_key_path: String,
    pub mode: String, // "implicit", "starttls", or "both"
    pub require_tls: bool,
    pub implicit_port: u16,
    pub starttls_port: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TlsMode {
    Implicit,  // SMTPS on port 465 - TLS from start
    StartTls,  // STARTTLS on port 587 - upgrade via command
    Both,      // Support both on separate ports
}

impl TlsMode {
    pub fn from_str(mode: &str) -> Self {
        match mode.to_lowercase().as_str() {
            "implicit" => TlsMode::Implicit,
            "starttls" => TlsMode::StartTls,
            "both" => TlsMode::Both,
            _ => TlsMode::StartTls, // default for compatibility
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            TlsMode::Implicit => "implicit",
            TlsMode::StartTls => "starttls",
            TlsMode::Both => "both",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSettings {
    pub enabled: bool,
    pub require_auth: bool,
    pub allow_plaintext: bool, // Allow AUTH PLAIN without TLS
    pub mechanisms: Vec<String>, // "PLAIN", "LOGIN", etc.
    pub max_failed_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImapSettings {
    pub enabled: bool,
    pub port: u16,
    pub hostname: String,
    pub max_connections: usize,
    pub idle_timeout_secs: u64,
    pub enable_idle: bool, // IMAP IDLE command support
    pub enable_utf8: bool, // UTF-8 accept support
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8025,
                hostname: "localhost".to_string(),
                max_connections: 100,
                connection_timeout_secs: 300,
            },
            limits: LimitSettings {
                max_message_size: 10_000_000, // 10MB
                max_recipients: 100,
                max_line_length: 1000,
                rate_limit_per_minute: 100,
            },
            logging: LoggingSettings {
                level: "info".to_string(),
                log_connections: true,
                log_commands: true,
                log_errors: true,
            },
            storage: StorageSettings {
                enabled: true,
                maildir_path: "mail_storage".to_string(),
                cleanup_interval_hours: 24,
            },
            tls: TlsSettings {
                enabled: true, // Enable TLS for testing
                certificate_path: "certs/server.crt".to_string(),
                private_key_path: "certs/server.key".to_string(),
                mode: "starttls".to_string(),
                require_tls: false,
                implicit_port: 465,
                starttls_port: 587,
            },
            auth: AuthSettings {
                enabled: true,
                require_auth: false, // Don't require auth by default for testing
                allow_plaintext: true, // Allow PLAIN auth without TLS for development
                mechanisms: vec!["PLAIN".to_string(), "LOGIN".to_string()],
                max_failed_attempts: 3,
            },
            imap: ImapSettings {
                enabled: true,
                port: 1143, // Default IMAP port (143 requires root, so using 1143)
                hostname: "localhost".to_string(),
                max_connections: 50,
                idle_timeout_secs: 600, // 10 minutes
                enable_idle: true,
                enable_utf8: true,
            },
        }
    }
}

impl ServerConfig {
    pub fn load() -> anyhow::Result<Self> {
        // Try to load from file, fall back to defaults
        let config = Self::default();

        // For now, just return defaults
        // TODO: Implement file loading in future enhancement
        Ok(config)
    }

    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.server.host, self.server.port)
    }

    pub fn connection_timeout(&self) -> Duration {
        Duration::from_secs(self.server.connection_timeout_secs)
    }

    pub fn storage_path(&self) -> Option<String> {
        if self.storage.enabled {
            Some(self.storage.maildir_path.clone())
        } else {
            None
        }
    }

    pub fn is_storage_enabled(&self) -> bool {
        self.storage.enabled
    }

    pub fn is_tls_enabled(&self) -> bool {
        self.tls.enabled
    }

    pub fn get_tls_mode(&self) -> TlsMode {
        TlsMode::from_str(&self.tls.mode)
    }

    pub fn tls_bind_address(&self) -> String {
        let port = match self.get_tls_mode() {
            TlsMode::Implicit => self.tls.implicit_port,
            TlsMode::StartTls => self.tls.starttls_port,
            TlsMode::Both => self.tls.starttls_port,
        };
        format!("{}:{}", self.server.host, port)
    }

    pub fn tls_implicit_address(&self) -> String {
        format!("{}:{}", self.server.host, self.tls.implicit_port)
    }

    pub fn tls_starttls_address(&self) -> String {
        format!("{}:{}", self.server.host, self.tls.starttls_port)
    }

    pub fn is_tls_required(&self) -> bool {
        self.tls.require_tls
    }

    pub fn is_auth_enabled(&self) -> bool {
        self.auth.enabled
    }

    pub fn is_auth_required(&self) -> bool {
        self.auth.require_auth
    }

    pub fn get_auth_mechanisms(&self) -> &[String] {
        &self.auth.mechanisms
    }

    pub fn allows_plaintext_auth(&self) -> bool {
        self.auth.allow_plaintext
    }

    // IMAP-specific methods
    pub fn is_imap_enabled(&self) -> bool {
        self.imap.enabled
    }

    pub fn imap_bind_address(&self) -> String {
        format!("{}:{}", self.server.host, self.imap.port)
    }

    pub fn imap_hostname(&self) -> &str {
        &self.imap.hostname
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ServerConfig::default();
        assert_eq!(config.server.port, 8025);
        assert_eq!(config.server.max_connections, 100);
        assert_eq!(config.limits.max_message_size, 10_000_000);
    }

    #[test]
    fn test_bind_address() {
        let config = ServerConfig::default();
        assert_eq!(config.bind_address(), "127.0.0.1:8025");
    }
}