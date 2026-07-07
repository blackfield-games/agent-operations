//! Earner signature verification — result attestations and registration.
//!
//! Both gates share one mechanism: the earner signs a canonical 32-byte digest
//! with a recoverable secp256k1 ECDSA signature and ships the 65-byte `[r||s||v]`
//! as `signature_hex`; we recover the signer's public key, derive its
//! Ethereum-style address, and confirm it matches the claimed `earner_address`.
//! Only the digest differs: result attestations sign
//! `proto::signing_digest(job_id, output_hash)` ([`verify_signature`]),
//! registration signs `proto::hello_digest` over the advertised capabilities
//! ([`verify_hello_signature`]).
//!
//! Signatures must be canonical (low-S, per EIP-2): for any valid `(r, s)` the
//! malleated `(r, n - s)` with the flipped recovery bit recovers the same key,
//! so accepting high-S would let one attestation carry two distinct 65-byte
//! encodings. We reject the high-S half so each accepted result has exactly one
//! valid signature — the form the earner's signer already produces.

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use proto::{hello_digest, signing_digest, JobKind};
use sha3::{Digest, Keccak256};
use uuid::Uuid;

/// Why a submission's signature failed verification.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// `signature_hex` was not valid hex or not 65 bytes.
    BadSignatureEncoding,
    /// Signature parsed but carries a high-S value (non-canonical / malleable
    /// per EIP-2). Rejected so each accepted attestation has one canonical form.
    NonCanonicalSignature,
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

/// The canonical identity form of an earner address — the key the registry, the
/// `earner_faults` ledger, and the `/stats` leaderboard all index on. EVM
/// addresses are case-insensitive (EIP-55 only varies hex case for an integrity
/// checksum), but identity is keyed on the raw string, so a client that presents
/// one case at registration and another at submit would split into two identities:
/// fault counts split (bypassing the max-faults dead-letter budget) and the
/// leaderboard duplicates. Folding to lowercase yields exactly what
/// [`address_from_verifying_key`] recovers (`hex::encode` emits lowercase), so for
/// any signature the gate accepts — it matches case-insensitively
/// ([`verify_recovered_address`]) — the normalized claim IS the recovered signer
/// address: identity derived from the key, not the case the client chose. Apply at
/// every identity boundary so all keys derive from one canonical form.
pub fn canonical_earner_address(address: &str) -> String {
    address.to_ascii_lowercase()
}

/// Recover the signer address from a hex-encoded `[r||s||v]` signature over
/// `digest` and assert it equals `claimed_address` (case-insensitive). The
/// shared core of both gates: identical decoding, low-S enforcement, and
/// recovery — only the digest the caller commits to differs.
fn verify_recovered_address(
    digest: &[u8; 32],
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
    // Enforce low-S (EIP-2): `normalize_s` yields `Some` only when `s` was high,
    // i.e. the malleable half. Reject it so a signature has one canonical form.
    if sig.normalize_s().is_some() {
        return Err(VerifyError::NonCanonicalSignature);
    }
    let recid = RecoveryId::from_byte(raw[64]).ok_or(VerifyError::BadSignatureEncoding)?;

    let vk = VerifyingKey::recover_from_prehash(digest, &sig, recid)
        .map_err(|_| VerifyError::Unrecoverable)?;
    if address_from_verifying_key(&vk).eq_ignore_ascii_case(claimed_address) {
        Ok(())
    } else {
        Err(VerifyError::AddressMismatch)
    }
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
    verify_recovered_address(
        &signing_digest(job_id, output_hash),
        claimed_address,
        signature_hex,
    )
}

/// Recover the signer from a `Hello`'s `signature_hex` over `hello_digest` and
/// assert it equals the claimed `earner_address` (case-insensitive). Proves the
/// registrant controls the key behind the address the capability filter, fault
/// attribution, and `/stats` totals key on, so the self-reported identity is
/// authenticated rather than trusted on assertion.
///
/// `earner_address` feeds the digest *and* is the recovery target, so a captured
/// signature can't be reattached to a different claimed address — a forger would
/// have to recover its own (different) address.
///
/// `nonce` binds freshness. On the WS path the coordinator passes the
/// per-connection challenge it issued, so a Hello captured off the wire and
/// replayed on a *new* connection — which receives a different challenge — fails
/// recovery and never bootstraps a session under the victim's address. The HTTP
/// `/register` path passes an empty nonce: its replay is a benign idempotent
/// upsert of the victim's own data (no session, no reputation effect), so it is
/// intentionally not challenge-gated. (Anti-replay landed in
/// `mesh-signed-hello-nonce`; HTTP freshness would only matter if `/register`
/// ever gained a session/reputation effect.)
pub fn verify_hello_signature(
    earner_address: &str,
    gpu_model: &str,
    vram_gb: u32,
    supported: &[JobKind],
    nonce: &[u8],
    signature_hex: &str,
) -> Result<(), VerifyError> {
    verify_recovered_address(
        &hello_digest(earner_address, gpu_model, vram_gb, supported, nonce),
        earner_address,
        signature_hex,
    )
}

