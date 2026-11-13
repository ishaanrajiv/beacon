//! The signing key and the provider tokens made from it (`spec/apns.md`, "Provider tokens").

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use beacon_core::problem::Problem;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use std::fmt;
use std::path::Path;

/// How long a provider token is reused before it is signed again: 40 minutes. Apple refuses
/// tokens older than an hour and throttles ones refreshed more often than every 20 minutes.
pub const PROVIDER_TOKEN_MS: i64 = 40 * 60 * 1000;

/// `[apns]` from `beacon.toml`, with the key read and checked.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// The `.p8` file's text: a PKCS#8 P-256 private key in PEM.
    pub key_pem: String,
    pub key_id: String,
    pub team_id: String,
    /// The app's bundle id.
    pub topic: String,
}

/// Leaves the key out, so a config can be logged.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("key_pem", &"<redacted>")
            .field("key_id", &self.key_id)
            .field("team_id", &self.team_id)
            .field("topic", &self.topic)
            .finish()
    }
}

impl Config {
    /// Reads the key at `key_path` and checks every field. `key_id` defaults to the `<key id>` in a
    /// file named `AuthKey_<key id>.p8`.
    pub fn load(
        key_path: &Path,
        key_id: Option<&str>,
        team_id: &str,
        topic: &str,
    ) -> Result<Config, Problem> {
        let shown = key_path.display();
        let key_pem = std::fs::read_to_string(key_path).map_err(|e| {
            Problem::new(
                "apns.key_path",
                format!("can't read `{shown}`: {e}"),
                "point key_path at the .p8 key Apple issued, like \"/etc/beacon/AuthKey_ABC123DEFG.p8\", readable by Beacon",
            )
        })?;
        Config::from_parts(key_pem, &shown.to_string(), key_path, key_id, team_id, topic)
    }

    fn from_parts(
        key_pem: String,
        shown: &str,
        key_path: &Path,
        key_id: Option<&str>,
        team_id: &str,
        topic: &str,
    ) -> Result<Config, Problem> {
        signing_key(&key_pem).map_err(|why| {
            Problem::new(
                "apns.key_path",
                format!("`{shown}` is not a PKCS#8 P-256 private key: {why}"),
                "use the .p8 file Apple's developer site issued, unchanged: it begins with -----BEGIN PRIVATE KEY-----",
            )
        })?;
        let key_id = match key_id {
            Some(id) => id.to_string(),
            None => key_id_from_file_name(key_path).ok_or_else(|| {
                Problem::new(
                    "apns.key_id",
                    format!("not set, and `{shown}` is not named AuthKey_<key id>.p8"),
                    "set key_id = \"ABC123DEFG\" (the key's id on Apple's developer site), or keep the file's original name",
                )
            })?,
        };
        if key_id.is_empty() || !key_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(Problem::new(
                "apns.key_id",
                format!("`{key_id}` is not a key id: it should be letters and digits"),
                "set key_id = \"ABC123DEFG\", the 10-character id Apple shows beside the key",
            ));
        }
        if team_id.is_empty() || !team_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(Problem::new(
                "apns.team_id",
                format!("`{team_id}` is not a team id: it should be letters and digits"),
                "set team_id = \"TEAM123456\", the 10-character id on Apple's developer account page",
            ));
        }
        if topic.is_empty()
            || !topic
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return Err(Problem::new(
                "apns.topic",
                format!("`{topic}` is not a bundle id"),
                "set topic = \"com.example.lab\", the app's bundle id",
            ));
        }
        Ok(Config {
            key_pem,
            key_id,
            team_id: team_id.to_string(),
            topic: topic.to_string(),
        })
    }
}

fn key_id_from_file_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_prefix("AuthKey_")?.strip_suffix(".p8")?;
    (!id.is_empty()).then(|| id.to_string())
}

/// The PEM's DER bytes: the lines between BEGIN and END PRIVATE KEY, base64-decoded.
fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let mut inside = false;
    let mut found_end = false;
    let mut b64 = String::new();
    for line in pem.lines().map(str::trim) {
        if line == "-----BEGIN PRIVATE KEY-----" {
            inside = true;
        } else if line == "-----END PRIVATE KEY-----" {
            found_end = inside;
            break;
        } else if inside {
            b64.push_str(line);
        }
    }
    if !found_end {
        return Err("no -----BEGIN PRIVATE KEY----- ... -----END PRIVATE KEY----- block".to_string());
    }
    STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| format!("the base64 between the markers doesn't decode ({e})"))
}

fn signing_key(pem: &str) -> Result<EcdsaKeyPair, String> {
    let der = pem_to_der(pem)?;
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &SystemRandom::new())
        .map_err(|e| format!("ring refused it ({e})"))
}

