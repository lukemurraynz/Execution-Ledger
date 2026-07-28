//! Bearer-token authentication against Microsoft Entra ID, multi-tenant.
//!
//! Any Entra tenant can present a token (validated against Microsoft's
//! public signing keys), but a token only authorizes a request once its
//! tenant is present and `active` in the `EXECUTION_LEDGER_tenants` table (see
//! `db::is_tenant_active`). That check — not anything in the JWT — is the
//! real multi-tenancy boundary: it is how a new org gets onboarded or
//! offboarded without a code change.
//!
//! Every authenticated request carries a `tenant_id`; route handlers must
//! scope every query by it (see `execution-ledger-api`'s `get_job_run`) so one tenant
//! can never read another tenant's rows, even by guessing an id.

use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::{ExecutionLedgerError, Result};

const JWKS_URL: &str = "https://login.microsoftonline.com/common/discovery/v2.0/keys";
const JWKS_TTL: Duration = Duration::from_hours(24);

/// The claims Execution Ledger cares about from an Entra-issued access token. Entra
/// tokens carry many more claims; only the ones used for tenant scoping and
/// audit are deserialized.
#[derive(Debug, Clone, Deserialize)]
struct EntraClaims {
    iss: String,
    tid: Uuid,
    sub: String,
    #[serde(default)]
    oid: Option<String>,
    #[serde(default)]
    scp: String,
    #[serde(default)]
    roles: Vec<String>,
}

/// The authenticated caller, attached to the request after a successful
/// [`EntraJwtValidator::validate`]. Route handlers use `tenant_id` to scope
/// every query — never the raw JWT again.
#[derive(Debug, Clone)]
pub struct AuthenticatedTenant {
    pub tenant_id: Uuid,
    pub subject: String,
    pub scopes: Vec<String>,
}

impl AuthenticatedTenant {
    #[must_use]
    pub fn has_scope(&self, required: &str) -> bool {
        self.scopes.iter().any(|scope| scope == required)
    }

    #[must_use]
    pub fn has_any_scope(&self, required: &[&str]) -> bool {
        required.iter().any(|scope| self.has_scope(scope))
    }
}

struct CachedJwks {
    keys: JwkSet,
    fetched_at: Instant,
}

/// Validates Entra ID access tokens against Microsoft's public JWKS,
/// caching the key set for 24 hours.
pub struct EntraJwtValidator {
    http: reqwest::Client,
    audience: String,
    cache: RwLock<Option<CachedJwks>>,
}

impl EntraJwtValidator {
    /// # Errors
    /// Returns [`ExecutionLedgerError::DependencyFailed`] when the TLS-capable
    /// HTTP client cannot be built (missing platform TLS backend). This is a
    /// fatal startup condition — the validator cannot safely validate tokens
    /// without a configured HTTP client.
    pub fn new(audience: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "reqwest",
                source: Box::new(source),
            })?;
        Ok(Self {
            http,
            audience,
            cache: RwLock::new(None),
        })
    }

    async fn jwks(&self) -> Result<JwkSet> {
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.as_ref()
                && cached.fetched_at.elapsed() < JWKS_TTL
            {
                return Ok(cached.keys.clone());
            }
        }

        let keys: JwkSet = self
            .http
            .get(JWKS_URL)
            .send()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "entra-jwks",
                source: Box::new(source),
            })?
            .json()
            .await
            .map_err(|source| ExecutionLedgerError::DependencyFailed {
                dependency: "entra-jwks",
                source: Box::new(source),
            })?;

        *self.cache.write().await = Some(CachedJwks {
            keys: keys.clone(),
            fetched_at: Instant::now(),
        });

        Ok(keys)
    }

    /// Verifies signature, expiry, and audience, then checks that the
    /// token's issuer actually matches its own tenant claim — Entra's
    /// multi-tenant `common` endpoint issues a per-tenant `iss`, so this
    /// stops a token from one tenant claiming a different tenant's `tid`.
    ///
    /// # Errors
    /// Returns [`ExecutionLedgerError::Unauthenticated`] for any malformed, unsigned,
    /// expired, wrong-audience, or issuer/tenant-mismatched token.
    pub async fn validate(&self, bearer_token: &str) -> Result<AuthenticatedTenant> {
        let header =
            decode_header(bearer_token).map_err(|_| ExecutionLedgerError::Unauthenticated {
                reason: "malformed token",
            })?;
        let kid = header.kid.ok_or(ExecutionLedgerError::Unauthenticated {
            reason: "token has no key id",
        })?;

        let jwks = self.jwks().await?;
        let jwk = jwks
            .find(&kid)
            .ok_or(ExecutionLedgerError::Unauthenticated {
                reason: "unknown signing key",
            })?;
        let decoding_key =
            DecodingKey::from_jwk(jwk).map_err(|_| ExecutionLedgerError::Unauthenticated {
                reason: "unsupported signing key",
            })?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.audience]);

        let data =
            decode::<EntraClaims>(bearer_token, &decoding_key, &validation).map_err(|_| {
                ExecutionLedgerError::Unauthenticated {
                    reason: "signature, expiry, or audience check failed",
                }
            })?;

        let expected_issuer = format!("https://login.microsoftonline.com/{}/v2.0", data.claims.tid);
        if data.claims.iss != expected_issuer {
            return Err(ExecutionLedgerError::Unauthenticated {
                reason: "issuer does not match the token's own tenant claim",
            });
        }

        Ok(AuthenticatedTenant {
            tenant_id: data.claims.tid,
            subject: data.claims.oid.unwrap_or(data.claims.sub),
            scopes: normalize_scopes(&data.claims.scp, &data.claims.roles),
        })
    }
}

