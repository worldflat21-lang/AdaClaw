# Matrix Channel Setup Guide

Connect AdaClaw to the Matrix decentralized messaging protocol.

> **Feature Flag**: Requires compiling with `--features matrix`:
> ```bash
> cargo build --features adaclaw-channels/matrix
> ```
> Or add to the main binary's Cargo.toml features.

## What is Matrix?

[Matrix](https://matrix.org) is an open, decentralized communication protocol. Popular clients include:
- **Element** (Web/Desktop/Mobile) — the flagship client
- **FluffyChat** — mobile-focused
- **Cinny** — web-based

Self-hosting your own homeserver: [Synapse](https://github.com/matrix-org/synapse) or [Conduit](https://conduit.rs/).

---

## Step 1: Create a Bot Account

You need a dedicated Matrix account for your bot.

### Using matrix.org (free)

1. Go to [app.element.io/#/register](https://app.element.io/#/register)
2. Choose homeserver: `matrix.org`
3. Register a new account (e.g., `@adaclaw:matrix.org`)

### Using a Self-Hosted Homeserver

```bash
# Register via admin API (Synapse)
curl -X POST "https://your-homeserver.com/_matrix/client/v3/register" \
  -H "Content-Type: application/json" \
  -d '{"username": "adaclaw", "password": "secure-pass", "kind": "user"}'
```

---

## Step 2: Get the Access Token

You need a persistent `access_token` (not your password).

### Method A: via Element Web

1. Log in to [app.element.io](https://app.element.io) as the bot account
2. Go to **Settings → Help & About → Advanced**
3. Click **Access Token** → copy it

### Method B: via API (Recommended)

```bash
curl -XPOST "https://matrix.org/_matrix/client/v3/login" \
  -H "Content-Type: application/json" \
  -d '{
    "type": "m.login.password",
    "user": "@adaclaw:matrix.org",
    "password": "your-password",
    "device_id": "ADACLAWDEV01",
    "initial_device_display_name": "AdaClaw Bot"
  }'
```

Response:
```json
{
  "access_token": "syt_YWRhY2xhd...",
  "device_id": "ADACLAWDEV01",
  "user_id": "@adaclaw:matrix.org"
}
```

Save the `access_token` and `device_id` — you'll need both.

---

## Step 3: Invite the Bot to a Room

The bot must be in the room to send/receive messages.

1. Open Element
2. Create or open a room
3. Go to **Room Settings → People → Invite**
4. Invite `@adaclaw:matrix.org`
5. Note the **Room ID** (e.g., `!roomid:matrix.org`) from Room Settings → Advanced

---

## Step 4: Configure AdaClaw

```toml
[channels.matrix]
kind = "matrix"
token = "syt_YWRhY2xhd..."       # access_token from Step 2
allow_from = [
  "@alice:matrix.org",            # Allow specific users
  "!roomid:matrix.org",           # Or allow entire rooms
]

[channels.matrix.extra]
homeserver = "https://matrix.org"      # Your homeserver URL
user_id = "@adaclaw:matrix.org"        # Bot user ID
device_id = "ADACLAWDEV01"             # Device ID (consistent across restarts)
sync_timeout_ms = "30000"              # Long poll timeout (30 seconds)
```

### Multi-Room Setup

```toml
[channels.matrix]
kind = "matrix"
token = "syt_..."
allow_from = [
  "!general:your-homeserver.com",
  "!dev:your-homeserver.com",
  "@admin:your-homeserver.com",    # Admin can DM the bot directly
]
```

### Allow All (No Restriction)

```toml
[channels.matrix]
kind = "matrix"
token = "syt_..."
allow_from = []   # Empty = allow all rooms/users the bot is in
```

---

## Step 5: Enable Matrix Feature

### For Development

```bash
cargo run --features adaclaw-channels/matrix
```

### For Production Build

Add to root `Cargo.toml`:
```toml
[dependencies]
adaclaw-channels = { path = "crates/adaclaw-channels", features = ["matrix"] }
```

Or build with:
```bash
cargo build --release --features adaclaw-channels/matrix
```

---

## How It Works

AdaClaw uses **Matrix Client-Server API** with long polling:

1. `GET /_matrix/client/v3/sync?timeout=30000` — long poll for new events
2. Process `m.room.message` events with `msgtype = "m.text"`
3. Ignore messages from the bot itself (prevents loops)
4. `PUT /_matrix/client/v3/rooms/{roomId}/send/m.room.message/{txnId}` — reply

---

## Routing by Room

Configure routing to route different rooms to different agents:

```toml
[[routing]]
channel_pattern = "matrix"
sender_id = "!dev-room:matrix.org"
agent = "coder"

[[routing]]
default = true
agent = "assistant"
```

> **Note**: `sender_id` in routing matches against the `session_id`, which for Matrix is the `room_id`.

---

## E2EE (End-to-End Encryption)

E2EE is **not currently implemented** in this version. Rooms with E2EE enabled will show encrypted messages that cannot be read.

To use E2EE in the future:
- Enable `e2ee_enabled = "true"` in config (reserved, not yet functional)
- The `vodozemac` crate (Olm/Megolm) would be used for key management

For now, use **unencrypted rooms** with the bot account.

---

## Troubleshooting

| Issue | Solution |
|-------|----------|
| `channels.matrix.token (access_token) is required` | Add `token = "syt_..."` to config |
| `channels.matrix.extra.homeserver is required` | Add `homeserver = "https://..."` to extra |
| Bot not responding | Check `allow_from` — add room ID or user ID |
| `Matrix sync error 401` | Access token expired — re-login and update token |
| E2EE messages | Use unencrypted rooms; E2EE not yet supported |
| `Matrix sync error, retrying in 10s` | Network issue; bot will auto-reconnect |
| Feature not enabled | Compile with `--features adaclaw-channels/matrix` |
