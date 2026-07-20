#!/bin/bash

# Test script to verify email storage works
set -e

echo "🧪 Testing Email Storage System..."
echo ""

# Start the server in background
echo "🚀 Starting email server..."
cargo run --release > /tmp/email_server.log 2>&1 &
SERVER_PID=$!
echo "Server PID: $SERVER_PID"

# Wait for server to start
sleep 3

echo ""
echo "📧 Sending test email via SMTP..."
# Send email without authentication (should work now)
echo -e "EHLO test.com\r\nMAIL FROM:<alice@test.com>\r\nRCPT TO:<testuser@test.com>\r\nDATA\r\nSubject: Test Email\r\nFrom: alice@test.com\r\nTo: testuser@test.com\r\n\r\nThis is a test email to verify mail storage works!\r\n.\r\nQUIT\r\n" | nc -q 2 localhost 8025 > /tmp/smtp_response.txt 2>&1

echo "SMTP Response:"
cat /tmp/smtp_response.txt
echo ""

echo "📬 Checking storage directory..."
find /home/gy/my_projects/Rust_Lang/rust_email_server_ver2/mail_storage -type f
echo ""

echo "📧 Testing IMAP retrieval..."
echo -e "A1 LOGIN testuser testpass\r\nA2 SELECT INBOX\r\nA3 SEARCH ALL\r\nA4 FETCH 1 (BODY[])\r\nA5 LOGOUT\r\n" | nc -q 2 localhost 1143 > /tmp/imap_response.txt 2>&1

echo "IMAP Response:"
cat /tmp/imap_response.txt
echo ""

echo "📊 Server logs:"
tail -20 /tmp/email_server.log
echo ""

# Cleanup
kill $SERVER_PID 2>/dev/null || true

echo "✅ Test complete!"