fn normalize_scopes(scp: &str, roles: &[String]) -> Vec<String> {
    let mut scopes = Vec::new();
    for scope in scp.split_whitespace() {
        if !scope.is_empty() && !scopes.iter().any(|existing| existing == scope) {
            scopes.push(scope.to_string());
        }
    }
    for role in roles {
        if !role.is_empty() && !scopes.iter().any(|existing| existing == role) {
            scopes.push(role.clone());
        }
    }
    scopes
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::jwk::{
        AlgorithmParameters, CommonParameters, Jwk, KeyAlgorithm, RSAKeyParameters, RSAKeyType,
    };
    use jsonwebtoken::{EncodingKey, get_current_timestamp};
    use jsonwebtoken::{Header, encode};
    use serde::Serialize;

    // Test-only RSA keypair, generated once offline and pinned here so this
    // test needs no network call and no key-generation dependency; it is
    // never used outside this module.
    const TEST_PRIVATE_KEY_PEM: &str = include_str!("testdata/test_rsa_private.pem");
    const TEST_MODULUS: &str = "pStxGGIHOYsM8eI6gkQuDQzENp3eF3vmeK4DzBIdfWHdBG8F-RPh0aWyjU7SU0sEBt6wqSlciX27OULlBRLygWlrsJ3mdP9fQ3hqTBkRPeRweKhlMnf0WurhPPEXEDYVHn3rCy9_4TIo15OWPlGAN7InQkh0fVdSOUQgCRNKifDqHF4EjKDEKnoHE_a8Og6_Jub5-2W70GrYDKQ-MjIOvF-PiG9O9W9OKNDj0BV-y9r8GhjL9F01L3VjfnCo8JY6w4ae9tfga4RpT-l9b4-emQNbsGsk6hV33hz7-rEDjeFRqchhryJXI37iXqwwcF6ou8-uVNGGQA3NNtTkuY808Q";
    const TEST_EXPONENT: &str = "AQAB";
    const TEST_KID: &str = "test-key-1";
    const TEST_TENANT: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0001);

    #[derive(Serialize)]
    struct TestClaims {
        iss: String,
        tid: Uuid,
        aud: String,
        sub: String,
        oid: Option<String>,
        scp: String,
        roles: Vec<String>,
        exp: u64,
    }

    fn test_jwks() -> JwkSet {
        JwkSet {
            keys: vec![Jwk {
                common: CommonParameters {
                    key_id: Some(TEST_KID.to_string()),
                    key_algorithm: Some(KeyAlgorithm::RS256),
                    ..Default::default()
                },
                algorithm: AlgorithmParameters::RSA(RSAKeyParameters {
                    key_type: RSAKeyType::RSA,
                    n: TEST_MODULUS.to_string(),
                    e: TEST_EXPONENT.to_string(),
                }),
            }],
        }
    }

    // Test-only invariant: these fixtures are pinned, self-consistent
    // constants (a valid PEM, valid claims) so encoding can never fail; the
    // coding standard requires item-level justification rather than a
    // workspace-wide allowance.
    #[allow(clippy::expect_used)]
    fn sign_test_token(claims: &TestClaims) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(TEST_KID.to_string());
        let key = EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY_PEM.as_bytes())
            .expect("test fixture key is a valid PEM");
        encode(&header, claims, &key).expect("test fixture claims encode")
    }

    /// Exercises the real validation path — signature check against a JWK,
    /// audience check, and the issuer/tenant cross-check — using a
    /// self-signed token against a pinned test keypair, without any network
    /// call to Microsoft. This is the one runnable check for the auth
    /// module's non-trivial logic per the project's testing policy.
    #[tokio::test]
    // Test-only invariant: the token below is built to pass validation, so
    // `expect` can never panic in practice; justified per-item as above.
    #[allow(clippy::expect_used)]
    async fn accepts_a_correctly_signed_token_for_its_own_tenant() {
        let validator = EntraJwtValidator {
            http: reqwest::Client::new(),
            audience: "api://execution-ledger".to_string(),
            cache: RwLock::new(Some(CachedJwks {
                keys: test_jwks(),
                fetched_at: Instant::now(),
            })),
        };

        let token = sign_test_token(&TestClaims {
            iss: format!("https://login.microsoftonline.com/{TEST_TENANT}/v2.0"),
            tid: TEST_TENANT,
            aud: "api://execution-ledger".to_string(),
            sub: "fHU2zm9sCqhv_hetZhZ7qFCXvpVWPzLpEKA2AhEXu7g".to_string(),
            oid: Some("11111111-1111-1111-1111-111111111111".to_string()),
            scp: "jobs:read:own platform:read".to_string(),
            roles: vec![],
            exp: get_current_timestamp() + 3600,
        });

        let authenticated = validator
            .validate(&token)
            .await
            .expect("correctly signed, matching-issuer token should validate");

        assert_eq!(authenticated.tenant_id, TEST_TENANT);
        assert_eq!(
            authenticated.subject,
            "11111111-1111-1111-1111-111111111111".to_string()
        );
        assert!(authenticated.has_scope("jobs:read:own"));
        assert!(authenticated.has_scope("platform:read"));
    }

    #[tokio::test]
    async fn rejects_a_token_whose_issuer_does_not_match_its_tenant_claim() {
        let validator = EntraJwtValidator {
            http: reqwest::Client::new(),
            audience: "api://execution-ledger".to_string(),
            cache: RwLock::new(Some(CachedJwks {
                keys: test_jwks(),
                fetched_at: Instant::now(),
            })),
        };

        let other_tenant = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0002);
        let token = sign_test_token(&TestClaims {
            iss: format!("https://login.microsoftonline.com/{other_tenant}/v2.0"),
            tid: TEST_TENANT,
            aud: "api://execution-ledger".to_string(),
            sub: "fHU2zm9sCqhv_hetZhZ7qFCXvpVWPzLpEKA2AhEXu7g".to_string(),
            oid: Some("11111111-1111-1111-1111-111111111111".to_string()),
            scp: "jobs:read:own".to_string(),
            roles: vec![],
            exp: get_current_timestamp() + 3600,
        });

        let result = validator.validate(&token).await;

        assert!(matches!(
            result,
            Err(ExecutionLedgerError::Unauthenticated { .. })
        ));
    }

    #[tokio::test]
    // Test-only invariant: the token below is built to pass validation with
    // only `sub`, which mirrors service-to-service tokens that omit `oid`.
    #[allow(clippy::expect_used)]
    async fn falls_back_to_sub_when_oid_is_missing() {
        let validator = EntraJwtValidator {
            http: reqwest::Client::new(),
            audience: "api://execution-ledger".to_string(),
            cache: RwLock::new(Some(CachedJwks {
                keys: test_jwks(),
                fetched_at: Instant::now(),
            })),
        };

        let token = sign_test_token(&TestClaims {
            iss: format!("https://login.microsoftonline.com/{TEST_TENANT}/v2.0"),
            tid: TEST_TENANT,
            aud: "api://execution-ledger".to_string(),
            sub: "service-principal-subject".to_string(),
            oid: None,
            scp: "platform:read".to_string(),
            roles: vec![],
            exp: get_current_timestamp() + 3600,
        });

        let authenticated = validator
            .validate(&token)
            .await
            .expect("signed token with matching issuer should validate");

        assert_eq!(
            authenticated.subject,
            "service-principal-subject".to_string()
        );
    }
}
