# WhatsApp Channel Setup Guide

Connect AdaClaw to WhatsApp Business Cloud API (Meta's official API — free for moderate volumes).

## Prerequisites

- A Meta developer account at [developers.facebook.com](https://developers.facebook.com)
- A WhatsApp Business account (personal accounts are **not** supported)
- HTTPS access to your AdaClaw instance (required by Meta webhook policy)
  - Use the [tunnel integration](../README.md#tunnels): `adaclaw` + Cloudflare/ngrok/Tailscale

---

## Step 1: Create a Meta App

1. Go to [developers.facebook.com/apps](https://developers.facebook.com/apps)
2. Click **Create App** → choose **Business** type
3. Add the **WhatsApp** product to your app
4. Under **WhatsApp → Getting Started**:
   - Note your **Phone Number ID** (e.g., `123456789012345`)
   - Note your temporary **Access Token** (valid 24h) or generate a permanent token via System Users

---

## Step 2: Configure AdaClaw

Edit `config.toml`:

```toml
[channels.whatsapp]
kind = "whatsapp"
token = "EAAxxxx..."              # Access Token from Meta App Dashboard
webhook_secret = "your-app-secret" # App Secret (found in App Settings → Basic → App Secret)
allow_from = []                   # Leave empty to allow all, or add phone numbers

[channels.whatsapp.extra]
phone_number_id = "123456789012345"   # Phone Number ID from Meta Dashboard
verify_token = "my_secret_token_123"  # Any string you choose (used for webhook verification)
webhook_port = "9005"                 # Port for the embedded webhook server
webhook_path = "/whatsapp"            # URL path (leave as default)
```

### Option B: Use Gateway Mode (Shared HTTPS Port)

If you want WhatsApp to use the same port as the main gateway (port 8080 via tunnel):

```toml
[channels.whatsapp]
kind = "whatsapp"
token = "EAAxxxx..."
webhook_secret = "your-app-secret"

[channels.whatsapp.extra]
phone_number_id = "123456789012345"
verify_token = "my_secret_token_123"
# No webhook_port = gateway mode is used automatically
```

Then configure the tunnel to point to port 8080.

---

## Step 3: Start Tunnel + AdaClaw

```bash
# Option 1: Cloudflare Tunnel (recommended)
echo '[tunnel]
provider = "cloudflare"
cloudflare_token = "your-tunnel-token"' >> config.toml

# Option 2: ngrok (quick testing)
echo '[tunnel]
provider = "ngrok"
ngrok_token = "your-ngrok-token"' >> config.toml

# Start
adaclaw run
```

Note the public URL from the tunnel output (e.g., `https://xxx.trycloudflare.com`).

---

## Step 4: Register Webhook in Meta Dashboard

1. Go to **WhatsApp → Configuration** in your Meta App
2. Click **Edit** on Webhook
3. Enter:
   - **Callback URL**: `https://your-tunnel-url/whatsapp`
   - **Verify Token**: The same string you set as `verify_token` in config.toml
4. Click **Verify and Save**
5. Subscribe to the **messages** webhook field

---

## Step 5: Test

Send a WhatsApp message from your phone to the test number:

```
Hello, AdaClaw!
```

You should receive a response from the Agent.

---

## Allowlist Configuration

To restrict which phone numbers can contact the bot:

```toml
[channels.whatsapp]
allow_from = [
  "1234567890",      # E.164 format without + prefix (as Meta sends it)
  "441234567890",    # UK number
]
```

An empty `allow_from` allows messages from anyone. Unrecognized senders are silently ignored.

---

## Production: Permanent Access Token

The temporary token expires in 24 hours. For production:

1. In Meta App → **Business Settings → System Users**
2. Create a System User with admin role
3. Add your WhatsApp App and assign permissions (`whatsapp_business_messaging`, `whatsapp_business_management`)
4. Generate a token with **never expiring** option
5. Update `config.toml` with the permanent token

---

## Supported Message Types

| Type     | Receive | Send |
|----------|---------|------|
| Text     | ✅      | ✅   |
| Image    | ✅ (caption extracted) | ❌ |
| Audio    | ✅ (ID logged)         | ❌ |
| Document | ✅ (caption extracted) | ❌ |
| Video    | ❌      | ❌   |

Media sending support can be added via the `send()` extension in future versions.

---

## Troubleshooting

| Issue | Solution |
|-------|----------|
| `Webhook verification failed` | Check `verify_token` matches exactly (case-sensitive) |
| `X-Hub-Signature-256 verification failed` | Check `webhook_secret` = App Secret (not Access Token) |
| No response from bot | Check `allow_from` — empty means allow all |
| `Graph API error 401` | Access Token expired — generate a new one |
| Meta can't reach webhook | Ensure tunnel is running and HTTPS is valid |
