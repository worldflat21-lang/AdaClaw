use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    ChaCha20Poly1305, Nonce
};
use anyhow::{anyhow, Result};

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
        let mut ciphertext = self.cipher.encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow!("Encryption failed: {:?}", e))?;
        
        let mut result = nonce.to_vec();
        result.append(&mut ciphertext);
        Ok(result)
    }

    pub fn decrypt(&self, encrypted: &[u8]) -> Result<String> {
        if encrypted.len() < 12 {
            return Err(anyhow!("Invalid ciphertext length"));
        }
        let (nonce_bytes, ciphertext) = encrypted.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        
        let plaintext = self.cipher.decrypt(nonce, ciphertext)
            .map_err(|e| anyhow!("Decryption failed: {:?}", e))?;
            
        String::from_utf8(plaintext).map_err(|e| anyhow!("Invalid UTF-8: {:?}", e))
    }
}