/// Test-only: produce a valid hex `[r||s||v]` signature over an arbitrary
/// 32-byte prehash, mirroring the earner's signer. Lives here so the result and
/// registration test helpers build envelopes from one code path.
#[cfg(test)]
pub(crate) fn sign_digest_for_test(sk: &k256::ecdsa::SigningKey, digest: &[u8; 32]) -> String {
    let (sig, recid): (Signature, RecoveryId) = sk.sign_prehash_recoverable(digest).unwrap();
    let mut out = sig.to_bytes().to_vec();
    out.push(recid.to_byte());
    hex::encode(out)
}

/// Test-only: a valid signature over `signing_digest(job_id, output_hash)` — the
/// result-attestation envelope the `submit` integration tests build.
#[cfg(test)]
pub(crate) fn sign_for_test(
    sk: &k256::ecdsa::SigningKey,
    job_id: &Uuid,
    output_hash: &str,
) -> String {
    sign_digest_for_test(sk, &signing_digest(job_id, output_hash))
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

    /// A second, distinct dev key whose address differs from `dev_key`.
    fn other_key() -> SigningKey {
        let bytes =
            hex::decode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    #[test]
    fn forged_signature_from_other_key_rejected() {
        // A perfectly valid signature over the right (job_id, output_hash) — but
        // produced by a different key — must not pass when the claimed address is
        // someone else's. Verification binds the signer, not just well-formedness.
        let job_id = Uuid::new_v4();
        let hash = "abc123";
        let forged = sign(&other_key(), &job_id, hash);
        assert_eq!(
            verify_signature(&job_id, hash, &dev_address(), &forged),
            Err(VerifyError::AddressMismatch)
        );
    }

    #[test]
    fn replayed_signature_for_other_job_rejected() {
        // A signature lifted from job A and replayed onto job B (same earner,
        // same output_hash) fails: the digest commits to job_id, so recovery
        // against B's digest yields a different key than the claimed signer.
        let sk = dev_key();
        let hash = "abc123";
        let job_a = Uuid::new_v4();
        let job_b = Uuid::new_v4();
        assert_ne!(job_a, job_b);
        let sig_for_a = sign(&sk, &job_a, hash);
        assert_eq!(
            verify_signature(&job_b, hash, &dev_address(), &sig_for_a),
            Err(VerifyError::AddressMismatch)
        );
        // Sanity: the same signature still verifies for its real job.
        assert_eq!(verify_signature(&job_a, hash, &dev_address(), &sig_for_a), Ok(()));
    }

    #[test]
    fn pre_tag_result_signature_is_rejected() {
        // The domain-tag cutover, from the verifier's side: a signature made over
        // the pre-v1 preimage keccak256(job_id || output_hash) — the form every
        // result signature committed to before `signing_digest` gained its
        // b"blackfield/result/v1" tag — must no longer validate. The coordinator
        // recomputes the tagged digest, recovers a different key, and rejects it.
        // The intended hard cutover, exactly like hello v1->v2; both the signer and
        // this verifier share the one constructor, so there is no mirror left behind.
        let sk = dev_key();
        let job_id = Uuid::new_v4();
        let hash = "abc123";

        let pre_tag_digest: [u8; 32] = {
            let mut h = Keccak256::new();
            h.update(job_id.as_bytes());
            h.update(hash.as_bytes());
            h.finalize().into()
        };
        let stale_sig = sign_digest_for_test(&sk, &pre_tag_digest);
        assert_eq!(
            verify_signature(&job_id, hash, &dev_address(), &stale_sig),
            Err(VerifyError::AddressMismatch),
        );

        // Sanity: the same signer over the CURRENT tagged digest still verifies, so
        // the rejection is the cutover, not a broken key or signer.
        let current_sig = sign(&sk, &job_id, hash);
        assert_eq!(verify_signature(&job_id, hash, &dev_address(), &current_sig), Ok(()));
    }

    /// 256-bit big-endian `a - b` (a >= b). Used to flip a low-S signature to its
    /// malleable high-S counterpart `s' = n - s` without pulling in scalar types.
    fn be_sub(a: &[u8; 32], b: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let mut d = a[i] as i16 - b[i] as i16 - borrow;
            if d < 0 {
                d += 256;
                borrow = 1;
            } else {
                borrow = 0;
            }
            out[i] = d as u8;
        }
        out
    }

    #[test]
    fn high_s_signature_rejected() {
        // secp256k1 group order n.
        const N: [u8; 32] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C,
            0xD0, 0x36, 0x41, 0x41,
        ];
        let sk = dev_key();
        let job_id = Uuid::new_v4();
        let hash = "abc123";

        // The canonical (low-S) signature from the earner's signer verifies.
        let low_hex = sign(&sk, &job_id, hash);
        assert_eq!(verify_signature(&job_id, hash, &dev_address(), &low_hex), Ok(()));

        // Malleate it to the high-S half: s' = n - s, same r. The recovery byte is
        // irrelevant — the low-S gate fires before recovery is even reached (and
        // k256's own recovery refuses high-S anyway), so we assert canonicity
        // directly rather than via recovery.
        let raw = hex::decode(&low_hex).unwrap();
        assert_eq!(raw.len(), 65);
        let s_high = be_sub(&N, &raw[32..64]);
        let mut malleated = Vec::with_capacity(65);
        malleated.extend_from_slice(&raw[0..32]); // r unchanged
        malleated.extend_from_slice(&s_high); // n - s
        malleated.push(raw[64]);

        // The constructed half is well-formed and genuinely high-S.
        let parsed = Signature::from_slice(&malleated[..64]).unwrap();
        assert!(parsed.normalize_s().is_some(), "constructed s must be the high-S half");

        // Same (job_id, output_hash, signer) as the accepted low-S signature, but
        // the malleable encoding is rejected specifically as non-canonical.
        assert_eq!(
            verify_signature(&job_id, hash, &dev_address(), &hex::encode(&malleated)),
            Err(VerifyError::NonCanonicalSignature)
        );
    }

    /// A `Hello` signed by the key whose address it claims verifies — the
    /// registration analogue of `valid_signature_verifies`.
    #[test]
    fn valid_hello_signature_verifies() {
        let sk = dev_key();
        let addr = dev_address();
        let supported = [JobKind::Terrain, JobKind::DiffusionTile];
        let n: &[u8] = b"chal";
        let sig = sign_digest_for_test(&sk, &hello_digest(&addr, "RTX 4090", 24, &supported, n));
        assert_eq!(
            verify_hello_signature(&addr, "RTX 4090", 24, &supported, n, &sig),
            Ok(())
        );
    }

    #[test]
    fn forged_hello_from_other_key_rejected() {
        // An attacker signs a Hello *claiming* dev's address but with its own key.
        // Recovery against the claimed-address digest yields the attacker's
        // address, not dev's, so key possession fails — an identity the
        // capability filter / fault ledger / `/stats` totals trust can't be
        // spoofed onto by someone who doesn't hold its key.
        let attacker = other_key();
        let victim = dev_address();
        let supported = [JobKind::Terrain];
        let n: &[u8] = b"chal";
        let sig =
            sign_digest_for_test(&attacker, &hello_digest(&victim, "RTX 4090", 24, &supported, n));
        assert_eq!(
            verify_hello_signature(&victim, "RTX 4090", 24, &supported, n, &sig),
            Err(VerifyError::AddressMismatch)
        );
    }

    #[test]
    fn tampered_hello_capabilities_or_nonce_rejected() {
        // The signature binds the WHOLE advertised Hello AND the challenge nonce:
        // a signature over (vram=24, [Terrain], nonce="chal") fails against an
        // inflated vram, a swapped capability set, OR a different nonce — so a
        // captured signature can't be reattached to a better profile or replayed
        // against a fresh challenge.
        let sk = dev_key();
        let addr = dev_address();
        let supported = [JobKind::Terrain];
        let n: &[u8] = b"chal";
        let sig = sign_digest_for_test(&sk, &hello_digest(&addr, "RTX 4090", 24, &supported, n));
        assert!(
            verify_hello_signature(&addr, "RTX 4090", 99, &supported, n, &sig).is_err(),
            "inflated vram must not verify against a vram=24 signature"
        );
        assert!(
            verify_hello_signature(&addr, "RTX 4090", 24, &[JobKind::Optimization], n, &sig)
                .is_err(),
            "swapped capability set must not verify"
        );
        assert!(
            verify_hello_signature(&addr, "RTX 4090", 24, &supported, b"other", &sig).is_err(),
            "a different challenge nonce must not verify (anti-replay)"
        );
        // Sanity: the untampered Hello over its own nonce still verifies.
        assert_eq!(
            verify_hello_signature(&addr, "RTX 4090", 24, &supported, n, &sig),
            Ok(())
        );
    }

    #[test]
    fn canonical_earner_address_folds_to_the_recovered_address() {
        // The recovered address is the canonical lowercase identity.
        let recovered = dev_address();
        // An EIP-55-style mixed-case claim is the same address, different case.
        let upper = format!("0x{}", recovered[2..].to_ascii_uppercase());
        assert_ne!(upper, recovered, "the uppercased variant differs only by case");
        // The gate accepts the mixed-case claim (case-insensitive match)...
        let sk = dev_key();
        let n: &[u8] = b"chal";
        let sig = sign_digest_for_test(&sk, &hello_digest(&upper, "RTX 4090", 24, &[], n));
        assert_eq!(verify_hello_signature(&upper, "RTX 4090", 24, &[], n, &sig), Ok(()));
        // ...and normalizing that accepted claim yields exactly the recovered signer
        // address, so every identity boundary keys on the key, not the chosen case.
        assert_eq!(canonical_earner_address(&upper), recovered);
        assert_eq!(canonical_earner_address(&recovered), recovered);
    }
}
