//! Argon2id password hashing (docs/specs/Auth_Spec.md, "Password hashing").

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use common::{Error, Result};

pub fn hash_password(password: &str) -> Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| Error::wrap("auth", "failed to hash password", e))
}

pub fn verify_password(password: &str, hash: &str) -> Result<bool> {
    let parsed_hash = PasswordHash::new(hash)
        .map_err(|e| Error::wrap("auth", "stored password hash is invalid", e))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correct_password_verifies() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash).unwrap());
    }

    #[test]
    fn wrong_password_does_not_verify() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(!verify_password("wrong password", &hash).unwrap());
    }

    #[test]
    fn hash_never_contains_the_plaintext_password() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(!hash.contains("correct horse battery staple"));
    }

    #[test]
    fn same_password_hashes_differently_each_time() {
        // Distinct random salts per call — a straight equality check on the
        // stored hash could never detect a broken/omitted salt.
        let a = hash_password("correct horse battery staple").unwrap();
        let b = hash_password("correct horse battery staple").unwrap();
        assert_ne!(a, b);
    }
}
