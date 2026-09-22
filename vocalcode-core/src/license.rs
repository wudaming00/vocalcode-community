//! Portable licensing: verify a server-signed activation receipt locally, and
//! track the free trial. The receipt is an Ed25519-signed snapshot of a server
//! validation. It is device-bound and short-lived, so a revoked or suspended
//! licence is eventually observed when the app refreshes it online.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::error::{Result, VocalCodeError};

pub const PRODUCT: &str = "vocalcode";
pub const TRIAL_DAYS: u64 = 30;
pub const TRIAL_RECEIPT_VERSION: u32 = 1;

/// Claims the activation server signs. The token is
/// `base64(claims_json) + "." + base64(ed25519_sig_over_claims_json)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LicenseClaims {
    /// Opaque licence id. This is an id, never the buyer's reusable secret key.
    pub key: String,
    /// Device fingerprint this token is bound to.
    pub device: String,
    /// Product id — must equal [`PRODUCT`].
    pub product: String,
    /// Highest major version this license is entitled to (paid-upgrade gate).
    pub max_version: u32,
    /// Server time when live entitlement validation produced this receipt.
    pub iat: u64,
    /// Receipt expiry (unix secs). Receipts are never perpetual.
    pub exp: u64,
}

/// Server-authenticated free-trial epoch. Unlike the retired unsigned local
/// timestamp, deleting this cached token cannot create a new trial: the server
/// reissues a receipt for the same device-owned `iat`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrialClaims {
    pub kind: String,
    pub version: u32,
    pub device: String,
    pub product: String,
    pub iat: u64,
    pub exp: u64,
    /// Server time of the latest live lookup for this immutable trial epoch.
    pub checked_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseStatus {
    /// Valid signed license bound to this device.
    Licensed { max_version: u32 },
    /// Still within the free trial.
    Trial { days_left: u32 },
    /// No authenticated trial cache exists yet. One bounded online provisioning
    /// request is required; this is distinct from a genuinely elapsed trial.
    TrialSetupRequired,
    /// Trial elapsed and no valid license.
    Expired,
    /// A token was present but failed verification.
    Invalid(String),
}