/// A provider token: an ES256 JWT, `{"alg":"ES256","kid":key_id}` over `{"iss":team_id,"iat":…}`,
/// each part base64url without padding. `now` is unix milliseconds.
pub(crate) fn sign(config: &Config, now: i64) -> Result<String, String> {
    let key = signing_key(&config.key_pem)?;
    let header = serde_json::json!({"alg": "ES256", "kid": config.key_id});
    let claims = serde_json::json!({"iss": config.team_id, "iat": now.div_euclid(1000)});
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature = key
        .sign(&SystemRandom::new(), signing_input.as_bytes())
        .map_err(|e| format!("signing failed ({e})"))?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_ref())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{decode_part, key_pem};
    use ring::signature::{ECDSA_P256_SHA256_FIXED, KeyPair, UnparsedPublicKey};

    fn write_key(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("beacon-apns-{}-{name}", std::process::id()))
    }

    #[test]
    fn load_takes_the_key_id_from_the_file_name() {
        let dir = scratch("name");
        let path = write_key(&dir, "AuthKey_ABC123DEFG.p8", &key_pem());
        let config = Config::load(&path, None, "TEAM123456", "com.example.lab").unwrap();
        assert_eq!(config.key_id, "ABC123DEFG");
        assert_eq!(config.team_id, "TEAM123456");
        assert_eq!(config.topic, "com.example.lab");
        let given = Config::load(&path, Some("XYZ987WVUT"), "TEAM123456", "com.example.lab").unwrap();
        assert_eq!(given.key_id, "XYZ987WVUT");
        assert!(
            !format!("{config:?}").contains("PRIVATE"),
            "Debug leaves the key out"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_explains_each_bad_field() {
        let dir = scratch("bad");
        let good = write_key(&dir, "AuthKey_ABC123DEFG.p8", &key_pem());
        let renamed = write_key(&dir, "signing.p8", &key_pem());
        let garbage = write_key(&dir, "AuthKey_GARBAGE000.p8", "not a key");
        let bad_b64 = write_key(
            &dir,
            "AuthKey_BADB64XXXX.p8",
            "-----BEGIN PRIVATE KEY-----\n!!!\n-----END PRIVATE KEY-----\n",
        );
        let not_p256 = write_key(
            &dir,
            "AuthKey_NOTP256XXX.p8",
            &format!(
                "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
                STANDARD.encode(b"nope")
            ),
        );
        let topic = "com.example.lab";

        let p = Config::load(&dir.join("missing.p8"), None, "TEAM123456", topic).unwrap_err();
        assert_eq!(p.at, "apns.key_path");
        assert!(p.message.contains("can't read"), "{p}");
        assert!(p.fix.contains("AuthKey_"), "{p}");

        for path in [&garbage, &bad_b64, &not_p256] {
            let p = Config::load(path, None, "TEAM123456", topic).unwrap_err();
            assert_eq!(p.at, "apns.key_path");
            assert!(p.message.contains("not a PKCS#8 P-256 private key"), "{p}");
            assert!(p.fix.contains("BEGIN PRIVATE KEY"), "{p}");
        }

        let p = Config::load(&renamed, None, "TEAM123456", topic).unwrap_err();
        assert_eq!(p.at, "apns.key_id");
        assert!(p.message.contains("AuthKey_<key id>.p8"), "{p}");
        assert!(Config::load(&renamed, Some("ABC123DEFG"), "TEAM123456", topic).is_ok());

        let p = Config::load(&good, Some("ABC 123"), "TEAM123456", topic).unwrap_err();
        assert_eq!(p.at, "apns.key_id");
        let p = Config::load(&good, None, "", topic).unwrap_err();
        assert_eq!(p.at, "apns.team_id");
        assert!(p.fix.contains("TEAM123456"));
        let p = Config::load(&good, None, "TEAM123456", "").unwrap_err();
        assert_eq!(p.at, "apns.topic");
        let p = Config::load(&good, None, "TEAM123456", "com.example.lab\r\nx: y").unwrap_err();
        assert_eq!(p.at, "apns.topic");
        assert!(p.fix.contains("com.example.lab"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_token_is_an_es256_jwt_the_public_key_verifies() {
        let pem = key_pem();
        let config = Config {
            key_pem: pem.clone(),
            key_id: "ABC123DEFG".into(),
            team_id: "TEAM123456".into(),
            topic: "com.example.lab".into(),
        };
        let jwt = sign(&config, 1_794_571_200_999).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(
            !jwt.contains('=') && !jwt.contains('+') && !jwt.contains('/'),
            "base64url, no padding"
        );
        assert_eq!(
            decode_part(parts[0]),
            serde_json::json!({"alg": "ES256", "kid": "ABC123DEFG"})
        );
        assert_eq!(
            decode_part(parts[1]),
            serde_json::json!({"iss": "TEAM123456", "iat": 1_794_571_200})
        );

        let pair = signing_key(&pem).unwrap();
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert_eq!(signature.len(), 64, "raw r || s");
        let public = UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, pair.public_key().as_ref());
        public
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
        assert!(public.verify(b"something else", &signature).is_err());
    }
}
