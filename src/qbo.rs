//! QuickBooks Online access for jobs.
//!
//! The chat application keeps each company's QuickBooks connection in the
//! `company` row: the realm id, plus OAuth access and refresh tokens stored
//! AES-256-GCM encrypted (base64 of `iv(12) | tag(16) | ciphertext`, the
//! format of the app's `cryptoHelpers.server.ts`). Before a job starts,
//! [`QboBroker::access`] makes sure the asking company's access token stays
//! valid for the whole job, refreshing it (and writing the rotated tokens
//! back, exactly as the app does) when it would not.
//!
//! Jobs only ever receive that one short-lived access token and the realm id,
//! so what a job can reach in QuickBooks is structurally limited to its own
//! company. The AES key, the app's client secret, the refresh token and the
//! writable database login all stay in this process.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use sqlx::{MySqlPool, Row};
use tracing::{info, warn};

use crate::worker::chain;

/// Intuit's OAuth token endpoint (the same for sandbox and production).
const TOKEN_URL: &str = "https://oauth.platform.intuit.com/oauth2/v1/tokens/bearer";

/// How long a refresh round trip to Intuit may take.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// AES-GCM tag length used by the chat application.
const TAG_LEN: usize = 16;

/// Credentials shorter than this are treated as unset, as in the app.
const MIN_CREDENTIAL_LEN: usize = 10;

/// Which Intuit environment the tokens belong to (`QBO_ENV`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QboEnvironment {
    /// `sandbox-quickbooks.api.intuit.com`. The app's default too.
    #[default]
    Sandbox,
    /// `quickbooks.api.intuit.com`.
    Production,
}

impl QboEnvironment {
    /// Parses `QBO_ENV`: `production` means production, anything else sandbox,
    /// mirroring the chat application.
    #[must_use]
    pub fn from_env_value(raw: &str) -> Self {
        if raw.trim() == "production" {
            Self::Production
        } else {
            Self::Sandbox
        }
    }

    /// Base URL of the accounting API, without the company path.
    #[must_use]
    pub const fn api_base(self) -> &'static str {
        match self {
            Self::Sandbox => "https://sandbox-quickbooks.api.intuit.com",
            Self::Production => "https://quickbooks.api.intuit.com",
        }
    }
}

/// Settings for [`QboBroker`], read from the runner's environment.
#[derive(Clone, Default)]
pub struct QboConfig {
    /// `AES_KEY`: 64 hex characters, the chat application's key.
    pub aes_key_hex: String,
    /// `QBO_CLIENT_ID` / `QBO_CLIENT_SECRET`: the shared Intuit app, used
    /// for companies that did not register their own.
    pub client_id: Option<String>,
    /// See `client_id`.
    pub client_secret: Option<String>,
    /// `QBO_ENV`.
    pub environment: QboEnvironment,
}

impl fmt::Debug for QboConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QboConfig")
            .field("client_id", &self.client_id.as_ref().map(|_| "<set>"))
            .field("environment", &self.environment)
            .finish_non_exhaustive()
    }
}

/// Errors from [`QboBroker`].
#[derive(Debug, thiserror::Error)]
pub enum QboError {
    /// The configuration cannot work.
    #[error("{0}")]
    Config(String),
    /// A stored value could not be decrypted.
    #[error("cannot decrypt {0}")]
    Decrypt(&'static str),
    /// The database driver reported an error.
    #[error("database error")]
    Database(#[from] sqlx::Error),
    /// Talking to Intuit failed.
    #[error("{0}")]
    Http(String),
}

/// What a job gets to know about QuickBooks.
#[derive(Clone, PartialEq, Eq)]
pub enum QboJobAccess {
    /// Connected: the job may call the API with this token.
    Connected {
        /// `company.qbo_realm_id`.
        realm_id: String,
        /// A bearer token valid for at least the job's lifetime.
        access_token: String,
        /// Base URL of the accounting API for this environment.
        api_base: String,
    },
    /// Not usable for this company; the reason is for the job's system prompt.
    Unavailable(String),
}

impl fmt::Debug for QboJobAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected { realm_id, .. } => f
                .debug_struct("Connected")
                .field("realm_id", realm_id)
                .finish_non_exhaustive(),
            Self::Unavailable(reason) => f.debug_tuple("Unavailable").field(reason).finish(),
        }
    }
}

