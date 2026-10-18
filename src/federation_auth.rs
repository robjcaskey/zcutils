//! Explicit, non-transitive identity and approval authorization.
//!
//! This module has no discovery, network, clock override, or I/O admission
//! side effects. Callers supply administratively registered keys and real time.
//! The debug HTTP adapters exercise it; existing management listeners do not
//! automatically gain OAuth support by linking this module.

use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

pub fn denied(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason.to_owned())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub issuer: String,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationKey {
    pub issuer: String,
    pub kid: String,
    /// SEC1 uncompressed P-256 public point, base64url without padding.
    pub public_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    typ: String,
    kid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdentityClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
    pub jti: String,
    // Other issuer claims, including groups/roles/tenant, confer no authority.
}

/// Constructible only by signature verification. Untrusted claims cannot be
/// passed directly to the authorization policy.
#[derive(Clone, Debug)]
pub struct VerifiedIdentity {
    claims: IdentityClaims,
    key: VerificationKey,
}

impl VerifiedIdentity {
    pub fn principal(&self) -> Principal {
        Principal {
            issuer: self.claims.iss.clone(),
            subject: self.claims.sub.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalClaims {
    pub iss: String,
    pub aud: String,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
    pub jti: String,
    pub principal: Principal,
    pub federation: String,
    pub region: String,
    pub resource: String,
    pub tenant: Vec<String>,
    pub action: String,
    pub role: String,
    pub ticket: String,
    pub approver: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalStatus {
    pub iss: String,
    pub aud: String,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
    pub grant_jti: String,
    pub active: bool,
}

fn verify<T: DeserializeOwned>(token: &str, key: &VerificationKey, typ: &str) -> io::Result<T> {
    if token.len() > 16_384 {
        return Err(denied("token_too_large"));
    }
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(denied("malformed_jwt"));
    }
    let decode = |part: &str| {
        URL_SAFE_NO_PAD
            .decode(part)
            .map_err(|_| denied("malformed_base64"))
    };
    let header: Header =
        serde_json::from_slice(&decode(parts[0])?).map_err(|_| denied("invalid_jwt_header"))?;
    if header.alg != "ES256" || header.typ != typ || header.kid != key.kid {
        return Err(denied("wrong_algorithm_type_or_key"));
    }
    let public = decode(&key.public_key)?;
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, public)
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &decode(parts[2])?,
        )
        .map_err(|_| denied("invalid_signature"))?;
    serde_json::from_slice(&decode(parts[1])?).map_err(|_| denied("invalid_claims"))
}

fn valid_times(iat: u64, nbf: u64, exp: u64, now: u64, maximum: u64) -> io::Result<()> {
    if iat > now
        || nbf > now
        || exp <= now
        || exp <= iat
        || nbf < iat
        || nbf >= exp
        || exp.saturating_sub(iat) > maximum
    {
        return Err(denied("expired_or_invalid_token_time"));
    }
    Ok(())
}

/// Issuer is only an index into configured keys, never a discovery URL.
pub fn verify_identity(
    token: &str,
    keys: &[VerificationKey],
    audience: &str,
    now: u64,
) -> io::Result<VerifiedIdentity> {
    if token.len() > 16_384 {
        return Err(denied("token_too_large"));
    }
    let body = token
        .split('.')
        .nth(1)
        .ok_or_else(|| denied("malformed_jwt"))?;
    let hint: IdentityClaims = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| denied("malformed_base64"))?,
    )
    .map_err(|_| denied("invalid_claims"))?;
    let key = keys
        .iter()
        .find(|k| k.issuer == hint.iss)
        .ok_or_else(|| denied("untrusted_identity_issuer"))?;
    let claims: IdentityClaims = verify(token, key, "at+jwt")?;
    if claims.iss != key.issuer
        || claims.aud != audience
        || claims.sub.is_empty()
        || claims.jti.is_empty()
    {
        return Err(denied("wrong_identity_issuer_or_audience"));
    }
    valid_times(claims.iat, claims.nbf, claims.exp, now, 3600)?;
    Ok(VerifiedIdentity {
        claims,
        key: key.clone(),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    pub id: String,
    pub tenant: Vec<String>,
    pub budget_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub principal: Principal,
    pub region: String,
    pub tenant: Vec<String>,
    pub descendants: bool,
    pub actions: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationPolicy {
    pub federation: String,
    pub audience: String,
    pub identity_keys: Vec<VerificationKey>,
    pub approval_key: VerificationKey,
    pub approval_audience: String,
    pub max_approval_seconds: u64,
    pub resources: BTreeMap<String, Resource>,
    pub grants: Vec<Grant>,
    /// region -> action -> required approval-only role.
    pub approval_roles: BTreeMap<String, BTreeMap<String, String>>,
}

impl AuthorizationPolicy {
    pub fn validate(&self) -> io::Result<()> {
        if self.federation.is_empty()
            || self.audience.is_empty()
            || self.approval_audience.is_empty()
            || self.audience == self.approval_audience
            || self.identity_keys.is_empty()
            || self.max_approval_seconds == 0
            || self.max_approval_seconds > 3600
        {
            return Err(denied("invalid_authorization_policy"));
        }
        let mut issuers = BTreeSet::new();
        for key in &self.identity_keys {
            if key.issuer.is_empty()
                || key.kid.is_empty()
                || !issuers.insert(&key.issuer)
                || key.issuer == self.approval_key.issuer
                || key.public_key == self.approval_key.public_key
            {
                return Err(denied("identity_and_approval_authorities_must_be_distinct"));
            }
        }
        if self.grants.iter().any(|g| {
            g.region.is_empty() || g.tenant.is_empty() || g.tenant.iter().any(|s| s.is_empty())
        }) {
            return Err(denied("empty_tenant_scope"));
        }
        if self.resources.iter().any(|(id, resource)| {
            id != &resource.id
                || id.is_empty()
                || resource.budget_id.is_empty()
                || resource.tenant.is_empty()
                || resource.tenant.iter().any(|s| s.is_empty())
        }) {
            return Err(denied("invalid_resource_catalog"));
        }
        Ok(())
    }

    pub fn identity(&self, token: &str, now: u64) -> io::Result<VerifiedIdentity> {
        self.validate()?;
        verify_identity(token, &self.identity_keys, &self.audience, now)
    }

    pub fn authorize(
        &self,
        verified: &VerifiedIdentity,
        region: &str,
        resource_id: &str,
        action: &str,
        approval: Option<&str>,
        status: Option<&str>,
        now: u64,
    ) -> io::Result<&Resource> {
        self.validate()?;
        // Recheck time: callers must not cache a previously valid identity
        // past its expiration while continuing to authorize mutations.
        let identity = &verified.claims;
        valid_times(identity.iat, identity.nbf, identity.exp, now, 3600)?;
        if identity.aud != self.audience || !self.identity_keys.contains(&verified.key) {
            return Err(denied("wrong_identity_issuer_or_audience"));
        }
        let resource = self
            .resources
            .get(resource_id)
            .ok_or_else(|| denied("resource_denied"))?;
        let principal = verified.principal();
        if !self.grants.iter().any(|g| {
            g.principal == principal
                && g.region == region
                && g.actions.contains(action)
                && (g.tenant == resource.tenant
                    || (g.descendants && resource.tenant.starts_with(&g.tenant)))
        }) {
            return Err(denied("scope_or_action_denied"));
        }
        if let Some(role) = self
            .approval_roles
            .get(region)
            .and_then(|rules| rules.get(action))
        {
            let token = approval.ok_or_else(|| denied("approval_required"))?;
            let grant: ApprovalClaims = verify(token, &self.approval_key, "zc-approval+jwt")?;
            valid_times(
                grant.iat,
                grant.nbf,
                grant.exp,
                now,
                self.max_approval_seconds,
            )?;
            if grant.iss != self.approval_key.issuer
                || grant.aud != self.approval_audience
                || grant.principal != principal
                || grant.federation != self.federation
                || grant.region != region
                || grant.resource != resource.id
                || grant.tenant != resource.tenant
                || grant.action != action
                || &grant.role != role
                || grant.jti.is_empty()
                || grant.ticket.is_empty()
                || grant.approver.is_empty()
            {
                return Err(denied("approval_binding_mismatch"));
            }
            let status: ApprovalStatus = verify(
                status.ok_or_else(|| denied("approval_authority_unavailable"))?,
                &self.approval_key,
                "zc-status+jwt",
            )?;
            valid_times(status.iat, status.nbf, status.exp, now, 5)?;
            if status.iss != self.approval_key.issuer
                || status.aud != self.approval_audience
                || status.grant_jti != grant.jti
                || !status.active
                || status.exp > grant.exp
            {
                return Err(denied("approval_revoked_or_status_mismatch"));
            }
        }
        Ok(resource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
    };
    use serde_json::{Value, json};

    struct Signer {
        pair: EcdsaKeyPair,
        key: VerificationKey,
    }
    impl Signer {
        fn new(issuer: &str) -> Self {
            let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
            let key = VerificationKey {
                issuer: issuer.into(),
                kid: "test-key".into(),
                public_key: URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()),
            };
            Self { pair, key }
        }
        fn sign_header(&self, claims: &Value, header: Value) -> String {
            let data = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap())
            );
            let signature = self
                .pair
                .sign(&SystemRandom::new(), data.as_bytes())
                .unwrap();
            format!("{}.{}", data, URL_SAFE_NO_PAD.encode(signature.as_ref()))
        }
        fn sign(&self, claims: &Value, typ: &str) -> String {
            self.sign_header(claims, json!({"alg":"ES256","kid":self.key.kid,"typ":typ}))
        }
    }
    fn setup() -> (Signer, Signer, AuthorizationPolicy, Value, Value, Value) {
        let idp = Signer::new("https://identity.example");
        let approval = Signer::new("https://approval.example");
        let principal = Principal {
            issuer: idp.key.issuer.clone(),
            subject: "alice".into(),
        };
        let policy = AuthorizationPolicy {
            federation: "f1".into(),
            audience: "storage-f1".into(),
            identity_keys: vec![idp.key.clone()],
            approval_key: approval.key.clone(),
            approval_audience: "approval-f1".into(),
            max_approval_seconds: 30,
            resources: BTreeMap::from([(
                "v1".into(),
                Resource {
                    id: "v1".into(),
                    tenant: vec!["acme".into(), "retail".into()],
                    budget_id: "b1".into(),
                },
            )]),
            grants: vec![Grant {
                principal: principal.clone(),
                region: "east".into(),
                tenant: vec!["acme".into()],
                descendants: true,
                actions: BTreeSet::from(["read".into(), "delete".into()]),
            }],
            approval_roles: BTreeMap::from([(
                "east".into(),
                BTreeMap::from([("delete".into(), "repair".into())]),
            )]),
        };
        let identity = json!({"iss":idp.key.issuer,"sub":"alice","aud":"storage-f1","iat":100,"nbf":100,"exp":400,"jti":"identity-1","roles":["admin"]});
        let grant = json!({"iss":approval.key.issuer,"aud":"approval-f1","iat":100,"nbf":100,"exp":130,"jti":"approval-1","principal":principal,"federation":"f1","region":"east","resource":"v1","tenant":["acme","retail"],"action":"delete","role":"repair","ticket":"CHG1","approver":"independent-reviewer"});
        let status = json!({"iss":approval.key.issuer,"aud":"approval-f1","iat":100,"nbf":100,"exp":102,"grant_jti":"approval-1","active":true});
        (idp, approval, policy, identity, grant, status)
    }

    #[test]
    fn identity_never_substitutes_for_independent_approval() {
        let (idp, approval, policy, claims, grant, status) = setup();
        let token = idp.sign(&claims, "at+jwt");
        let identity = policy.identity(&token, 101).unwrap();
        assert!(
            policy
                .authorize(&identity, "east", "v1", "read", None, None, 101)
                .is_ok()
        );
        assert!(
            policy
                .authorize(&identity, "east", "v1", "delete", None, None, 101)
                .is_err()
        );
        assert!(
            policy
                .authorize(&identity, "east", "v1", "delete", Some(&token), None, 101)
                .is_err()
        );
        assert!(
            policy
                .authorize(
                    &identity,
                    "east",
                    "v1",
                    "delete",
                    Some(&approval.sign(&grant, "zc-approval+jwt")),
                    Some(&approval.sign(&status, "zc-status+jwt")),
                    101
                )
                .is_ok()
        );
    }

    #[test]
    fn every_approval_binding_is_enforced_even_with_a_valid_signature() {
        let (idp, approval, policy, claims, grant, status) = setup();
        let identity = policy.identity(&idp.sign(&claims, "at+jwt"), 101).unwrap();
        let status = approval.sign(&status, "zc-status+jwt");
        for (field, value) in [
            ("iss", json!("foreign")),
            ("aud", json!("other")),
            ("federation", json!("f2")),
            ("region", json!("west")),
            ("resource", json!("v2")),
            ("tenant", json!(["beta"])),
            ("action", json!("read")),
            ("role", json!("admin")),
            (
                "principal",
                json!({"issuer":idp.key.issuer,"subject":"bob"}),
            ),
            ("exp", json!(131)),
        ] {
            let mut bad = grant.clone();
            bad[field] = value;
            assert!(
                policy
                    .authorize(
                        &identity,
                        "east",
                        "v1",
                        "delete",
                        Some(&approval.sign(&bad, "zc-approval+jwt")),
                        Some(&status),
                        101
                    )
                    .is_err(),
                "binding {field}"
            );
        }
    }

    #[test]
    fn revocation_expiry_and_status_substitution_fail_closed() {
        let (idp, approval, policy, claims, grant, status) = setup();
        let identity = policy.identity(&idp.sign(&claims, "at+jwt"), 101).unwrap();
        let grant = approval.sign(&grant, "zc-approval+jwt");
        assert!(
            policy
                .authorize(&identity, "east", "v1", "delete", Some(&grant), None, 101)
                .is_err()
        );
        for (field, value) in [
            ("active", json!(false)),
            ("grant_jti", json!("other-grant")),
            ("exp", json!(101)),
            ("exp", json!(106)),
            ("aud", json!("other")),
        ] {
            let mut bad = status.clone();
            bad[field] = value;
            assert!(
                policy
                    .authorize(
                        &identity,
                        "east",
                        "v1",
                        "delete",
                        Some(&grant),
                        Some(&approval.sign(&bad, "zc-status+jwt")),
                        101
                    )
                    .is_err(),
                "status {field}"
            );
        }
        assert!(
            policy
                .authorize(
                    &identity,
                    "east",
                    "v1",
                    "delete",
                    Some(&grant),
                    Some(&approval.sign(&status, "zc-status+jwt")),
                    130
                )
                .is_err()
        );
    }

    #[test]
    fn algorithm_key_urls_unknown_issuers_and_audiences_are_rejected() {
        let (idp, _, policy, claims, _, _) = setup();
        for header in [
            json!({"alg":"none","kid":"test-key","typ":"at+jwt"}),
            json!({"alg":"HS256","kid":"test-key","typ":"at+jwt"}),
            json!({"alg":"ES256","kid":"test-key","typ":"at+jwt","jku":"http://127.0.0.1/private"}),
        ] {
            assert!(
                policy
                    .identity(&idp.sign_header(&claims, header), 101)
                    .is_err()
            );
        }
        for (field, value) in [
            ("iss", json!("http://unregistered.invalid")),
            ("aud", json!("different-federation")),
            ("nbf", json!(102)),
            ("exp", json!(101)),
        ] {
            let mut bad = claims.clone();
            bad[field] = value;
            assert!(policy.identity(&idp.sign(&bad, "at+jwt"), 101).is_err());
        }
    }

    #[test]
    fn verified_identity_is_bound_to_the_actual_registered_key() {
        let (idp, _, mut policy, claims, _, _) = setup();
        let identity = policy.identity(&idp.sign(&claims, "at+jwt"), 101).unwrap();
        policy.identity_keys[0] = Signer::new(&idp.key.issuer).key;
        assert!(
            policy
                .authorize(&identity, "east", "v1", "read", None, None, 101)
                .is_err()
        );
    }

    #[test]
    fn approval_cannot_expand_the_local_ceiling_or_share_identity_keys() {
        let (idp, approval, mut policy, claims, grant, status) = setup();
        let identity = policy.identity(&idp.sign(&claims, "at+jwt"), 101).unwrap();
        policy.grants[0].actions.remove("delete");
        assert!(
            policy
                .authorize(
                    &identity,
                    "east",
                    "v1",
                    "delete",
                    Some(&approval.sign(&grant, "zc-approval+jwt")),
                    Some(&approval.sign(&status, "zc-status+jwt")),
                    101
                )
                .is_err()
        );
        policy.approval_key.public_key = policy.identity_keys[0].public_key.clone();
        assert!(policy.validate().is_err());
    }
    #[test]
    fn path_boundaries_and_expiry_are_exact() {
        let root = vec!["acme".to_string()];
        assert!(vec!["acme".to_string(), "retail".to_string()].starts_with(&root));
        assert!(!vec!["acme-other".to_string()].starts_with(&root));
        assert!(valid_times(10, 10, 20, 19, 10).is_ok());
        assert!(valid_times(10, 10, 20, 20, 10).is_err());
        assert!(valid_times(10, 10, 21, 19, 10).is_err());
    }
}
