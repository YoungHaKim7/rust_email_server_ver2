use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, info, warn};
use base64::{Engine as _, engine::general_purpose};

/// User credentials for authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCredentials {
    pub username: String,
    pub password: String, // In production, this should be hashed!
}

/// Authentication result
#[derive(Debug, Clone, PartialEq)]
pub enum AuthResult {
    Success,
    InvalidCredentials,
    InvalidMechanism,
    TooManyAttempts,
    AuthNotRequired,
    TlsRequired,
}

/// Authentication state for a session
#[derive(Debug, Clone, PartialEq)]
pub enum AuthState {
    NotAuthenticated,
    InProgress(String), // mechanism name
    Authenticated(String), // username
    Failed(u32), // failed attempt count
}

/// SASL authentication mechanisms
#[derive(Debug, Clone, PartialEq)]
pub enum SaslMechanism {
    Plain,
    Login,
}

impl SaslMechanism {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "PLAIN" => Some(SaslMechanism::Plain),
            "LOGIN" => Some(SaslMechanism::Login),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            SaslMechanism::Plain => "PLAIN",
            SaslMechanism::Login => "LOGIN",
        }
    }
}

/// User database for authentication
pub struct UserDatabase {
    users: Arc<RwLock<HashMap<String, String>>>, // username -> password (hashed in production)
}

impl UserDatabase {
    /// Create a new user database
    pub fn new() -> Self {
        let mut users = HashMap::new();

        // Add some default users for testing
        users.insert("testuser".to_string(), "testpass".to_string());
        users.insert("admin".to_string(), "admin".to_string());

        debug!("🔐 Created user database with {} users", users.len());

        Self {
            users: Arc::new(RwLock::new(users)),
        }
    }

    /// Add a user to the database
    pub fn add_user(&self, username: String, password: String) -> Result<()> {
        let mut users = self.users.write()
            .map_err(|_| anyhow::anyhow!("Failed to acquire write lock on user database"))?;

        users.insert(username, password);
        info!("✅ User added to database");
        Ok(())
    }

    /// Authenticate a user with credentials
    pub fn authenticate(&self, username: &str, password: &str) -> bool {
        let users = self.users.read()
            .map_err(|_| anyhow::anyhow!("Failed to acquire read lock on user database")).ok();

        if let Some(users) = users {
            if let Some(stored_password) = users.get(username) {
                let valid = stored_password == password;
                if valid {
                    debug!("✅ User '{}' authenticated successfully", username);
                } else {
                    debug!("❌ Authentication failed for user '{}'", username);
                }
                return valid;
            }
        }

        debug!("❌ User '{}' not found in database", username);
        false
    }

    /// Check if a user exists
    pub fn user_exists(&self, username: &str) -> bool {
        let users = self.users.read()
            .map_err(|_| anyhow::anyhow!("Failed to acquire read lock on user database")).ok();

        users.map(|users| users.contains_key(username)).unwrap_or(false)
    }

