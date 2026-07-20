#!/bin/bash

# Test IMAP server (port 1143)
echo "=== Testing IMAP Server ==="
echo "Connecting to IMAP server on port 1143..."

(
  sleep 1
  echo "A1 LOGIN testuser testpass"
  sleep 1
  echo "A2 SELECT INBOX"
  sleep 1
  echo "A3 LIST \"\" *"
  sleep 1
  echo "A4 LOGOUT"
) | telnet localhost 1143

echo ""
echo "After IMAP testing, check mail_storage for emails:"
echo "mail_storage/cur/ - for processed emails"
echo "mail_storage/new/ - for new/unread emails"