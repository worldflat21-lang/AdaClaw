//! Credential scrubber — removes sensitive values from strings before logging or
//! returning them to the user.
//!
//! Two-pass strategy:
//! 1. **Bearer token regex** — scrubs `Authorization: Bearer <token>` style headers
//! 2. **Key=value regex** — scrubs `api_key=sk-...`, `password: hunter2`, etc.
//!
//! Values shorter than 4 characters are not scrubbed (regex requires `{4,}`).
//! The first 4 characters of each secret are retained to aid debugging:
//! `api_key=sk-abcdefg` → `api_key=sk-a****`
//!
//! # Security note
//!
//! This is a best-effort defence against accidental credential leakage in logs.
//! It is **not** a substitute for proper secret management (use `SecretStore`).

use regex::Regex;
use std::sync::LazyLock;

// ── Regexes ───────────────────────────────────────────────────────────────────

/// Matches `Authorization: Bearer <token>` (and variants).
///
/// Run BEFORE `SENSITIVE_KV_REGEX` so that the word "Bearer" is not itself
/// mistakenly treated as part of a key=value pair.
static BEARER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(Bearer\s+)([A-Za-z0-9\-_\.+/]{10,}=*)").expect("Invalid BEARER_REGEX")
});

/// Matches key=value and key: value pairs for a comprehensive list of sensitive
/// key names.
///
/// Group 1 — the key name (retained)
/// Group 2 — the separator + optional opening quote (retained)
/// Group 3 — the secret value (first 4 chars retained, rest replaced with `****`)
static SENSITIVE_KV_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Note: `authorization` is intentionally excluded — HTTP Authorization headers
    // containing Bearer tokens are already handled by BEARER_REGEX (Pass 1).
    // Including `authorization` here would cause double-scrubbing of the Bearer scheme.
    // We include `auth_token`, `auth_key`, `auth_secret` (with required separator)
    // but not bare `auth` or `authorization`.
    Regex::new(
        r#"(?i)(token|api[_-]?key|api[_-]?secret|password|passwd|passphrase|secret|credential[s]?|private[_-]?key|access[_-]?key|client[_-]?secret|auth[_\-](?:token|key|secret)|x[_-]?api[_-]?key|webhook[_-]?secret|signing[_-]?secret|session[_-]?(?:token|key|secret)|refresh[_-]?token|encrypt(?:ion)?[_-]?key|database[_-]?(?:url|password|pass)|db[_-]?pass(?:word)?|smtp[_-]?pass(?:word)?)["']?\s*([=:]\s*["']?)([^\s,;"'\n\r]{4,})"#
    ).expect("Invalid SENSITIVE_KV_REGEX")
});

/// Matches URL-embedded credentials: `https://user:password@host`
static URL_CRED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(https?://[^:@\s]+:)([^@\s]{4,})(@)").expect("Invalid URL_CRED_REGEX")
});

// ── Public API ────────────────────────────────────────────────────────────────

/// Scrub sensitive credentials from a string.
///
/// Applies three passes in order:
/// 1. Bearer tokens — `Bearer eyJh****`
/// 2. URL-embedded credentials — `https://user:****@host`
/// 3. Key=value pairs — `api_key=sk-a****`
///
/// Keeps the first 4 characters of each secret value to aid debugging.
/// The original string is returned unchanged if no sensitive values are found.
pub fn scrub_credentials(input: &str) -> String {
    // ── Pass 1: Bearer tokens ─────────────────────────────────────────────────
    let after_bearer = BEARER_REGEX.replace_all(input, |caps: &regex::Captures| {
        let prefix = safe_prefix(&caps[2], 4);
        format!("{}{}****", &caps[1], prefix)
    });

    // ── Pass 2: URL-embedded credentials ─────────────────────────────────────
    let after_url = URL_CRED_REGEX.replace_all(&after_bearer, |caps: &regex::Captures| {
        let prefix = safe_prefix(&caps[2], 4);
        format!("{}{}****{}", &caps[1], prefix, &caps[3])
    });

    // ── Pass 3: key=value / key: "value" patterns ─────────────────────────────
    let after_kv = SENSITIVE_KV_REGEX.replace_all(&after_url, |caps: &regex::Captures| {
        let key = &caps[1];
        let sep = &caps[2];
        let val = &caps[3];
        let prefix = safe_prefix(val, 4);
        format!("{}{}{}****", key, sep, prefix)
    });

    after_kv.into_owned()
}