    /// Get list of users (for admin purposes)
    pub fn list_users(&self) -> Vec<String> {
        let users = self.users.read()
            .map_err(|_| anyhow::anyhow!("Failed to acquire read lock on user database")).ok();

        users.map(|users| users.keys().cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for UserDatabase {
    fn default() -> Self {
        Self::new()
    }
}

/// Authentication manager for handling SMTP AUTH
pub struct AuthManager {
    users: Arc<UserDatabase>,
    failed_attempts: Arc<RwLock<HashMap<String, u32>>>, // IP -> attempt count
}

impl AuthManager {
    /// Create a new authentication manager
    pub fn new(users: Arc<UserDatabase>) -> Self {
        info!("🔐 Authentication manager created");

        Self {
            users,
            failed_attempts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Authenticate credentials
    pub fn authenticate(&self, username: &str, password: &str, client_ip: &str) -> AuthResult {
        // Check if client has too many failed attempts
        if let Some(attempts) = self.get_failed_attempts(client_ip) {
            if attempts > 3 {
                warn!("⚠️  Client {} has too many failed auth attempts: {}", client_ip, attempts);
                return AuthResult::TooManyAttempts;
            }
        }

        let valid = self.users.authenticate(username, password);

        if valid {
            // Reset failed attempts on success
            self.reset_failed_attempts(client_ip);
            AuthResult::Success
        } else {
            // Increment failed attempts
            self.increment_failed_attempts(client_ip);
            AuthResult::InvalidCredentials
        }
    }

    /// Get failed attempts for a client
    fn get_failed_attempts(&self, client_ip: &str) -> Option<u32> {
        let attempts = self.failed_attempts.read()
            .map_err(|_| anyhow::anyhow!("Failed to read failed attempts")).ok()?;
        attempts.get(client_ip).copied()
    }

    /// Increment failed attempts
    fn increment_failed_attempts(&self, client_ip: &str) {
        let attempts = self.failed_attempts.write()
            .map_err(|_| anyhow::anyhow!("Failed to write failed attempts")).ok();

        if let Some(mut attempts_guard) = attempts {
            let count = attempts_guard.entry(client_ip.to_string()).or_insert(0);
            *count += 1;
            warn!("⚠️  Failed auth attempt {} for client {}", count, client_ip);
        }
    }

    /// Reset failed attempts
    fn reset_failed_attempts(&self, client_ip: &str) {
        let attempts = self.failed_attempts.write()
            .map_err(|_| anyhow::anyhow!("Failed to write failed attempts")).ok();

        if let Some(mut attempts_guard) = attempts {
            attempts_guard.remove(client_ip);
        }
    }

    /// Validate if authentication mechanism is supported
    pub fn validate_mechanism(&self, mechanism: &str) -> AuthResult {
        match SaslMechanism::from_str(mechanism) {
            Some(_) => AuthResult::Success,
            None => {
                warn!("❌ Unsupported authentication mechanism: {}", mechanism);
                AuthResult::InvalidMechanism
            }
        }
    }

    /// Parse AUTH PLAIN credentials
    pub fn parse_plain_credentials(&self, credentials: &str) -> Result<(String, String)> {
        // AUTH PLAIN format: base64(\0username\0password) or base64(\0authorization_id\0username\0password)
        let decoded = general_purpose::STANDARD
            .decode(credentials)
            .map_err(|_| anyhow::anyhow!("Invalid base64 encoding in AUTH PLAIN"))?;

        let credentials_str = String::from_utf8(decoded)
            .map_err(|_| anyhow::anyhow!("Invalid UTF-8 in AUTH PLAIN credentials"))?;

        let parts: Vec<&str> = credentials_str.split('\0').collect();

        if parts.len() >= 3 {
            // Handle both formats:
            // - \0username\0password (parts[0]="", parts[1]=username, parts[2]=password)
            // - \0auth_id\0username\0password (parts[0]="", parts[1]=auth_id, parts[2]=username, parts[3]=password)
            let (username, password) = if parts.len() == 3 {
                (parts[1].to_string(), parts[2].to_string())
            } else {
                (parts[2].to_string(), parts[3].to_string())
            };

            if username.is_empty() || password.is_empty() {
                return Err(anyhow::anyhow!("Empty username or password in AUTH PLAIN"));
            }

            debug!("🔐 AUTH PLAIN: Attempting authentication for user '{}'", username);
            Ok((username, password))
        } else {
            Err(anyhow::anyhow!("Invalid AUTH PLAIN format"))
        }
    }

    /// Parse AUTH LOGIN credentials
    pub fn parse_login_credentials(&self, credentials: &str) -> Result<String> {
        // AUTH LOGIN sends username and password in base64 separately
        let decoded = general_purpose::STANDARD
            .decode(credentials)
            .map_err(|_| anyhow::anyhow!("Invalid base64 encoding in AUTH LOGIN"))?;

        let credential_str = String::from_utf8(decoded)
            .map_err(|_| anyhow::anyhow!("Invalid UTF-8 in AUTH LOGIN credentials"))?;

        if credential_str.is_empty() {
            return Err(anyhow::anyhow!("Empty credential in AUTH LOGIN"));
        }

        debug!("🔐 AUTH LOGIN: Received credential ({} bytes)", credential_str.len());
        Ok(credential_str)
    }

    /// Encode credentials for AUTH LOGIN challenge response
    pub fn encode_login_response(&self, response: &str) -> String {
        general_purpose::STANDARD.encode(response)
    }
}

impl Default for AuthManager {
    fn default() -> Self {
        Self::new(Arc::new(UserDatabase::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_database() {
        let db = UserDatabase::new();

        assert!(db.user_exists("testuser"));
        assert!(db.authenticate("testuser", "testpass"));
        assert!(!db.authenticate("testuser", "wrongpass"));
        assert!(!db.user_exists("nonexistent"));
    }

    #[test]
    fn test_sasl_mechanisms() {
        assert_eq!(SaslMechanism::from_str("PLAIN"), Some(SaslMechanism::Plain));
        assert_eq!(SaslMechanism::from_str("LOGIN"), Some(SaslMechanism::Login));
        assert_eq!(SaslMechanism::from_str("INVALID"), None);

        assert_eq!(SaslMechanism::Plain.as_str(), "PLAIN");
        assert_eq!(SaslMechanism::Login.as_str(), "LOGIN");
    }

    #[test]
    fn test_auth_manager() {
        let users = Arc::new(UserDatabase::new());
        let auth_manager = AuthManager::new(users);

        assert_eq!(auth_manager.authenticate("testuser", "testpass", "127.0.0.1"), AuthResult::Success);
        assert_eq!(auth_manager.authenticate("testuser", "wrong", "127.0.0.1"), AuthResult::InvalidCredentials);
    }

    #[test]
    fn test_parse_plain_credentials() {
        let users = Arc::new(UserDatabase::new());
        let auth_manager = AuthManager::new(users);

        // Test AUTH PLAIN format: \0username\0password
        let credentials = "\0testuser\0testpass";
        let (username, password) = auth_manager.parse_plain_credentials(credentials).unwrap();

        assert_eq!(username, "testuser");
        assert_eq!(password, "testpass");
    }
}