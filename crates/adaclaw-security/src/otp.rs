//! OTP — TOTP-based One-Time Password (RFC 6238 / RFC 4226)
//!
//! Implements TOTP manually using HMAC-SHA1 to avoid external TOTP crate
//! API compatibility issues. Compatible with Google Authenticator, Authy,
//! and any RFC 6238-compliant TOTP app.
//!
//! Default settings: SHA-1, 6 digits, 30-second step, ±1 step tolerance.
//!
//! # Usage
//!
//! ```rust,ignore
//! use adaclaw_security::otp::OtpProvider;
//! let secret = OtpProvider::generate_secret();
//! println!("Add this to your TOTP app: {}", secret);
//! let otp = OtpProvider::from_base32(&secret).unwrap();
//! if otp.verify("123456") {
//!     println!("OTP valid!");
//! }
//! ```

use anyhow::Result;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha1 = Hmac<Sha1>;

// ── Base32 helpers ────────────────────────────────────────────────────────────

const BASE32_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Encode bytes as base32 (RFC 4648, no padding).
fn base32_encode(data: &[u8]) -> String {
    let mut output = String::new();
    let mut buffer: u32 = 0;
    let mut bits_left: u32 = 0;

    for &byte in data {
        buffer = (buffer << 8) | byte as u32;
        bits_left += 8;
        while bits_left >= 5 {
            bits_left -= 5;
            output.push(BASE32_ALPHABET[((buffer >> bits_left) & 0x1f) as usize] as char);
        }
    }

    if bits_left > 0 {
        output.push(BASE32_ALPHABET[((buffer << (5 - bits_left)) & 0x1f) as usize] as char);
    }

    output
}

/// Decode a base32 string (case-insensitive, RFC 4648, padding ignored).
/// Returns `None` if any character is invalid.
fn base32_decode(input: &str) -> Option<Vec<u8>> {
    let mut buffer: u32 = 0;
    let mut bits_left: u32 = 0;
    let mut output = Vec::new();

    for ch in input.chars() {
        let ch_upper = ch.to_ascii_uppercase();
        // Skip padding characters
        if ch_upper == '=' {
            continue;
        }
        let val = BASE32_ALPHABET.iter().position(|&x| x == ch_upper as u8)?;
        buffer = (buffer << 5) | val as u32;
        bits_left += 5;
        if bits_left >= 8 {
            bits_left -= 8;
            output.push((buffer >> bits_left) as u8);
        }
    }

    Some(output)
}

// ── OtpProvider ───────────────────────────────────────────────────────────────

/// TOTP provider (RFC 6238: SHA-1, 6 digits, 30-second step).
pub struct OtpProvider {
    secret: Vec<u8>,
    digits: u32,
    step: u64,
    /// Number of adjacent steps to check on either side (default: 1 → ±30s window).
    skew: u64,
}

impl OtpProvider {
    /// Create a provider from a base32-encoded secret string.
    pub fn from_base32(secret_b32: &str) -> Result<Self> {
        let secret = base32_decode(secret_b32.trim()).ok_or_else(|| {
            anyhow::anyhow!("Invalid base32 secret: contains non-base32 characters")
        })?;
        if secret.is_empty() {
            anyhow::bail!("OTP secret must not be empty");
        }
        Ok(Self {
            secret,
            digits: 6,
            step: 30,
            skew: 1,
        })
    }

    /// Create a provider from raw secret bytes.
    pub fn from_raw(secret: Vec<u8>) -> Self {
        Self {
            secret,
            digits: 6,
            step: 30,
            skew: 1,
        }
    }

    /// Generate a new random 160-bit (20-byte) TOTP secret, returned as base32.
    ///
    /// Store this securely (e.g. in `SecretStore`). Display it to the user
    /// only once so they can add it to their TOTP app.
    pub fn generate_secret() -> String {
        use rand_core::{OsRng, RngCore};
        let mut bytes = [0u8; 20];
        OsRng.fill_bytes(&mut bytes);
        base32_encode(&bytes)
    }

    /// Return the secret as a base32 string (for display / QR code generation).
    pub fn secret_base32(&self) -> String {
        base32_encode(&self.secret)
    }

    /// Verify a 6-digit TOTP token string.
    ///
    /// Accepts the current step ± `skew` steps (default ±1 → ±30 second window)
    /// to accommodate clock drift.
    pub fn verify(&self, token: &str) -> bool {
        let token = token.trim();
        // Must be exactly `digits` decimal characters
        if token.len() != self.digits as usize {
            return false;
        }
        let Ok(code) = token.parse::<u32>() else {
            return false;
        };

        let counter = self.current_counter();
        for delta in 0..=(self.skew as i64) {
            for sign in [1i64, -1] {
                let c = (counter as i64 + delta * sign) as u64;
                if self.hotp(c) == code {
                    return true;
                }
            }
        }
        false
    }

    /// Generate the current TOTP code as a zero-padded string.
    pub fn current_code(&self) -> Result<String> {
        let counter = self.current_counter();
        let code = self.hotp(counter);
        Ok(format!("{:0>width$}", code, width = self.digits as usize))
    }

