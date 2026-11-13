//! The naming rules. Names are checked once, here, so every tool agrees on them.

/// Sources Beacon owns. A service may not publish declarations under these names.
pub const RESERVED_SOURCES: [&str; 2] = ["beacon", "trail"];

/// A source's name: lowercase letters, digits and hyphens, starting with a letter, at most 32
/// characters (`backup`, `mail-worker`).
pub fn is_source(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && text.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A fact, slot, label or custom action name: snake_case, starting with a letter, at most 32 characters.
pub fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && text.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A kind's name: its source, a dot, then one or more snake_case parts joined by dots
/// (`backup.failed`, `assistant.task.result`). At most 96 characters.
pub fn is_kind_name(source: &str, text: &str) -> bool {
    match text.strip_prefix(source).and_then(|rest| rest.strip_prefix('.')) {
        Some(rest) => text.len() <= 96 && rest.split('.').all(is_ident),
        None => false,
    }
}

/// The source part of a kind name: everything before the first dot.
pub fn source_of(kind: &str) -> &str {
    kind.split('.').next().unwrap_or(kind)
}

/// An item key or an Idempotency-Key: 0 to 128 printable ASCII characters, no spaces. Empty is
/// the default key.
pub fn is_key(text: &str) -> bool {
    text.len() <= 128 && text.bytes().all(|b| b.is_ascii_graphic())
}

/// Words that mark a fact or slot as likely to hold a secret. Notifications are kept for months
/// and shown on a lock screen, so a declaration with one of these in a name is refused.
const SECRET_WORDS: [&str; 10] = [
    "password",
    "passwd",
    "secret",
    "token",
    "otp",
    "api_key",
    "apikey",
    "authorization",
    "cookie",
    "credential",
];

/// The secret-like word in a name, if any.
pub fn secret_word(name: &str) -> Option<&'static str> {
    SECRET_WORDS.into_iter().find(|word| name.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(is_source("backup"));
        assert!(is_source("mail-worker2"));
        assert!(!is_source("Backup"));
        assert!(!is_source("2fa"));
        assert!(!is_source("mail_worker"));
        assert!(is_ident("last_ok"));
        assert!(!is_ident("lastOk"));
        assert!(!is_ident("_x"));
        assert!(is_kind_name("backup", "backup.failed"));
        assert!(is_kind_name("mail-worker", "mail-worker.queue.stuck"));
        assert!(!is_kind_name("backup", "backup"));
        assert!(!is_kind_name("backup", "backup."));
        assert!(!is_kind_name("backup", "backups.failed"));
        assert!(!is_kind_name("backup", "deploy.failed"));
        assert!(!is_kind_name("backup", "backup.Failed"));
        assert_eq!(source_of("backup.failed"), "backup");
        assert!(is_key(""));
        assert!(is_key("thread:42/approval-7"));
        assert!(!is_key("two words"));
        assert_eq!(secret_word("reset_token"), Some("token"));
        assert_eq!(secret_word("reason"), None);
    }
}
