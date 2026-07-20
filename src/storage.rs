use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};

/// Maildir storage for email messages
pub struct MaildirStorage {
    pub base_path: PathBuf,
}

impl MaildirStorage {
    /// Create a dummy storage that doesn't persist emails (for when storage is disabled)
    pub fn dummy() -> Self {
        Self {
            base_path: std::path::PathBuf::from("/dev/null"),
        }
    }

    /// Create a new Maildir storage backend
    pub fn new(base_path: PathBuf) -> Result<Self> {
        // Create the Maildir directory structure
        fs::create_dir_all(&base_path).context("Failed to create maildir base directory")?;

        info!("📁 Maildir storage initialized at: {}", base_path.display());

        let storage = Self { base_path };
        storage.create_maildir_structure()?;

        Ok(storage)
    }

    /// Create the Maildir directory structure: {cur, new, tmp}
    fn create_maildir_structure(&self) -> Result<()> {
        for subdir in ["cur", "new", "tmp"] {
            let path = self.base_path.join(subdir);
            if !path.exists() {
                fs::create_dir(&path).context(format!("Failed to create maildir/{} directory", subdir))?;
                debug!("✅ Created maildir/{} directory", subdir);
            }
        }
        Ok(())
    }

    /// Save an email to Maildir storage (directly to cur for piling up test emails)
    pub fn save_email(&self, email: &crate::email::EmailMessage) -> Result<String> {
        // Generate unique filename
        let filename = Self::generate_unique_filename();
        let tmp_path = self.base_path.join("tmp").join(&filename);
        let cur_path = self.base_path.join("cur").join(&filename);

        // Write to tmp directory first (atomic operation)
        fs::write(&tmp_path, email.raw.as_bytes())
            .context("Failed to write email to tmp directory")?;

        debug!("💾 Email written to tmp: {}", filename);

        // Move from tmp to cur (piling up emails in cur directory)
        fs::rename(&tmp_path, &cur_path)
            .context("Failed to move email from tmp to cur")?;

        info!("📧 Email saved to maildir/cur/{}", filename);

        Ok(filename)
    }

    /// Save an email to Maildir storage (specifically to new folder)
    pub fn save_email_to_new(&self, email: &crate::email::EmailMessage) -> Result<String> {
        // Generate unique filename
        let filename = Self::generate_unique_filename();
        let tmp_path = self.base_path.join("tmp").join(&filename);
        let new_path = self.base_path.join("new").join(&filename);

        // Write to tmp directory first (atomic operation)
        fs::write(&tmp_path, email.raw.as_bytes())
            .context("Failed to write email to tmp directory")?;

        debug!("💾 Email written to tmp: {}", filename);

        // Move from tmp to new (standard Maildir behavior for new emails)
        fs::rename(&tmp_path, &new_path)
            .context("Failed to move email from tmp to new")?;

        info!("📧 Email saved to maildir/new/{}", filename);

        Ok(filename)
    }

    /// Move email from new/ to cur/ (mark as read/processed)
    pub fn mark_as_read(&self, filename: &str) -> Result<()> {
        let new_path = self.base_path.join("new").join(filename);
        let cur_path = self.base_path.join("cur").join(filename);

        if !new_path.exists() {
            return Err(anyhow::anyhow!("Email not found in new/ directory: {}", filename));
        }

        fs::rename(&new_path, &cur_path)
            .context("Failed to move email from new to cur")?;

        debug!("📭 Email marked as read: {}", filename);
        Ok(())
    }

    /// Delete an email from storage
    pub fn delete_email(&self, filename: &str, from_cur: bool) -> Result<()> {
        let source_dir = if from_cur { "cur" } else { "new" };
        let email_path = self.base_path.join(source_dir).join(filename);

        if !email_path.exists() {
            return Err(anyhow::anyhow!("Email not found: {}/{}", source_dir, filename));
        }

        fs::remove_file(&email_path)
            .context("Failed to delete email")?;

        info!("🗑️  Email deleted: {}/{}", source_dir, filename);
        Ok(())
    }

