//! The identifiers a push carries (`spec/delivery.md`, "Sending").

/// The `apns-collapse-id` for an alert's or action's pushes: the same for every push about one
/// `(kind, key)`, so a recovery, a repeat or a snooze ending replaces the banner already on the
/// phone instead of adding one. `b1-` and 16 lowercase hex digits of FNV-1a 64 over the kind, a
/// newline and the key: 19 bytes, well inside Apple's 64.
pub fn collapse_id(kind: &str, key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in kind.bytes().chain(std::iter::once(b'\n')).chain(key.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("b1-{hash:016x}")
}

/// The `apns-collapse-id` for an update's push: one per item.
pub fn item_collapse_id(id: i64) -> String {
    format!("i{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_ids_are_stable_short_and_distinct() {
        // `b1-` and 16 hex digits, whatever the input.
        assert_eq!(collapse_id("", "").len(), 19);
        assert_eq!(
            collapse_id("backup.failed", "nightly"),
            collapse_id("backup.failed", "nightly")
        );
        assert_ne!(
            collapse_id("backup.failed", "nightly"),
            collapse_id("backup.failed", "weekly")
        );
        assert_ne!(collapse_id("backup.failed", ""), collapse_id("backup.stale", ""));
        // The newline keeps ("a.b", "c") and ("a.bc", "") apart.
        assert_ne!(collapse_id("a.b", "c"), collapse_id("a.bc", ""));
        assert_eq!(
            collapse_id("backup.failed", "nightly"),
            "b1-".to_string() + &fnv("backup.failed\nnightly")
        );
        assert_eq!(collapse_id("backup.failed", "nightly"), "b1-29fd692930019189");
        assert_eq!(item_collapse_id(4182), "i4182");
    }

    fn fnv(text: &str) -> String {
        let mut hash: u64 = 14_695_981_039_346_656_037;
        for byte in text.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211);
        }
        format!("{hash:016x}")
    }
}