    /// Build an `otpauth://` provisioning URI for QR code apps (Google Authenticator, etc.).
    ///
    /// Scan with any TOTP app to add the account.
    pub fn provisioning_uri(&self, account: &str, issuer: &str) -> String {
        format!(
            "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={}&period={}",
            urlencoded(issuer),
            urlencoded(account),
            self.secret_base32(),
            urlencoded(issuer),
            self.digits,
            self.step,
        )
    }

    // ── Internal HOTP ─────────────────────────────────────────────────────────

    /// Compute HOTP(secret, counter) → n-digit code.  (RFC 4226 §5)
    fn hotp(&self, counter: u64) -> u32 {
        let mut mac =
            HmacSha1::new_from_slice(&self.secret).expect("HMAC-SHA1 accepts any key length");
        mac.update(&counter.to_be_bytes());
        let result = mac.finalize().into_bytes();

        // Dynamic truncation (RFC 4226 §5.3)
        let offset = (result[19] & 0x0f) as usize;
        let code = ((result[offset] & 0x7f) as u32) << 24
            | (result[offset + 1] as u32) << 16
            | (result[offset + 2] as u32) << 8
            | result[offset + 3] as u32;

        code % 10u32.pow(self.digits)
    }

    fn current_counter(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            / self.step
    }
}

/// Minimal percent-encoding for otpauth:// URI labels.
fn urlencoded(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            c => format!("%{:02X}", c as u32),
        })
        .collect()
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Known-answer test vectors from RFC 4226 Appendix D (HOTP, secret = 0x3132...3839)
    // Note: TOTP wraps HOTP with time counter, so we test HOTP internals directly.
    const RFC_SECRET: &[u8] = b"12345678901234567890"; // 20 bytes

    fn rfc_provider() -> OtpProvider {
        OtpProvider::from_raw(RFC_SECRET.to_vec())
    }

    #[test]
    fn test_hotp_rfc4226_vectors() {
        let p = rfc_provider();
        // RFC 4226 Appendix D test vectors
        let expected = [
            755224u32, 287082, 359152, 969429, 338314, 254676, 287922, 162583, 399871, 520489,
        ];
        for (counter, &expected_code) in expected.iter().enumerate() {
            assert_eq!(
                p.hotp(counter as u64),
                expected_code,
                "HOTP counter={} failed",
                counter
            );
        }
    }

    #[test]
    fn test_generate_secret_is_valid_base32() {
        let secret = OtpProvider::generate_secret();
        // Must be non-empty and decodable
        assert!(!secret.is_empty());
        let decoded = base32_decode(&secret);
        assert!(decoded.is_some(), "Generated secret must be valid base32");
        assert_eq!(decoded.unwrap().len(), 20); // 160 bits
    }

    #[test]
    fn test_from_base32_roundtrip() {
        let secret = OtpProvider::generate_secret();
        let otp = OtpProvider::from_base32(&secret).unwrap();
        assert_eq!(otp.secret_base32(), secret);
    }

    #[test]
    fn test_verify_current_code() {
        let otp = OtpProvider::from_raw(RFC_SECRET.to_vec());
        let code = otp.current_code().unwrap();
        assert_eq!(code.len(), 6);
        assert!(otp.verify(&code), "current code should verify");
    }

    #[test]
    fn test_verify_wrong_code() {
        let otp = OtpProvider::from_raw(RFC_SECRET.to_vec());
        assert!(!otp.verify("000000"));
        assert!(!otp.verify("999999"));
    }

    #[test]
    fn test_verify_wrong_length() {
        let otp = OtpProvider::from_raw(RFC_SECRET.to_vec());
        assert!(!otp.verify("12345")); // too short
        assert!(!otp.verify("1234567")); // too long
        assert!(!otp.verify("abc123")); // non-numeric
    }

    #[test]
    fn test_from_base32_invalid() {
        assert!(OtpProvider::from_base32("!!!INVALID!!!").is_err());
        assert!(OtpProvider::from_base32("").is_err());
    }

    #[test]
    fn test_provisioning_uri_format() {
        let otp = OtpProvider::from_raw(RFC_SECRET.to_vec());
        let uri = otp.provisioning_uri("user@example.com", "AdaClaw");
        assert!(uri.starts_with("otpauth://totp/"));
        assert!(uri.contains("issuer=AdaClaw"));
        assert!(uri.contains("algorithm=SHA1"));
        assert!(uri.contains("digits=6"));
        assert!(uri.contains("period=30"));
    }

    #[test]
    fn test_base32_encode_decode_roundtrip() {
        let data = b"Hello, World! This is a test.";
        let encoded = base32_encode(data);
        let decoded = base32_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_base32_case_insensitive_decode() {
        let upper = "JBSWY3DPEB3W64TMMQ";
        let lower = "jbswy3dpeb3w64tmmq";
        assert_eq!(base32_decode(upper), base32_decode(lower));
    }
}