/// Decode the base64-encoded Ed25519 public key embedded by the release build.
/// Keeping this next to [`verify_token`] avoids duplicating a subtly different
/// decoder in each activation frontend.
pub fn verifying_key_from_base64(encoded: &str) -> Result<[u8; 32]> {
    let bytes = B64
        .decode(encoded.trim())
        .map_err(|e| VocalCodeError::License(format!("public key base64: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| VocalCodeError::License("public key must be exactly 32 bytes".into()))
}

/// Verify a server-signed activation receipt against the embedded public key and
/// this device. The activation client caches this token and calls this function
/// on every launch; no unsigned on-disk field is an authority.
pub fn verify_token(
    token: &str,
    verifying_key: &[u8; 32],
    device: &str,
    now_unix: u64,
) -> Result<LicenseClaims> {
    let claims_bytes = verify_signed_payload(token, verifying_key)?;
    let claims: LicenseClaims = serde_json::from_slice(&claims_bytes)
        .map_err(|e| VocalCodeError::License(format!("claims json: {e}")))?;

    validate_device_and_product(&claims.device, &claims.product, device)?;
    if claims.key.is_empty() {
        return Err(VocalCodeError::License(
            "receipt carries no licence id".into(),
        ));
    }
    if claims.max_version == 0 {
        return Err(VocalCodeError::License(
            "receipt carries no product entitlement".into(),
        ));
    }
    if claims.iat == 0
        || claims.exp <= claims.iat
        || claims.exp.saturating_sub(claims.iat) > 90 * 86_400
    {
        return Err(VocalCodeError::License(
            "receipt has an invalid issuance time or expiry".into(),
        ));
    }
    if now_unix != 0 {
        if claims.iat.saturating_sub(now_unix) > 300 {
            return Err(VocalCodeError::License(
                "system clock predates receipt issuance".into(),
            ));
        }
        if now_unix >= claims.exp {
            return Err(VocalCodeError::License(
                "token expired — re-validate online".into(),
            ));
        }
    }
    Ok(claims)
}

fn verify_signed_payload(token: &str, verifying_key: &[u8; 32]) -> Result<Vec<u8>> {
    let (claims_b64, sig_b64) = token
        .split_once('.')
        .ok_or_else(|| VocalCodeError::License("malformed token (no '.')".into()))?;

    let claims_bytes = B64
        .decode(claims_b64)
        .map_err(|e| VocalCodeError::License(format!("claims base64: {e}")))?;
    let sig_bytes = B64
        .decode(sig_b64)
        .map_err(|e| VocalCodeError::License(format!("sig base64: {e}")))?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| VocalCodeError::License("signature not 64 bytes".into()))?;

    let vk = VerifyingKey::from_bytes(verifying_key)
        .map_err(|e| VocalCodeError::License(format!("bad public key: {e}")))?;
    vk.verify_strict(&claims_bytes, &Signature::from_bytes(&sig_arr))
        .map_err(|_| VocalCodeError::License("signature verification failed".into()))?;

    Ok(claims_bytes)
}

fn validate_device_and_product(claim_device: &str, product: &str, device: &str) -> Result<()> {
    let device = device.trim();
    if device.is_empty() || device == "vocalcode-unknown-device" {
        return Err(VocalCodeError::License(
            "a stable machine fingerprint is unavailable".into(),
        ));
    }
    if product != PRODUCT {
        return Err(VocalCodeError::License(format!(
            "token is for product '{}', not '{PRODUCT}'",
            product
        )));
    }
    if claim_device != device {
        return Err(VocalCodeError::License(
            "token is bound to a different device".into(),
        ));
    }
    Ok(())
}

/// Verify the server-owned trial epoch against the release public key, exact
/// machine, fixed 30-day duration and local clock. Both tampering and material
/// clock rollback fail closed.
pub fn verify_trial_token(
    token: &str,
    verifying_key: &[u8; 32],
    device: &str,
    now_unix: u64,
) -> Result<TrialClaims> {
    let claims_bytes = verify_signed_payload(token, verifying_key)?;
    let claims: TrialClaims = serde_json::from_slice(&claims_bytes)
        .map_err(|e| VocalCodeError::License(format!("trial claims json: {e}")))?;
    validate_device_and_product(&claims.device, &claims.product, device)?;
    let expected_exp = claims
        .iat
        .checked_add(TRIAL_DAYS * 86_400)
        .ok_or_else(|| VocalCodeError::License("trial expiry overflows".into()))?;
    if claims.kind != "trial"
        || claims.version != TRIAL_RECEIPT_VERSION
        || claims.iat == 0
        || claims.exp != expected_exp
        || claims.checked_at < claims.iat
    {
        return Err(VocalCodeError::License("invalid trial claims".into()));
    }
    // Time zero is the authenticated-metadata mode used only to decide whether
    // an expired disk receipt is structurally genuine and therefore must not
    // be replaced with a fresh server epoch. Authorization always supplies the
    // real clock and takes both checks below.
    if now_unix != 0 {
        if claims.checked_at.saturating_sub(now_unix) > 300 {
            return Err(VocalCodeError::License(
                "system clock predates the latest trial check".into(),
            ));
        }
        if now_unix >= claims.exp {
            return Err(VocalCodeError::License("trial expired".into()));
        }
    }
    Ok(claims)
}

/// Days remaining in the trial (0 once elapsed).
pub fn trial_days_left(first_run_unix: u64, now_unix: u64, trial_days: u64) -> u32 {
    // Saturating subtraction granted a full fresh trial whenever the system
    // clock was moved to before the recorded first run. A rollback is not a
    // trustworthy basis for extending an offline entitlement, so fail closed.
    // Permit a small NTP/RTC correction around first launch; a material
    // rollback still cannot extend the entitlement.
    if first_run_unix.saturating_sub(now_unix) > 300 {
        return 0;
    }
    let elapsed_days = now_unix.saturating_sub(first_run_unix) / 86_400;
    trial_days.saturating_sub(elapsed_days) as u32
}

/// Combine an optional signed token with the trial clock into one status.
pub fn evaluate(
    token: Option<&str>,
    verifying_key: &[u8; 32],
    device: &str,
    first_run_unix: u64,
    now_unix: u64,
) -> LicenseStatus {
    if let Some(tok) = token {
        return match verify_token(tok, verifying_key, device, now_unix) {
            Ok(c) => LicenseStatus::Licensed {
                max_version: c.max_version,
            },
            Err(e) => LicenseStatus::Invalid(e.to_string()),
        };
    }
    let left = trial_days_left(first_run_unix, now_unix, TRIAL_DAYS);
    if left > 0 {
        LicenseStatus::Trial { days_left: left }
    } else {
        LicenseStatus::Expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn make_token(sk: &SigningKey, claims: &LicenseClaims) -> String {
        let json = serde_json::to_vec(claims).unwrap();
        let sig = sk.sign(&json);
        format!("{}.{}", B64.encode(&json), B64.encode(sig.to_bytes()))
    }

    fn make_trial_token(sk: &SigningKey, claims: &TrialClaims) -> String {
        let json = serde_json::to_vec(claims).unwrap();
        let sig = sk.sign(&json);
        format!("{}.{}", B64.encode(&json), B64.encode(sig.to_bytes()))
    }

    fn claims(device: &str, exp: u64) -> LicenseClaims {
        LicenseClaims {
            key: "LS-KEY-123".into(),
            device: device.into(),
            product: PRODUCT.into(),
            max_version: 1,
            iat: 100,
            exp,
        }
    }

    #[test]
    fn valid_token_verifies() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        let tok = make_token(&sk, &claims("dev-abc", 2_000));
        let out = verify_token(&tok, &vk, "dev-abc", 1_000).unwrap();
        assert_eq!(out.max_version, 1);
    }

    #[test]
    fn tampered_claims_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        let tok = make_token(&sk, &claims("dev-abc", 2_000));
        // Flip a character in the claims segment.
        let mut parts: Vec<&str> = tok.split('.').collect();
        let mut c = parts[0].to_string();
        c.replace_range(0..1, if c.starts_with('A') { "B" } else { "A" });
        let bad = format!("{}.{}", c, parts.pop().unwrap());
        assert!(verify_token(&bad, &vk, "dev-abc", 1_000).is_err());
    }

    #[test]
    fn wrong_device_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        let tok = make_token(&sk, &claims("dev-abc", 2_000));
        assert!(verify_token(&tok, &vk, "OTHER-device", 1_000).is_err());
    }

    #[test]
    fn expired_token_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        let tok = make_token(&sk, &claims("dev-abc", 500));
        assert!(verify_token(&tok, &vk, "dev-abc", 1_000).is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let other_vk = SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .to_bytes();
        let tok = make_token(&sk, &claims("dev-abc", 2_000));
        assert!(verify_token(&tok, &other_vk, "dev-abc", 1_000).is_err());
    }

    #[test]
    fn trial_countdown() {
        assert_eq!(trial_days_left(0, 0, 30), 30);
        assert_eq!(trial_days_left(0, 10 * 86_400, 30), 20);
        assert_eq!(trial_days_left(0, 40 * 86_400, 30), 0);
        assert_eq!(trial_days_left(10 * 86_400, 9 * 86_400, 30), 0);
    }

    #[test]
    fn perpetual_receipts_are_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        let tok = make_token(&sk, &claims("dev-abc", 0));
        assert!(verify_token(&tok, &vk, "dev-abc", 1_000).is_err());
    }

    #[test]
    fn shared_unknown_fingerprint_is_never_a_device() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        let tok = make_token(&sk, &claims("vocalcode-unknown-device", 2_000));
        assert!(verify_token(&tok, &vk, "vocalcode-unknown-device", 1_000).is_err());
        let blank = make_token(&sk, &claims("   ", 2_000));
        assert!(verify_token(&blank, &vk, "   ", 1_000).is_err());
    }

    #[test]
    fn public_key_decoder_requires_exactly_one_ed25519_key() {
        let key = B64.encode([7u8; 32]);
        assert_eq!(verifying_key_from_base64(&key).unwrap(), [7u8; 32]);
        assert!(verifying_key_from_base64("not base64").is_err());
        assert!(verifying_key_from_base64(&B64.encode([7u8; 31])).is_err());
    }

    #[test]
    fn signed_trial_is_machine_bound_fixed_length_and_clock_safe() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        let iat = 1_800_000_000;
        let claims = TrialClaims {
            kind: "trial".into(),
            version: TRIAL_RECEIPT_VERSION,
            device: "dev-abc".into(),
            product: PRODUCT.into(),
            iat,
            exp: iat + TRIAL_DAYS * 86_400,
            checked_at: iat,
        };
        let token = make_trial_token(&sk, &claims);
        assert_eq!(
            verify_trial_token(&token, &vk, "dev-abc", iat + 1).unwrap(),
            claims
        );
        assert!(verify_trial_token(&token, &vk, "other", iat + 1).is_err());
        assert!(verify_trial_token(&token, &vk, "dev-abc", iat - 301).is_err());
        assert!(verify_trial_token(&token, &vk, "dev-abc", claims.exp).is_err());

        let mut wrong_duration = claims.clone();
        wrong_duration.exp += 1;
        assert!(verify_trial_token(
            &make_trial_token(&sk, &wrong_duration),
            &vk,
            "dev-abc",
            iat + 1,
        )
        .is_err());
        let mut wrong_kind = claims;
        wrong_kind.kind = "paid".into();
        assert!(
            verify_trial_token(&make_trial_token(&sk, &wrong_kind), &vk, "dev-abc", iat + 1,)
                .is_err()
        );
    }
}
