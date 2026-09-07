use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chrono::Utc;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::SignatureEncoding;
use rsa::signature::Signer;
use rsa::RsaPrivateKey;
use sha1::Sha1;
use std::fs;
use tracing::debug;

use crate::config::CloudfrontConfig;

/// A pre-configured CloudFront signer. If no key is configured, URLs come back
/// unsigned, which is what makes the MinIO-backed local setup work.
pub struct CloudfrontService {
    cf_url: String,
    expiry_seconds: i64,
    cookie_domain: String,
    /// Pre-loaded private key, or None if signing is disabled.
    signing_key: Option<(String, RsaPrivateKey)>,
}

/// The three cookies CloudFront looks for on a custom-policy request.
#[derive(Debug, Clone)]
pub struct SignedCookies {
    pub policy: String,
    pub signature: String,
    pub key_pair_id: String,
    /// Path the cookies should be scoped to, so one video's cookies do not
    /// authorise another's.
    pub path: String,
    pub expires_at: i64,
    pub domain: Option<String>,
}

impl CloudfrontService {
    pub fn new(cfg: &CloudfrontConfig) -> Result<Self> {
        let signing_key = if cfg.keypair_id.is_empty() {
            None
        } else {
            let key_text = cfg.private_key.trim().to_string();
            let pem = if !key_text.is_empty() {
                debug!("CloudFront: loading private key from config text");
                rebuild_pem(&key_text)
            } else if !cfg.private_key_file.is_empty() {
                debug!("CloudFront: loading private key from file {}", cfg.private_key_file);
                fs::read_to_string(&cfg.private_key_file)
                    .with_context(|| format!("Failed to read CF key file: {}", cfg.private_key_file))?
            } else {
                bail!("CloudFront keypairId is set but no privateKey or privateKeyFile provided");
            };
            let private_key = RsaPrivateKey::from_pkcs8_pem(&pem)
                .or_else(|_| {
                    use rsa::pkcs1::DecodeRsaPrivateKey;
                    RsaPrivateKey::from_pkcs1_pem(&pem)
                })
                .context("Failed to parse CloudFront private key")?;
            Some((cfg.keypair_id.clone(), private_key))
        };

        Ok(Self {
            cf_url: cfg.url.trim_end_matches('/').to_string(),
            expiry_seconds: cfg.expiry_seconds,
            cookie_domain: cfg.cookie_domain.trim().to_string(),
            signing_key,
        })
    }

    /// A canned-policy signed URL for one object. Correct for a progressive MP4,
    /// which is a single request; see [`Self::get_signed_cookies`] for HLS, which
    /// is not.
    pub fn get_signed_url(&self, s3_path: &str) -> Result<String> {
        let full_url = format!("{}/{}", self.cf_url, s3_path);
        let Some((key_pair_id, private_key)) = &self.signing_key else {
            return Ok(full_url);
        };

        let expires_at = Utc::now().timestamp() + self.expiry_seconds;
        let policy = canned_policy(&full_url, expires_at);
        let encoded_sig = cf_base64(BASE64.encode(sign(private_key, policy.as_bytes())));

        Ok(format!(
            "{}?Expires={}&Signature={}&Key-Pair-Id={}",
            full_url, expires_at, encoded_sig, key_pair_id
        ))
    }

    /// Signed cookies covering everything under `prefix`.
    ///
    /// This is what HLS needs and a signed URL cannot give it. A signed URL
    /// authorises exactly the request that carries its query string; an HLS player
    /// fetches the manifest and then issues its own requests for each segment,
    /// carrying no query string at all, and every one of those would be a 403.
    /// A custom policy over a wildcard resource, delivered as cookies, authorises
    /// the manifest and its segments together.
    ///
    /// Returns `None` when signing is disabled, which is the local/MinIO case: the
    /// objects are public there, so there is nothing to authorise.
    pub fn get_signed_cookies(&self, prefix: &str) -> Result<Option<SignedCookies>> {
        let Some((key_pair_id, private_key)) = &self.signing_key else {
            return Ok(None);
        };

        let expires_at = Utc::now().timestamp() + self.expiry_seconds;
        let resource = format!("{}/{}*", self.cf_url, prefix);
        let policy = custom_policy(&resource, expires_at);

        Ok(Some(SignedCookies {
            policy: cf_base64(BASE64.encode(policy.as_bytes())),
            signature: cf_base64(BASE64.encode(sign(private_key, policy.as_bytes()))),
            key_pair_id: key_pair_id.clone(),
            // Scoped to the prefix so the cookies a viewer picks up for one video do
            // not silently authorise the whole distribution.
            path: format!("/{}", prefix),
            expires_at,
            domain: (!self.cookie_domain.is_empty()).then(|| self.cookie_domain.clone()),
        }))
    }

