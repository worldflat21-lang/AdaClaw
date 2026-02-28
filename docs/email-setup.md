# Email Channel Setup Guide

Connect AdaClaw to your email inbox via IMAP (receive) + SMTP (send).

> ⚠️ **Privacy Notice**: This channel reads your emails. You must explicitly consent by setting
> `extra.consent_granted = "true"` in `config.toml`. AdaClaw only reads **unread** messages
> and marks them as read after processing.

## Supported Services

| Service         | IMAP Host          | SMTP Host          | Notes                    |
|-----------------|--------------------|--------------------|--------------------------|
| Gmail           | `imap.gmail.com`   | `smtp.gmail.com`   | Requires App Password    |
| Outlook/Hotmail | `outlook.office365.com` | `smtp.office365.com` | Use account password |
| Yahoo Mail      | `imap.mail.yahoo.com` | `smtp.mail.yahoo.com` | Requires App Password |
| FastMail        | `imap.fastmail.com` | `smtp.fastmail.com` | Use account password |
| iCloud Mail     | `imap.mail.me.com` | `smtp.mail.me.com` | Requires App Password |
| Self-hosted     | Your IMAP host     | Your SMTP host     | Full control             |

---

## Gmail Setup (Recommended)

Gmail requires an **App Password** instead of your regular password when 2FA is enabled.

### Step 1: Enable 2-Step Verification

1. Go to [myaccount.google.com/security](https://myaccount.google.com/security)
2. Enable **2-Step Verification** (required for App Passwords)

### Step 2: Generate an App Password

1. Go to [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords)
2. Select app: **Mail**, device: **Other** → enter `AdaClaw`
3. Copy the generated 16-character password (e.g., `abcd efgh ijkl mnop`)
4. Remove spaces: `abcdefghijklmnop`

### Step 3: Configure AdaClaw

```toml
[channels.email]
kind = "email"
allow_from = []     # Empty = accept from anyone; add addresses to restrict

[channels.email.extra]
# ── REQUIRED: Explicit consent gate ──────────────────────────────────────────
consent_granted = "true"

# ── IMAP (receive) ────────────────────────────────────────────────────────────
imap_host = "imap.gmail.com"
imap_port = "993"
imap_username = "you@gmail.com"
imap_password = "abcdefghijklmnop"   # App Password (no spaces)

# ── SMTP (send) ───────────────────────────────────────────────────────────────
smtp_host = "smtp.gmail.com"
smtp_port = "587"                    # 587 = STARTTLS (recommended)
smtp_username = "you@gmail.com"
smtp_password = "abcdefghijklmnop"   # Same App Password
from_address = "AdaClaw Agent <you@gmail.com>"

# ── Optional ──────────────────────────────────────────────────────────────────
auto_reply_enabled = "true"          # Set to "false" to only read, not reply
poll_interval_secs = "60"            # Check for new emails every 60 seconds
```

---

## Generic IMAP/SMTP Configuration

For other email providers:

```toml
[channels.email.extra]
consent_granted = "true"

# IMAP
imap_host = "mail.example.com"
imap_port = "993"           # 993 = TLS (recommended)
imap_username = "user@example.com"
imap_password = "your-password"

# SMTP
smtp_host = "mail.example.com"
smtp_port = "587"           # 587 = STARTTLS, 465 = TLS/SSL
smtp_username = "user@example.com"
smtp_password = "your-password"
from_address = "My Bot <user@example.com>"
```

---

## Allowlist (Sender Filtering)

By default, emails from any sender are processed. To restrict:

```toml
[channels.email]
allow_from = [
  "boss@company.com",
  "team@company.com",
]
```

Emails from unlisted senders are silently ignored (not marked as read).

---

## Auto-Reply Control

```toml
[channels.email.extra]
# Disable auto-reply (read-only mode — useful for monitoring inboxes)
auto_reply_enabled = "false"

# Enable with 60-second polling
auto_reply_enabled = "true"
poll_interval_secs = "60"
```

---

## How AdaClaw Processes Emails

1. **Poll**: Connects to IMAP every `poll_interval_secs` seconds
2. **Fetch**: Retrieves all `UNSEEN` messages from INBOX
3. **Parse**: Extracts sender, subject, and body (prefers `text/plain` over `text/html`)
4. **Dispatch**: Sends to Agent as: `[Subject: {subject}]\n\n{body}`
5. **Mark**: Marks emails as `\Seen` after processing
6. **Reply**: Sends Agent response via SMTP (if `auto_reply_enabled = "true"`)

The Agent receives the email formatted as:
```
[Subject: Help with project X]

Hi,

I need help with...
```

---

## Security Recommendations

1. **Use App Passwords** (not your main password) for Gmail/Yahoo/iCloud
2. **Set `allow_from`** to restrict which senders trigger the Agent
3. **Use TLS** (port 993 for IMAP, 465/587 for SMTP)
4. **Review audit logs** when `security.audit_log` is configured — email access is logged

---

## Troubleshooting

| Issue | Solution |
|-------|----------|
| `Email channel requires explicit consent` | Set `extra.consent_granted = "true"` |
| `IMAP login failed` | Check username/password; use App Password for Gmail |
| `SMTP send failed` | Check port (587 vs 465) and credentials |
| Bot replies to its own emails | Add bot's address to `allow_from` exclusion — or use a dedicated email address for the bot |
| Emails piling up | Reduce `poll_interval_secs` or disable if channel is not needed |

---

## Privacy Note

AdaClaw:
- Only reads **INBOX** (not other folders)
- Only reads **UNSEEN** (unread) messages
- Marks messages as **\Seen** after processing (to avoid re-processing)
- Does **not** delete or move emails
- Does **not** send emails when `auto_reply_enabled = "false"`
