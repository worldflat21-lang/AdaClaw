//! `SecretStore` — 运行时密钥加密存储
//!
//! ## Phase 14-P1-2 改进
//!
//! - `decrypt()` 现在返回 `secrecy::Secret<String>` 而非裸 `String`。
//!   解密后的明文被 `Secret<T>` 包装，`Debug` 输出为 `[REDACTED]`，
//!   防止在日志、panic 报告或内存 dump 中意外泄露 API 密钥。
//! - 调用方需要明文时，必须显式调用 `.expose_secret()` ——
//!   此调用点即密钥暴露位置，便于审计。

use anyhow::{Result, anyhow};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use secrecy::Secret;

pub struct SecretStore {
    cipher: ChaCha20Poly1305,
}

impl SecretStore {
    pub fn new(key: &[u8; 32]) -> Self {
        let cipher = ChaCha20Poly1305::new(key.into());
        Self { cipher }
    }

    pub fn generate_key() -> [u8; 32] {
        ChaCha20Poly1305::generate_key(&mut OsRng).into()
    }

    pub fn encrypt(&self, plaintext: &str) -> Result<Vec<u8>> {
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng); // 96-bits; unique per message
        let mut ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow!("Encryption failed: {:?}", e))?;

        let mut result = nonce.to_vec();
        result.append(&mut ciphertext);
        Ok(result)
    }

    /// Decrypt ciphertext and return the plaintext wrapped in [`Secret`].
    ///
    /// The returned `Secret<String>` redacts itself in `Debug` / `Display`
    /// output.  Call `.expose_secret()` only at the point where the raw
    /// value is actually needed (e.g. building an `Authorization` header).
    pub fn decrypt(&self, encrypted: &[u8]) -> Result<Secret<String>> {
        if encrypted.len() < 12 {
            return Err(anyhow!("Invalid ciphertext length"));
        }
        let (nonce_bytes, ciphertext) = encrypted.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);

        let plaintext = self
            .cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow!("Decryption failed: {:?}", e))?;

        let s = String::from_utf8(plaintext).map_err(|e| anyhow!("Invalid UTF-8: {:?}", e))?;
        Ok(Secret::new(s))
    }
}
