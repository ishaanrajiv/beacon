//! Apple push for Beacon, as `spec/apns.md` describes. The one crate allowed reqwest, ring and
//! base64.
//!
//! - [`Config::load`] reads the `.p8` signing key once and checks it is a PKCS#8 P-256 key.
//! - [`payload`] and [`activity_payload`] write the JSON bodies Apple takes, within 4 KB.
//! - [`Client::send`] pushes one notification to one device: an ES256 provider token reused for
//!   40 minutes, one fresh token on `ExpiredProviderToken`, the other environment on
//!   `BadDeviceToken`, and dead tokens reported for the caller to delete.
//! - [`Client::live_activity`] sends one Live Activity update, trying environments in order.
//! - [`next`] is the retry schedule: what happens to a push after a round of attempts.
//!
//! Nothing here reads the clock: every call that needs the time takes `now` in unix milliseconds.
//! The wire is a [`Transport`]; [`HttpTransport`] is HTTP/2 over rustls, and the tests use a fake.
//! Device tokens never appear in an error or a reason this crate returns.

mod client;
mod payload;
mod retry;
mod token;

pub use client::{Answer, Client, DeviceResult, HttpTransport, Response, Transport};
pub use payload::{LiveActivity, MAX_PAYLOAD_BYTES, Notification, activity_payload, payload};
pub use retry::{Next, next};
pub use token::{Config, PROVIDER_TOKEN_MS};

/// Which of Apple's two push services a device token belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Environment {
    Production,
    Sandbox,
}

impl Environment {
    pub const ALL: [Environment; 2] = [Environment::Production, Environment::Sandbox];

    pub fn as_str(self) -> &'static str {
        match self {
            Environment::Production => "production",
            Environment::Sandbox => "sandbox",
        }
    }

    pub fn parse(text: &str) -> Option<Environment> {
        Environment::ALL.into_iter().find(|e| e.as_str() == text)
    }

    /// The environment a `BadDeviceToken` answer is tried in next.
    pub fn other(self) -> Environment {
        match self {
            Environment::Production => Environment::Sandbox,
            Environment::Sandbox => Environment::Production,
        }
    }

    /// Apple's host for this environment, port 443.
    pub fn host(self) -> &'static str {
        match self {
            Environment::Production => "https://api.push.apple.com",
            Environment::Sandbox => "https://api.sandbox.push.apple.com",
        }
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    use base64::Engine;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};

    /// A fresh P-256 PKCS#8 key, PEM-wrapped like Apple's `.p8` files.
    pub fn key_pem() -> String {
        let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
            .expect("generate a key");
        let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
        for line in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).expect("ascii"));
            pem.push('\n');
        }
        pem.push_str("-----END PRIVATE KEY-----\n");
        pem
    }

    /// One part of a JWT, decoded from base64url to JSON.
    pub fn decode_part(part: &str) -> serde_json::Value {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part)
            .expect("base64url");
        serde_json::from_slice(&bytes).expect("JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environments_round_trip_and_swap() {
        for e in Environment::ALL {
            assert_eq!(Environment::parse(e.as_str()), Some(e));
            assert_eq!(e.other().other(), e);
            assert_ne!(e.other(), e);
        }
        assert_eq!(Environment::parse("staging"), None);
        assert_eq!(Environment::Production.host(), "https://api.push.apple.com");
        assert_eq!(Environment::Sandbox.host(), "https://api.sandbox.push.apple.com");
    }
}