/// Return the first `n` characters of `s` (Unicode-safe — won't split multi-byte chars).
fn safe_prefix(s: &str, n: usize) -> &str {
    let end = s
        .char_indices()
        .nth(n)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());
    &s[..end]
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── api_key / token patterns ──────────────────────────────────────────────

    #[test]
    fn test_api_key_equals() {
        let out = scrub_credentials("api_key=sk-abcdefghijklmnop");
        assert!(out.contains("sk-a****"), "got: {}", out);
        assert!(!out.contains("sk-abcdefghijklmnop"), "should be scrubbed");
    }

    #[test]
    fn test_api_key_colon() {
        let out = scrub_credentials("api_key: sk-abcdefghijklmnop");
        assert!(out.contains("sk-a****"), "got: {}", out);
    }

    #[test]
    fn test_token_json_style() {
        let out = scrub_credentials(r#"{"token": "abcdef123456789"}"#);
        assert!(out.contains("abcd****"), "got: {}", out);
        assert!(!out.contains("abcdef123456789"), "should be scrubbed");
    }

    #[test]
    fn test_api_secret() {
        let out = scrub_credentials("api_secret=AKIAIOSFODNN7EXAMPLE");
        assert!(out.contains("AKIA****"), "got: {}", out);
    }

    #[test]
    fn test_client_secret() {
        let out = scrub_credentials("client_secret=my_super_secret_value");
        assert!(out.contains("my_s****"), "got: {}", out);
    }

    // ── Bearer tokens ─────────────────────────────────────────────────────────

    #[test]
    fn test_bearer_header() {
        let out = scrub_credentials("Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9");
        assert!(out.contains("Bearer eyJh****"), "got: {}", out);
        assert!(!out.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"));
    }

    #[test]
    fn test_bearer_lowercase() {
        let out = scrub_credentials("authorization: bearer sk-abc1234567890abcdef");
        assert!(out.contains("sk-a****"), "got: {}", out);
    }

    // ── Password patterns ─────────────────────────────────────────────────────

    #[test]
    fn test_password_colon() {
        let out = scrub_credentials("password: mysecretpassword123");
        assert!(out.contains("myse****"), "got: {}", out);
        assert!(!out.contains("mysecretpassword"), "should be scrubbed");
    }

    #[test]
    fn test_passwd_equals() {
        let out = scrub_credentials("passwd=hunter2isverysecret");
        assert!(out.contains("hunt****"), "got: {}", out);
    }

    #[test]
    fn test_database_password() {
        let out = scrub_credentials("database_password=mydbpass1234");
        assert!(out.contains("mydb****"), "got: {}", out);
    }

    #[test]
    fn test_db_pass() {
        let out = scrub_credentials("db_pass=secretvalue1234");
        assert!(out.contains("secr****"), "got: {}", out);
    }

    // ── URL credentials ───────────────────────────────────────────────────────

    #[test]
    fn test_url_embedded_credentials() {
        let out =
            scrub_credentials("Connecting to https://myuser:mysupersecretpassword@db.example.com");
        assert!(out.contains("mysu****"), "got: {}", out);
        assert!(
            !out.contains("mysupersecretpassword"),
            "password should be scrubbed"
        );
        assert!(out.contains("myuser:"), "username should be preserved");
        assert!(out.contains("@db.example.com"), "host should be preserved");
    }

    // ── Webhook / signing secrets ─────────────────────────────────────────────

    #[test]
    fn test_webhook_secret() {
        let out = scrub_credentials("webhook_secret=whsec_1234567890abcdef");
        assert!(out.contains("whse****"), "got: {}", out);
    }

    #[test]
    fn test_signing_secret() {
        let out = scrub_credentials("signing_secret=abc123xyz890qwerty");
        assert!(out.contains("abc1****"), "got: {}", out);
    }

    // ── Multiple secrets in one string ────────────────────────────────────────

    #[test]
    fn test_multiple_secrets() {
        let input = "api_key=sk-12345678 secret=mysecretvalue bearer eyJhbGciOiJSUzI1Ni";
        let out = scrub_credentials(input);
        assert!(!out.contains("sk-12345678"), "api_key should be scrubbed");
        assert!(!out.contains("mysecretvalue"), "secret should be scrubbed");
        assert!(
            !out.contains("eyJhbGciOiJSUzI1Ni"),
            "bearer should be scrubbed"
        );
    }

    // ── Edge cases ────────────────────────────────────────────────────────────

    #[test]
    fn test_clean_input_unchanged() {
        let input = "hello world, no secrets here";
        assert_eq!(scrub_credentials(input), input);
    }

    #[test]
    fn test_short_value_not_scrubbed() {
        // Values shorter than 4 chars — regex requires {4,}
        let input = "token=abc";
        let out = scrub_credentials(input);
        // Just ensure no panic and output is reasonable
        assert!(!out.contains("****") || out == "token=abc");
    }

    #[test]
    fn test_empty_string() {
        assert_eq!(scrub_credentials(""), "");
    }

    #[test]
    fn test_unicode_prefix_safe() {
        // Unicode key — safe_prefix should not split multi-byte chars
        let out = scrub_credentials("token=😀😀😀😀😀😀😀😀");
        assert!(out.contains("****"));
        // Should not panic on unicode
    }

    #[test]
    fn test_x_api_key() {
        let out = scrub_credentials("x-api-key: super_secret_api_key_123");
        assert!(out.contains("supe****"), "got: {}", out);
    }

    #[test]
    fn test_refresh_token() {
        let out = scrub_credentials("refresh_token=1//0abcdefghijklmnopqrstuvwxyz");
        assert!(out.contains("1//0****"), "got: {}", out);
    }

    #[test]
    fn test_session_token() {
        let out = scrub_credentials("session_token=sess_live_abc123defghij");
        assert!(out.contains("sess****"), "got: {}", out);
    }

    #[test]
    fn test_encryption_key() {
        let out = scrub_credentials("encryption_key=base64encodedkeyvalue==");
        assert!(out.contains("base****"), "got: {}", out);
    }

    #[test]
    fn test_private_key_pattern() {
        let out = scrub_credentials("private_key=MIIEpAIBAAKCAQEA123456789");
        assert!(out.contains("MIIE****"), "got: {}", out);
    }

    #[test]
    fn test_smtp_password() {
        let out = scrub_credentials("smtp_password=mailserver_secret_pw");
        assert!(out.contains("mail****"), "got: {}", out);
    }

    #[test]
    fn test_multiline_secrets() {
        let input = "config:\n  api_key: sk-12345678abcdef\n  name: myapp";
        let out = scrub_credentials(input);
        assert!(!out.contains("sk-12345678abcdef"), "key should be scrubbed");
        assert!(
            out.contains("name: myapp"),
            "non-secret should be preserved"
        );
    }

    #[test]
    fn test_safe_prefix_short_input() {
        // safe_prefix with n >= len should return the whole string
        assert_eq!(safe_prefix("ab", 4), "ab");
        assert_eq!(safe_prefix("", 4), "");
    }

    #[test]
    fn test_safe_prefix_exact() {
        assert_eq!(safe_prefix("abcd", 4), "abcd");
    }

    #[test]
    fn test_safe_prefix_longer() {
        assert_eq!(safe_prefix("abcdefgh", 4), "abcd");
    }
}
