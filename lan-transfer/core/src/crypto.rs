//! Ed25519 签名封装。设备ID就是公钥，握手时用私钥签名证明"我确实是这个ID"。

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

pub fn public_from_secret(secret: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

pub fn sign(secret: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    SigningKey::from_bytes(secret).sign(msg).to_bytes()
}

pub fn verify(public: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    let sig = Signature::from_bytes(sig);
    vk.verify_strict(msg, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_roundtrip() {
        let secret = [5u8; 32];
        let public = public_from_secret(&secret);
        let sig = sign(&secret, b"hello");
        assert!(verify(&public, b"hello", &sig));
        assert!(!verify(&public, b"hellO", &sig));
        let other = public_from_secret(&[6u8; 32]);
        assert!(!verify(&other, b"hello", &sig));
    }
}
