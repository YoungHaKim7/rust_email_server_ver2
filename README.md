# rust_email_server_ver2

# test
Looking at your telnet output, you're mixing up IMAP with SMTP commands. IMAP has a different protocol structure.

The proper way to test IMAP via telnet:

```bash
telnet localhost 1143
```

Then use these IMAP commands (note the tag prefix like `A1`, `A2`, etc.):

```imap
# Authenticate (replace with your credentials)
A1 LOGIN testuser testpass

# List all mailboxes
A2 LIST "" *

# Select INBOX
A3 SELECT INBOX

# List messages in INBOX
A4 SEARCH ALL

# Fetch a message (e.g., message 1)
A5 FETCH 1 (BODY[])

# Logout
A6 LOGOUT
```

Key differences from what you tried:
- **IMAP requires tags** - each command must start with a tag like `A1`, `A2` (user-defined)
- **LOGIN syntax** - `A1 LOGIN username password` (space-separated, not testuser/testpass on one line)
- **No EHLO** - that's an SMTP command
- **No AUTH PLAIN** in raw form - IMAP uses LOGIN or AUTHENTICATE with SASL mechanisms

If you want to use **PLAIN authentication** (SASL), the syntax is:
```imap
A1 AUTHENTICATE PLAIN
<base64 encoded \0username\0password>
```

Where the base64 would be: `\0testuser\0testpass` → `AHRlc3R1c2VyAHRlc3RwYXNz`

Would you like me to help you test with proper IMAP commands, or would you prefer using a dedicated IMAP client like `openssl` or a mail client for testing?

# 테스트2

- Yes, you can use Telnet! I can see from your documentation that your email server supports both SMTP (for sending/saving emails) on port 8025 and IMAP (for retrieving saved emails) on port 1143.

Here's how to test the complete flow - **save an email and check if it's saved**:

## Step 1: Start your server

```bash
cd /home/gy/my_projects/Rust_Lang/rust_email_server_ver2
pkill -f rust_email_server 2>/dev/null
cargo run
```

## Step 2: Send a test email using SMTP (to save it)

In a new terminal, use telnet or netcat to send an email:

```bash
# Using netcat (simpler)
echo -e "EHLO test.com\r\nMAIL FROM:<alice@test.com>\r\nRCPT TO:<testuser@test.com>\r\nDATA\r\nSubject: Test Email\r\nFrom: alice@test.com\r\nTo: testuser@test.com\r\n\r\nThis is a test email to verify mail storage works!\r\n.\r\nQUIT\r\n" | nc -q 5 localhost 8025
```

Or using telnet interactively:
```bash
telnet localhost 8025
# Then type these commands:
EHLO test.com
MAIL FROM:<alice@test.com>
RCPT TO:<testuser@test.com>
DATA
Subject: Test Email
From: alice@test.com
To: testuser@test.com

This is the email body.
.
QUIT
```

## Step 3: Check if the email was saved using IMAP

```bash
telnet localhost 1143
# Then use IMAP commands:
A1 LOGIN testuser testpass
A2 SELECT INBOX
A3 SEARCH ALL
A4 FETCH 1 (BODY[])
A5 LOGOUT
```

Or using netcat:
```bash
echo -e "A1 LOGIN testuser testpass\r\nA2 SELECT INBOX\r\nA3 SEARCH ALL\r\nA4 FETCH 1 (BODY[])\r\nA5 LOGOUT\r\n" | nc -q 2 localhost 1143
```

## Quick verification: Check maildir directly

You can also check the maildir directly to see if the email was saved:

```bash
ls -la mail_storage/new/    # New/unread emails
ls -la mail_storage/cur/    # Read emails
cat mail_storage/new/*     # View email content
```

Would you like me to help you run this test now, or would you prefer to test it yourself first?