    /// List all emails in storage
    pub fn list_emails(&self) -> Result<Vec<StoredEmail>> {
        let mut emails = Vec::new();

        // List emails from both new/ and cur/ directories
        for (dir_name, is_read) in [("new", false), ("cur", true)] {
            let dir_path = self.base_path.join(dir_name);

            if let Ok(entries) = fs::read_dir(&dir_path) {
                for entry in entries {
                    let entry = entry.context("Failed to read directory entry")?;
                    let filename = entry.file_name().to_string_lossy().to_string();

                    // Get file metadata
                    let metadata = entry.metadata().context("Failed to get file metadata")?;
                    let modified = metadata.modified().context("Failed to get modification time")?;
                    let size = metadata.len();

                    emails.push(StoredEmail {
                        filename: filename.clone(),
                        is_read,
                        size,
                        modified_at: DateTime::from(modified),
                        directory: dir_name.to_string(),
                    });
                }
            }
        }

        // Sort by modification time, newest first
        emails.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));

        Ok(emails)
    }

    /// Read an email from storage
    pub fn read_email(&self, filename: &str) -> Result<String> {
        // Try cur/ first, then new/
        let cur_path = self.base_path.join("cur").join(filename);
        let new_path = self.base_path.join("new").join(filename);

        let email_path = if cur_path.exists() {
            cur_path
        } else {
            new_path
        };

        if !email_path.exists() {
            return Err(anyhow::anyhow!("Email not found: {}", filename));
        }

        let content = fs::read_to_string(&email_path)
            .context("Failed to read email file")?;

        debug!("📖 Read email: {}", filename);
        Ok(content)
    }

    /// Get storage statistics
    pub fn get_stats(&self) -> Result<StorageStats> {
        let mut total_emails = 0usize;
        let mut unread_emails = 0usize;
        let mut total_size = 0u64;

        for (dir_name, is_read_dir) in [("new", false), ("cur", true)] {
            let dir_path = self.base_path.join(dir_name);

            if let Ok(entries) = fs::read_dir(&dir_path) {
                for entry in entries {
                    let entry = entry.context("Failed to read directory entry")?;
                    let metadata = entry.metadata().context("Failed to get file metadata")?;

                    total_emails += 1;
                    if !is_read_dir {
                        unread_emails += 1;
                    }
                    total_size += metadata.len();
                }
            }
        }

        Ok(StorageStats {
            total_emails,
            unread_emails,
            total_size,
            storage_path: self.base_path.clone(),
        })
    }

    /// Generate unique filename for email
    fn generate_unique_filename() -> String {
        let timestamp = chrono::Utc::now().timestamp();
        let random: u32 = rand::random();

        // Get hostname and clean it for use in filename
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().replace('.', "_").to_string())
            .unwrap_or_else(|_| "localhost".to_string());

        format!("{}.{:x}.{}", timestamp, random, hostname)
    }

    /// Clean up old/invalid emails from tmp directory
    pub fn cleanup_tmp(&self) -> Result<usize> {
        let tmp_path = self.base_path.join("tmp");
        let mut cleaned_count = 0usize;

        if let Ok(entries) = fs::read_dir(&tmp_path) {
            for entry in entries {
                let entry = entry.context("Failed to read tmp directory entry")?;

                // Remove files older than 1 hour
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        let age = chrono::Utc::now() - DateTime::from(modified);
                        if age.num_hours() > 1 {
                            fs::remove_file(entry.path())
                                .context("Failed to cleanup old tmp file")?;
                            cleaned_count += 1;
                        }
                    }
                }
            }
        }

        if cleaned_count > 0 {
            info!("🧹 Cleaned up {} old files from tmp/", cleaned_count);
        }

        Ok(cleaned_count)
    }
}

/// Email metadata for stored emails
#[derive(Debug, Clone)]
pub struct StoredEmail {
    pub filename: String,
    pub is_read: bool,
    pub size: u64,
    pub modified_at: DateTime<Utc>,
    pub directory: String,
}

/// Storage statistics
#[derive(Debug, Clone)]
pub struct StorageStats {
    pub total_emails: usize,
    pub unread_emails: usize,
    pub total_size: u64,
    pub storage_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_maildir_storage_creation() {
        let temp_dir = env::temp_dir().join("test_maildir");
        let _storage = MaildirStorage::new(temp_dir.clone()).unwrap();

        assert!(temp_dir.join("new").exists());
        assert!(temp_dir.join("cur").exists());
        assert!(temp_dir.join("tmp").exists());

        // Cleanup
        fs::remove_dir_all(temp_dir).ok();
    }

    #[test]
    fn test_email_filename_generation() {
        let filename = MaildirStorage::generate_unique_filename();

        // Filename should have format: timestamp.random.hostname
        assert!(filename.contains('.'));
        assert!(filename.split('.').count() >= 3);

        println!("Generated filename: {}", filename);
    }
}