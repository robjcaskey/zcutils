//! Network trust and transport-protection policy.
//!
//! Trust describes a network segment, never an identity. Cross-region links
//! are unconditionally untrusted. The two supported wire modes always protect
//! user data with authenticated encryption; TLS is an optional outer layer for
//! compliance regimes that specifically require it.

use serde::{Deserialize, Serialize};
use std::io;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentTrust {
    Trusted,
    #[default]
    Untrusted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkSegmentScope {
    SameAz,
    SameRegion,
    CrossRegion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTrustPolicy {
    pub generation: u64,
    #[serde(default)]
    pub same_az: SegmentTrust,
    #[serde(default)]
    pub same_region: SegmentTrust,
}

impl Default for NetworkTrustPolicy {
    fn default() -> Self {
        Self {
            generation: 0,
            same_az: SegmentTrust::Untrusted,
            same_region: SegmentTrust::Untrusted,
        }
    }
}

impl NetworkTrustPolicy {
    pub fn segment_trust(&self, scope: NetworkSegmentScope) -> SegmentTrust {
        match scope {
            NetworkSegmentScope::SameAz => self.same_az,
            NetworkSegmentScope::SameRegion => self.same_region,
            // Deliberately not configurable. A provider's private backbone,
            // placement group, or VPN does not make cross-region traffic
            // trusted for this policy layer.
            NetworkSegmentScope::CrossRegion => SegmentTrust::Untrusted,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FramingProfile {
    /// The base cleartext fields are magic, protocol version, direction,
    /// unpredictable nonce, ciphertext length, and audited transport flags.
    /// A reviewed lane protocol may additionally expose lane ordinal/count and
    /// monotonically increasing frame sequence. Object/tenant/volume IDs,
    /// logical offsets, data, checksums of plaintext, topology labels, and
    /// error text are encrypted.
    #[default]
    PublicEnvelopeV1,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkTransportMode {
    /// Native authenticated payload encryption with an audited, non-sensitive
    /// cleartext envelope. This is the performance/reference mode.
    #[default]
    NativeAead,
    /// The same native payload protection inside an authenticated TLS tunnel.
    /// This exists for check-the-box TLS requirements, not headline numbers.
    NativeAeadWithTls,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkTransportSecurity {
    #[serde(default)]
    pub mode: LinkTransportMode,
    #[serde(default)]
    pub framing_profile: FramingProfile,
}

/// Wire behavior proven by a concrete transport adapter. Policy intent and
/// adapter capability are deliberately separate so an old/plain adapter cannot
/// become compliant merely because a secure mode was selected in Raft.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportAdapterCapabilities {
    pub authenticated_user_data_encryption: bool,
    pub public_envelope_v1: bool,
    pub mutual_tls_1_3: bool,
}

impl TransportAdapterCapabilities {
    pub const GLOBAL_RPC: Self = Self {
        authenticated_user_data_encryption: true,
        public_envelope_v1: true,
        mutual_tls_1_3: true,
    };

    pub const LEGACY_TCP_MUX: Self = Self {
        authenticated_user_data_encryption: false,
        public_envelope_v1: false,
        mutual_tls_1_3: false,
    };
}

impl LinkTransportSecurity {
    pub fn validates_user_data_confidentiality(&self) -> bool {
        matches!(
            self.mode,
            LinkTransportMode::NativeAead | LinkTransportMode::NativeAeadWithTls
        )
    }

    pub fn uses_tls(&self) -> bool {
        self.mode == LinkTransportMode::NativeAeadWithTls
    }

    /// TLS results remain useful compliance and regression measurements, but
    /// must not be published as the native transport's representative ceiling.
    pub fn headline_performance_eligible(&self) -> bool {
        self.mode == LinkTransportMode::NativeAead
    }

    pub fn validate(&self) -> io::Result<()> {
        if !self.validates_user_data_confidentiality() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "network links must use authenticated user-data encryption",
            ));
        }
        Ok(())
    }

    /// Bind committed policy to measured/implemented wire capability before a
    /// listener or dialer is activated. This is a control-path check, never a
    /// per-I/O operation.
    pub fn validate_realization(
        &self,
        capabilities: TransportAdapterCapabilities,
    ) -> io::Result<()> {
        self.validate()?;
        if !capabilities.authenticated_user_data_encryption || !capabilities.public_envelope_v1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "transport adapter lacks authenticated encryption or public-envelope conformance",
            ));
        }
        if self.uses_tls() && !capabilities.mutual_tls_1_3 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "transport adapter cannot realize required mutual TLS 1.3",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_defaults_to_untrusted_and_cross_region_cannot_be_relaxed() {
        let defaults = NetworkTrustPolicy::default();
        assert_eq!(
            defaults.segment_trust(NetworkSegmentScope::SameAz),
            SegmentTrust::Untrusted
        );
        assert_eq!(
            defaults.segment_trust(NetworkSegmentScope::SameRegion),
            SegmentTrust::Untrusted
        );
        let permissive_local = NetworkTrustPolicy {
            generation: 1,
            same_az: SegmentTrust::Trusted,
            same_region: SegmentTrust::Trusted,
        };
        assert_eq!(
            permissive_local.segment_trust(NetworkSegmentScope::CrossRegion),
            SegmentTrust::Untrusted
        );
    }

    #[test]
    fn tls_is_compliance_only_for_headline_benchmarks() {
        let native = LinkTransportSecurity::default();
        assert!(native.headline_performance_eligible());
        assert!(!native.uses_tls());

        let tls = LinkTransportSecurity {
            mode: LinkTransportMode::NativeAeadWithTls,
            framing_profile: FramingProfile::PublicEnvelopeV1,
        };
        assert!(tls.validates_user_data_confidentiality());
        assert!(tls.uses_tls());
        assert!(!tls.headline_performance_eligible());
    }

    #[test]
    fn declared_security_cannot_bless_a_plain_adapter() {
        let native = LinkTransportSecurity::default();
        assert!(
            native
                .validate_realization(TransportAdapterCapabilities::GLOBAL_RPC)
                .is_ok()
        );
        assert!(
            native
                .validate_realization(TransportAdapterCapabilities::LEGACY_TCP_MUX)
                .is_err()
        );

        let tls = LinkTransportSecurity {
            mode: LinkTransportMode::NativeAeadWithTls,
            framing_profile: FramingProfile::PublicEnvelopeV1,
        };
        assert!(
            tls.validate_realization(TransportAdapterCapabilities {
                authenticated_user_data_encryption: true,
                public_envelope_v1: true,
                mutual_tls_1_3: false,
            })
            .is_err()
        );
    }
}
