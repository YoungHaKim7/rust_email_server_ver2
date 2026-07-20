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
