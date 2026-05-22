//! Earner attestation verification.
//!
//! The earner signs the canonical `proto::signing_digest(job_id, output_hash)`
//! with a recoverable secp256k1 ECDSA signature and ships the 65-byte
//! `[r||s||v]` as `signature_hex`. We recover the signer's public key, derive
//! its Ethereum-style address, and confirm it matches the claimed
//! `earner_address`.

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use proto::signing_digest;
use sha3::{Digest, Keccak256};
use uuid::Uuid;

/// Why a submission's signature failed verification.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// `signature_hex` was not valid hex or not 65 bytes.
    BadSignatureEncoding,
    /// Could not recover a public key from the signature + digest.
    Unrecoverable,
    /// Recovered address did not match the claimed `earner_address`.
    AddressMismatch,
}

/// Derive an Ethereum-style address (0x-prefixed, lowercase) from a verifying
/// key: keccak256(uncompressed_pubkey[1..])[12..].
fn address_from_verifying_key(vk: &VerifyingKey) -> String {
    let point = vk.to_encoded_point(false);
    let bytes = point.as_bytes(); // 65 bytes: 0x04 || X || Y
    let hash = Keccak256::digest(&bytes[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

/// Recover the signer address from a hex-encoded `[r||s||v]` signature over
/// `signing_digest(job_id, output_hash)` and assert it equals
/// `claimed_address` (case-insensitive).
pub fn verify_signature(
    job_id: &Uuid,
    output_hash: &str,
    claimed_address: &str,
    signature_hex: &str,
) -> Result<(), VerifyError> {
    let raw = hex::decode(signature_hex.strip_prefix("0x").unwrap_or(signature_hex))
        .map_err(|_| VerifyError::BadSignatureEncoding)?;
    if raw.len() != 65 {
        return Err(VerifyError::BadSignatureEncoding);
    }
    let sig =
        Signature::from_slice(&raw[..64]).map_err(|_| VerifyError::BadSignatureEncoding)?;
    let recid = RecoveryId::from_byte(raw[64]).ok_or(VerifyError::BadSignatureEncoding)?;

    let digest = signing_digest(job_id, output_hash);
    let vk = VerifyingKey::recover_from_prehash(&digest, &sig, recid)
        .map_err(|_| VerifyError::Unrecoverable)?;
    let recovered = address_from_verifying_key(&vk);

    if recovered.eq_ignore_ascii_case(claimed_address) {
        Ok(())
    } else {
        Err(VerifyError::AddressMismatch)
    }
}

/// Test-only: produce a valid hex `[r||s||v]` signature over
/// `signing_digest(job_id, output_hash)`, mirroring the earner. Lives here so
/// both the unit tests and the `submit` integration tests can build valid
/// envelopes from the same code path.
#[cfg(test)]
pub(crate) fn sign_for_test(
    sk: &k256::ecdsa::SigningKey,
    job_id: &Uuid,
    output_hash: &str,
) -> String {
    let digest = signing_digest(job_id, output_hash);
    let (sig, recid): (Signature, RecoveryId) =
        sk.sign_prehash_recoverable(&digest).unwrap();
    let mut out = sig.to_bytes().to_vec();
    out.push(recid.to_byte());
    hex::encode(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::SigningKey;

    fn dev_key() -> SigningKey {
        let bytes =
            hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318")
                .unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    fn sign(sk: &SigningKey, job_id: &Uuid, output_hash: &str) -> String {
        sign_for_test(sk, job_id, output_hash)
    }

    fn dev_address() -> String {
        address_from_verifying_key(dev_key().verifying_key())
    }

    #[test]
    fn valid_signature_verifies() {
        let sk = dev_key();
        let job_id = Uuid::new_v4();
        let hash = "abc123";
        let sig = sign(&sk, &job_id, hash);
        assert_eq!(verify_signature(&job_id, hash, &dev_address(), &sig), Ok(()));
    }

    #[test]
    fn tampered_signature_rejected() {
        let sk = dev_key();
        let job_id = Uuid::new_v4();
        let hash = "abc123";
        let mut sig = sign(&sk, &job_id, hash);
        // Flip the last hex char to corrupt the recovery byte / s value.
        sig.pop();
        sig.push('f');
        let res = verify_signature(&job_id, hash, &dev_address(), &sig);
        assert!(res.is_err());
    }

    #[test]
    fn mismatched_address_rejected() {
        let sk = dev_key();
        let job_id = Uuid::new_v4();
        let hash = "abc123";
        let sig = sign(&sk, &job_id, hash);
        let res = verify_signature(
            &job_id,
            hash,
            "0x000000000000000000000000000000000000dead",
            &sig,
        );
        assert_eq!(res, Err(VerifyError::AddressMismatch));
    }

    #[test]
    fn malformed_signature_encoding_rejected() {
        // The two encoding-level reject branches, distinct from the recovery-byte
        // path `tampered_signature_rejected` hits: no other test asserts the
        // BadSignatureEncoding variant by name on this security gate.
        let job_id = Uuid::new_v4();
        let hash = "abc123";
        let addr = dev_address();
        // Non-hex input fails at hex::decode (the optional 0x prefix is stripped first).
        assert_eq!(
            verify_signature(&job_id, hash, &addr, "0xnothex"),
            Err(VerifyError::BadSignatureEncoding)
        );
        // Valid hex but the wrong byte length (64, not the required 65) is rejected
        // before any curve parsing.
        let sixty_four_bytes = "00".repeat(64);
        assert_eq!(
            verify_signature(&job_id, hash, &addr, &sixty_four_bytes),
            Err(VerifyError::BadSignatureEncoding)
        );
    }
}