/// Hands out per-job QuickBooks access tokens. Cheap to clone.
#[derive(Clone)]
pub struct QboBroker {
    inner: Arc<Inner>,
}

struct Inner {
    pool: MySqlPool,
    key: LessSafeKey,
    app_credentials: Option<(String, String)>,
    environment: QboEnvironment,
    http: reqwest::Client,
    valid_for: Duration,
}

impl fmt::Debug for QboBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QboBroker")
            .field("environment", &self.inner.environment)
            .field("valid_for", &self.inner.valid_for)
            .finish_non_exhaustive()
    }
}

/// The `company` columns this module reads.
struct CompanyRow {
    enabled: bool,
    realm_id: Option<String>,
    access_token: Option<Vec<u8>>,
    refresh_token: Option<Vec<u8>>,
    access_expires: Option<i64>,
    refresh_expires: Option<i64>,
    client_id: Option<Vec<u8>>,
    client_secret: Option<Vec<u8>>,
}

/// Intuit's token response.
#[derive(Debug, Deserialize)]
struct TokenSet {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    x_refresh_token_expires_in: i64,
}

impl QboBroker {
    /// Builds a broker that writes refreshed tokens through `pool` (the
    /// runner's writable login) and hands out tokens valid for `valid_for`.
    pub fn new(pool: MySqlPool, config: &QboConfig, valid_for: Duration) -> Result<Self, QboError> {
        let key = parse_key(&config.aes_key_hex)?;
        let app_credentials = match (&config.client_id, &config.client_secret) {
            (Some(id), Some(secret))
                if id.trim().len() >= MIN_CREDENTIAL_LEN
                    && secret.trim().len() >= MIN_CREDENTIAL_LEN =>
            {
                Some((id.trim().to_owned(), secret.trim().to_owned()))
            }
            _ => None,
        };
        install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|err| QboError::Http(format!("cannot build HTTP client: {err}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                pool,
                key,
                app_credentials,
                environment: config.environment,
                http,
                valid_for,
            }),
        })
    }

    /// Access for a job of `company_id`. Never fails: problems are logged and
    /// reported as [`QboJobAccess::Unavailable`], so a QuickBooks outage
    /// only costs the job its QuickBooks tools.
    pub async fn access(&self, company_id: i64) -> QboJobAccess {
        match self.try_access(company_id).await {
            Ok(access) => access,
            Err(err) => {
                warn!(company_id, error = %chain(&err), "could not get QuickBooks access");
                QboJobAccess::Unavailable(
                    "QuickBooks could not be reached right now; try again later.".to_owned(),
                )
            }
        }
    }

    async fn try_access(&self, company_id: i64) -> Result<QboJobAccess, QboError> {
        let inner = &self.inner;
        // The row lock is held across the refresh call so this runner and the
        // chat application (which reads the row FOR UPDATE before refreshing)
        // never both spend the same refresh token.
        let mut tx = inner.pool.begin().await?;
        let row = sqlx::query(
            "SELECT quickbooks_enabled, qbo_realm_id, qbo_access_token, qbo_refresh_token, \
                    qbo_access_expires, qbo_refresh_expires, qbo_client_id, qbo_client_secret \
               FROM company WHERE id = ? FOR UPDATE",
        )
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| -> Result<CompanyRow, sqlx::Error> {
            Ok(CompanyRow {
                enabled: row.try_get("quickbooks_enabled")?,
                realm_id: row.try_get("qbo_realm_id")?,
                access_token: row.try_get("qbo_access_token")?,
                refresh_token: row.try_get("qbo_refresh_token")?,
                access_expires: row.try_get("qbo_access_expires")?,
                refresh_expires: row.try_get("qbo_refresh_expires")?,
                client_id: row.try_get("qbo_client_id")?,
                client_secret: row.try_get("qbo_client_secret")?,
            })
        })
        .transpose()?;

        let not_connected = || {
            QboJobAccess::Unavailable("QuickBooks is not connected for this company.".to_owned())
        };
        let Some(row) = row else {
            return Ok(not_connected());
        };
        if !row.enabled {
            return Ok(QboJobAccess::Unavailable(
                "QuickBooks is turned off for this company.".to_owned(),
            ));
        }
        let realm_id = row.realm_id.as_deref().map(str::trim).unwrap_or_default();
        let (Some(access_blob), Some(refresh_blob), Some(access_expires), Some(refresh_expires)) = (
            &row.access_token,
            &row.refresh_token,
            row.access_expires,
            row.refresh_expires,
        ) else {
            return Ok(not_connected());
        };
        if realm_id.is_empty() {
            return Ok(not_connected());
        }

        let now = unix_now();
        let needed = i64::try_from(inner.valid_for.as_secs()).unwrap_or(i64::MAX);
        let connected = |access_token| QboJobAccess::Connected {
            realm_id: realm_id.to_owned(),
            access_token,
            api_base: inner.environment.api_base().to_owned(),
        };
        if access_expires - now >= needed {
            let token =
                decrypt(&inner.key, access_blob).ok_or(QboError::Decrypt("access token"))?;
            tx.commit().await?;
            return Ok(connected(token));
        }
        if refresh_expires <= now + 60 {
            return Ok(QboJobAccess::Unavailable(
                "The QuickBooks connection has expired; an admin needs to reconnect QuickBooks."
                    .to_owned(),
            ));
        }

        let refresh_token =
            decrypt(&inner.key, refresh_blob).ok_or(QboError::Decrypt("refresh token"))?;
        let (client_id, client_secret) = self.client_credentials(&row)?;
        let tokens = self
            .refresh(&client_id, &client_secret, &refresh_token)
            .await?;
        let now = unix_now();
        sqlx::query(
            "UPDATE company SET qbo_access_token = ?, qbo_refresh_token = ?, \
                    qbo_access_expires = ?, qbo_refresh_expires = ? WHERE id = ?",
        )
        .bind(encrypt(&inner.key, &tokens.access_token)?)
        .bind(encrypt(&inner.key, &tokens.refresh_token)?)
        .bind(now + tokens.expires_in)
        .bind(now + tokens.x_refresh_token_expires_in)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        info!(company_id, "refreshed the QuickBooks access token");
        Ok(connected(tokens.access_token))
    }

    /// The company's own Intuit app if it registered one, else the shared one.
    fn client_credentials(&self, row: &CompanyRow) -> Result<(String, String), QboError> {
        let own = |blob: &Option<Vec<u8>>| {
            blob.as_deref()
                .and_then(|blob| decrypt(&self.inner.key, blob))
                .map(|value| value.trim().to_owned())
                .filter(|value| value.len() >= MIN_CREDENTIAL_LEN)
        };
        if let (Some(id), Some(secret)) = (own(&row.client_id), own(&row.client_secret)) {
            return Ok((id, secret));
        }
        self.inner.app_credentials.clone().ok_or_else(|| {
            QboError::Config(
                "no QuickBooks app credentials: set QBO_CLIENT_ID and QBO_CLIENT_SECRET".to_owned(),
            )
        })
    }

    async fn refresh(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<TokenSet, QboError> {
        let response = self
            .inner
            .http
            .post(TOKEN_URL)
            .basic_auth(client_id, Some(client_secret))
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "grant_type=refresh_token&refresh_token={}",
                percent_encode(refresh_token)
            ))
            .send()
            .await
            .map_err(|err| QboError::Http(format!("QuickBooks token refresh: {err}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| QboError::Http(format!("QuickBooks token refresh: {err}")))?;
        if !status.is_success() {
            return Err(QboError::Http(format!(
                "QuickBooks token refresh: HTTP {status}: {body:.300}"
            )));
        }
        serde_json::from_str(&body)
            .map_err(|err| QboError::Http(format!("QuickBooks token refresh: bad response: {err}")))
    }
}

/// reqwest is built without a default TLS crypto provider; use ring, which
/// sqlx's rustls already links (a no-op when one is installed).
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn parse_key(hex: &str) -> Result<LessSafeKey, QboError> {
    let hex = hex.trim();
    let bytes: Option<Vec<u8>> = (hex.len() == 64)
        .then(|| {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
                .collect()
        })
        .flatten();
    let bytes = bytes.ok_or_else(|| {
        QboError::Config("AES_KEY must be 64 hex characters (a 256-bit key)".to_owned())
    })?;
    UnboundKey::new(&AES_256_GCM, &bytes)
        .map(LessSafeKey::new)
        .map_err(|_| QboError::Config("AES_KEY is not a valid AES-256 key".to_owned()))
}

/// Decrypts a value written by the chat application's `encrypt`. The column
/// holds the base64 text as bytes.
fn decrypt(key: &LessSafeKey, stored: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stored).ok()?.trim();
    let bytes = BASE64.decode(text).ok()?;
    if bytes.len() < NONCE_LEN + TAG_LEN {
        return None;
    }
    let (iv, rest) = bytes.split_at(NONCE_LEN);
    let (tag, ciphertext) = rest.split_at(TAG_LEN);
    // ring wants `ciphertext | tag`.
    let mut in_out = [ciphertext, tag].concat();
    let nonce = Nonce::try_assume_unique_for_key(iv).ok()?;
    let plain = key.open_in_place(nonce, Aad::empty(), &mut in_out).ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

/// Encrypts `text` the way the chat application does.
fn encrypt(key: &LessSafeKey, text: &str) -> Result<String, QboError> {
    let mut iv = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut iv)
        .map_err(|_| QboError::Config("no system randomness".to_owned()))?;
    let mut ciphertext = text.as_bytes().to_vec();
    let tag = key
        .seal_in_place_separate_tag(
            Nonce::assume_unique_for_key(iv),
            Aad::empty(),
            &mut ciphertext,
        )
        .map_err(|_| QboError::Config("encryption failed".to_owned()))?;
    Ok(BASE64.encode([&iv[..], tag.as_ref(), &ciphertext].concat()))
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn values_round_trip_through_the_app_format() {
        let key = parse_key(KEY).unwrap();
        let stored = encrypt(&key, "secret-token").unwrap();
        assert_eq!(
            decrypt(&key, stored.as_bytes()).as_deref(),
            Some("secret-token")
        );
        // Layout: base64(iv | tag | ciphertext).
        assert_eq!(
            BASE64.decode(&stored).unwrap().len(),
            12 + 16 + "secret-token".len()
        );
    }

    #[test]
    fn decrypts_a_value_written_by_node() {
        // node -e 'const c=require("crypto");const k=Buffer.from(KEY,"hex");
        //   const iv=Buffer.alloc(12,7);const x=c.createCipheriv("aes-256-gcm",k,iv);
        //   const ct=Buffer.concat([x.update("hello","utf8"),x.final()]);
        //   console.log(Buffer.concat([iv,x.getAuthTag(),ct]).toString("base64"))'
        let key = parse_key(KEY).unwrap();
        let stored = "BwcHBwcHBwcHBwcHwoZjQXd5Iz7b7X5cJCk6v2cPyjAA";
        assert_eq!(decrypt(&key, stored.as_bytes()).as_deref(), Some("hello"));
    }

    #[test]
    fn tampered_or_garbage_values_do_not_decrypt() {
        let key = parse_key(KEY).unwrap();
        let mut stored = BASE64.decode(encrypt(&key, "x").unwrap()).unwrap();
        *stored.last_mut().unwrap() ^= 1;
        assert_eq!(decrypt(&key, BASE64.encode(stored).as_bytes()), None);
        assert_eq!(decrypt(&key, b"not base64!"), None);
        assert_eq!(decrypt(&key, b"AAAA"), None);
    }

    #[test]
    fn key_must_be_256_bits_of_hex() {
        assert!(parse_key("abcd").is_err());
        assert!(parse_key(&"zz".repeat(32)).is_err());
        assert!(parse_key(KEY).is_ok());
    }

    #[test]
    fn environment_follows_qbo_env() {
        assert_eq!(
            QboEnvironment::from_env_value("production"),
            QboEnvironment::Production
        );
        assert_eq!(
            QboEnvironment::from_env_value("sandbox"),
            QboEnvironment::Sandbox
        );
        assert_eq!(QboEnvironment::from_env_value(""), QboEnvironment::Sandbox);
        assert_eq!(
            QboEnvironment::Production.api_base(),
            "https://quickbooks.api.intuit.com"
        );
    }

    #[test]
    fn refresh_tokens_are_form_encoded() {
        assert_eq!(percent_encode("AB12-_.~"), "AB12-_.~");
        assert_eq!(percent_encode("a+b/c="), "a%2Bb%2Fc%3D");
    }

    #[test]
    fn debug_output_never_shows_tokens() {
        let access = QboJobAccess::Connected {
            realm_id: "123".to_owned(),
            access_token: "very-secret".to_owned(),
            api_base: "x".to_owned(),
        };
        assert!(!format!("{access:?}").contains("very-secret"));
    }
}
