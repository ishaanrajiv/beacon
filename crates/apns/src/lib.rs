//! Apple push for Beacon, as `spec/apns.md` describes: ES256 provider tokens, HTTP/2 to Apple,
//! payloads within 4 KB, retries and dead-token pruning. The one crate allowed reqwest, ring and
//! base64. Not written yet.