    /// The unsigned URL for a path — used for an HLS manifest, whose authorisation
    /// travels in the cookies rather than the query string.
    pub fn get_url(&self, s3_path: &str) -> String {
        format!("{}/{}", self.cf_url, s3_path)
    }

    pub fn is_signing(&self) -> bool {
        self.signing_key.is_some()
    }
}

/// RSA-SHA1 PKCS#1 v1.5, which is what CloudFront requires. RSA-PSS or SHA-256 will
/// not verify. PKCS1v15 is deterministic, so no RNG is needed.
fn sign(private_key: &RsaPrivateKey, message: &[u8]) -> Vec<u8> {
    let signing_key: SigningKey<Sha1> = SigningKey::new(private_key.clone());
    Signer::sign(&signing_key, message).to_bytes().to_vec()
}

fn canned_policy(resource: &str, expires_at: i64) -> String {
    format!(
        r#"{{"Statement":[{{"Resource":"{}","Condition":{{"DateLessThan":{{"AWS:EpochTime":{}}}}}}}]}}"#,
        resource, expires_at
    )
}

/// Identical in shape to the canned policy, but the resource carries a `*`. The
/// distinction that matters is in delivery, not structure: a canned policy can ride
/// in the query string, a wildcard one must be sent whole, which is why it goes in
/// a cookie.
fn custom_policy(resource: &str, expires_at: i64) -> String {
    canned_policy(resource, expires_at)
}

/// Re-wrap a raw base64 private key string into a PKCS#8 PEM block.
fn rebuild_pem(raw_b64: &str) -> String {
    let wrapped: String = raw_b64
        .chars()
        .collect::<Vec<_>>()
        .chunks(64)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
        wrapped
    )
}

/// CloudFront's modified base64 alphabet, per AWS docs: `+`→`-`, `/`→`~`, `=`→`_`.
/// Note this is NOT standard URL-safe base64 — the `/` and `=` mappings are the
/// opposite of what you would expect.
fn cf_base64(s: String) -> String {
    s.replace('+', "-").replace('/', "~").replace('=', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsigned() -> CloudfrontService {
        CloudfrontService::new(&CloudfrontConfig {
            url: "https://cdn.example.com/".into(),
            ..Default::default()
        })
        .unwrap()
    }

    /// Without a keypair the service still has to produce usable URLs — that is the
    /// local MinIO setup, and it is what makes the transcode endpoint testable.
    #[test]
    fn unsigned_urls_pass_through_and_the_trailing_slash_is_trimmed() {
        let cf = unsigned();
        assert_eq!(cf.get_signed_url("clip-1.mp4").unwrap(), "https://cdn.example.com/clip-1.mp4");
        assert!(!cf.is_signing());
        assert!(cf.get_signed_cookies("clip-1-hls").unwrap().is_none());
    }

    #[test]
    fn the_cloudfront_alphabet_is_not_url_safe_base64() {
        assert_eq!(cf_base64("a+b/c=".to_string()), "a-b~c_");
    }

    /// The wildcard is the whole point of the cookie path: it has to cover the
    /// segments the player will ask for, not just the manifest it was handed.
    #[test]
    fn a_custom_policy_covers_the_whole_prefix() {
        let policy = custom_policy("https://cdn.example.com/clip-1-hls*", 42);
        assert!(policy.contains(r#""Resource":"https://cdn.example.com/clip-1-hls*""#));
        assert!(policy.contains(r#""AWS:EpochTime":42"#));
    }
}
