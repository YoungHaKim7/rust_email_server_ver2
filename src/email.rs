use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tracing::debug;

/// Represents a complete email message
#[derive(Debug, Clone)]
pub struct EmailMessage {
    pub headers: EmailHeaders,
    pub body: EmailBody,
    pub raw: String,
    pub parsed_at: DateTime<Utc>,
}

/// Email headers according to RFC 5322
#[derive(Debug, Clone)]
pub struct EmailHeaders {
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: Option<String>,
    pub date: Option<DateTime<Utc>>,
    pub message_id: Option<String>,
    pub content_type: Option<String>,
    pub custom_headers: HashMap<String, String>,
}

/// Email body content
#[derive(Debug, Clone)]
pub struct EmailBody {
    pub content_type: String,
    pub content: String,
    pub is_multipart: bool,
}

impl EmailMessage {
    /// Parse raw email content into structured EmailMessage
    pub fn parse(raw_email: &str) -> Result<Self> {
        debug!("📧 Parsing email message ({} bytes)", raw_email.len());

        // Split headers and body at the first blank line
        let parts: Vec<&str> = raw_email.splitn(2, "\r\n\r\n").collect();
        if parts.len() < 2 {
            return Err(anyhow!("Invalid email format - no headers/body separator found"));
        }

        let headers_section = parts[0];
        let body_section = parts[1];

        let headers = Self::parse_headers(headers_section)?;
        let body = Self::parse_body(body_section, &headers.content_type);

        debug!("✅ Email parsed successfully - From: {:?}, Subject: {:?}",
               headers.from, headers.subject);

        Ok(Self {
            headers,
            body,
            raw: raw_email.to_string(),
            parsed_at: Utc::now(),
        })
    }

    /// Parse email headers
    fn parse_headers(headers_section: &str) -> Result<EmailHeaders> {
        let mut headers = EmailHeaders {
            from: None,
            to: Vec::new(),
            cc: Vec::new(),
            subject: None,
            date: None,
            message_id: None,
            content_type: None,
            custom_headers: HashMap::new(),
        };

        for line in headers_section.lines() {
            if let Some(colon_pos) = line.find(':') {
                let name = line[..colon_pos].trim();
                let value = line[colon_pos + 1..].trim();

                match name.to_lowercase().as_str() {
                    "from" => headers.from = Some(value.to_string()),
                    "to" => headers.to = Self::parse_email_addresses(value),
                    "cc" => headers.cc = Self::parse_email_addresses(value),
                    "subject" => headers.subject = Some(value.to_string()),
                    "date" => headers.date = Self::parse_date(value),
                    "message-id" => headers.message_id = Some(value.to_string()),
                    "content-type" => headers.content_type = Some(value.to_string()),
                    _ => {
                        headers.custom_headers.insert(name.to_string(), value.to_string());
                    }
                }
            }
        }

        Ok(headers)
    }

    /// Parse email body
    fn parse_body(body_section: &str, content_type: &Option<String>) -> EmailBody {
        let content_type = content_type
            .as_ref()
            .and_then(|ct| ct.split(';').next())
            .unwrap_or("text/plain")
            .to_string();

        let is_multipart = content_type.starts_with("multipart/");

        EmailBody {
            content_type,
            content: body_section.to_string(),
            is_multipart,
        }
    }

    /// Parse email addresses from header value
    fn parse_email_addresses(address_string: &str) -> Vec<String> {
        let mut addresses = Vec::new();

        // Simple email address extraction
        let parts: Vec<&str> = address_string.split(',').collect();

        for part in parts {
            let cleaned = part.trim();
            if let Some(email) = cleaned.strip_prefix('<') {
                if let Some(email) = email.strip_suffix('>') {
                    addresses.push(email.trim().to_string());
                }
            } else if cleaned.contains('@') {
                addresses.push(cleaned.to_string());
            }
        }

        addresses
    }

    /// Parse date string to DateTime
    fn parse_date(date_str: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc2822(date_str).ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Get email size in bytes
    pub fn size(&self) -> usize {
        self.raw.len()
    }

    /// Get summary of email
    pub fn summary(&self) -> String {
        format!(
            "From: {:?} | To: {:?} | Subject: {:?} | Size: {} bytes | Type: {}",
            self.headers.from,
            self.headers.to,
            self.headers.subject,
            self.size(),
            self.body.content_type
        )
    }

    /// Get body preview
    pub fn body_preview(&self, max_chars: usize) -> String {
        let preview = self.body.content.chars().take(max_chars).collect::<String>();
        if self.body.content.chars().count() > max_chars {
            preview + "..."
        } else {
            preview
        }
    }
}

/// Create email message from SMTP session data
pub fn create_email_from_session(
    from: &Option<String>,
    to: &[String],
    data_lines: &[String],
) -> Result<EmailMessage> {
    // Reconstruct the email from SMTP session data with proper CRLF line endings
    let raw_email = data_lines.join("\r\n");

    // Validate that we have required content
    if raw_email.is_empty() {
        return Err(anyhow!("Empty email content"));
    }

    let mut email = EmailMessage::parse(&raw_email)?;

    // Override with SMTP envelope information if parsing failed
    if email.headers.from.is_none() {
        email.headers.from = from.clone();
    }

    if email.headers.to.is_empty() {
        email.headers.to = to.to_vec();
    }

    Ok(email)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_email() {
        let raw_email = "From: sender@example.com\r\n\
                         To: recipient@example.com\r\n\
                         Subject: Test Email\r\n\
                         \r\n\
                         This is a test email body.";

        let email = EmailMessage::parse(raw_email).unwrap();
        assert_eq!(email.headers.from, Some("sender@example.com".to_string()));
        assert_eq!(email.headers.to, vec!["recipient@example.com"]);
        assert_eq!(email.headers.subject, Some("Test Email".to_string()));
        assert_eq!(email.body.content, "This is a test email body.");
    }

    #[test]
    fn test_parse_multiple_recipients() {
        let raw_email = "From: sender@example.com\r\n\
                         To: recipient1@example.com, recipient2@example.com\r\n\
                         Subject: Test\r\n\
                         \r\n\
                         Body text";

        let email = EmailMessage::parse(raw_email).unwrap();
        assert_eq!(email.headers.to.len(), 2);
    }

    #[test]
    fn test_parse_email_addresses() {
        let addresses = EmailMessage::parse_email_addresses("user1@example.com, User Two <user2@example.com>");
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&"user1@example.com".to_string()));
        assert!(addresses.contains(&"user2@example.com".to_string()));
    }

    #[test]
    fn test_body_preview() {
        let raw_email = "From: sender@example.com\r\n\
                         To: recipient@example.com\r\n\
                         Subject: Test\r\n\
                         \r\n\
                         This is a long body that should be truncated.";

        let email = EmailMessage::parse(raw_email).unwrap();
        let preview = email.body_preview(20);
        assert!(preview.len() <= 23); // 20 chars + "..."
    }

    #[test]
    fn test_size() {
        let raw_email = "From: sender@example.com\r\n\
                         To: recipient@example.com\r\n\
                         Subject: Test\r\n\
                         \r\n\
                         Body";

        let email = EmailMessage::parse(raw_email).unwrap();
        assert!(email.size() > 0);
    }
}