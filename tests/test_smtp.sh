#!/bin/bash

# Test SMTP server (port 8025)
echo "=== Testing SMTP Server ==="
echo "Sending email to SMTP server on port 8025..."

(echo -e "EHLO testclient.com\r\nMAIL FROM:<test@example.com>\r\nRCPT TO:<recipient@example.com>\r\nDATA\r\nSubject: SMTP Test Email\r\n\r\nThis is a test email sent via SMTP protocol.\r\nIt should be saved to the cur folder.\r\n.\r\nQUIT"; sleep 1) | nc localhost 8025

echo ""
echo "Checking mail_storage/cur/ for SMTP emails:"
ls -la mail_storage/cur/ 2>/dev/null || echo "No emails in cur folder yet"

echo ""
echo "Checking mail_storage/new/ for SMTP emails:"
ls -la mail_storage/new/ 2>/dev/null || echo "No emails in new folder yet"