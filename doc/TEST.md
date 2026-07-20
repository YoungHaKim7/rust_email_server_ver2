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

## ✅ Summary: Email Server Storage Configuration Complete!

### What was accomplished:

1. **✅ Modified Storage Logic**: Changed the email storage system to save emails directly to `mail_storage/cur/` instead of `mail_storage/new/`

2. **✅ Added Dual Functionality**: Created both methods:
   - `save_email()` - saves to `cur/` folder (for piling up test emails)
   - `save_email_to_new()` - saves to `new/` folder (when needed)

3. **✅ Working Email System**: 
   - **SMTP server** (port 8025): Accepts emails via echo/netcat → saves to `cur/`
   - **IMAP server** (port 1143): Can read and manage the stored emails

### Current State:
- **2 emails** successfully saved to `mail_storage/cur/`
- **0 emails** in `mail_storage/new/` (ready for use)
- Both servers running and functional

### How to Test:

**For SMTP testing (echo/netcat):**
```bash
./test_smtp.sh
```

**For IMAP testing (Telnet):**
```bash
telnet localhost 1143
# Then use IMAP commands like: LOGIN, SELECT, SEARCH, FETCH
```

**Quick email check:**
```bash
ls -la mail_storage/cur/    # See all stored emails
```

Your echo test emails will now pile up in the `mail_storage/cur/` folder exactly as you requested! 📧
