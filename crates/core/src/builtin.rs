//! The declarations Beacon owns: source `beacon` (its own notices) and source `trail` (what Trail's
//! alert webhook becomes). They are the TOML files in `crates/core/builtin/`, read with the same
//! parser and checks as a service's, except that their sources are reserved and their titles may
//! be a slot on its own (`Check::builtin`).

use crate::decl::{Check, Declarations, parse};
use crate::problem::Problem;

pub const BEACON: &str = include_str!("../builtin/beacon.toml");
pub const TRAIL: &str = include_str!("../builtin/trail.toml");

/// Beacon's update for a source with no alert kinds that stopped reporting (`spec/delivery.md`,
/// "Sources").
pub const SOURCE_UNOBSERVED: &str = "beacon.source_unobserved";
/// Beacon's alert for a source with alert kinds that stopped reporting: its alerts are on hold.
pub const SOURCE_STOPPED: &str = "beacon.source_stopped";
/// Beacon's record for a source that reported again.
pub const SOURCE_OBSERVED: &str = "beacon.source_observed";
/// Beacon's one notice for a flapping alert.
pub const FLAPPING: &str = "beacon.flapping";
/// The morning push that stands in for what quiet hours held.
pub const CATCH_UP: &str = "beacon.catch_up";
/// The update Beacon records when non-alert pushes pass the daily budget.
pub const BUDGET: &str = "beacon.budget";
/// Trail's severe metric rules: alerts with a lifecycle.
pub const TRAIL_SEVERE: &str = "trail.severe";
/// Trail's severe event rules: one-off alerts.
pub const TRAIL_SEVERE_NOTICE: &str = "trail.severe_notice";
/// Trail's warnings: silent updates.
pub const TRAIL_WARNING: &str = "trail.warning";
/// Trail's info rules: records.
pub const TRAIL_INFO: &str = "trail.info";
/// The source Trail's alerts become. Beacon owns its four kinds above; a declarations folder with
/// `source = "trail"` may add more, which Trail's rules name with `kind` (`spec/api.md`,
/// `POST /v1/trail`).
pub const TRAIL_SOURCE: &str = "trail";
/// Trail's built-in kinds: always there, and no published kind may take one of their names.
pub const TRAIL_BUILTIN: [&str; 4] = [TRAIL_SEVERE, TRAIL_SEVERE_NOTICE, TRAIL_WARNING, TRAIL_INFO];

/// Beacon's and Trail's declarations, parsed and checked.
pub fn declarations() -> Result<Vec<Declarations>, Vec<Problem>> {
    let check = Check {
        builtin: true,
        ..Check::default()
    };
    Ok(vec![parse(BEACON, &check)?, parse(TRAIL, &check)?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Class, PushLevel};

    #[test]
    fn builtins_parse_and_pass_their_checks() {
        let all = declarations().unwrap();
        let beacon = &all[0];
        assert_eq!(beacon.source, "beacon");
        for name in [
            SOURCE_UNOBSERVED,
            SOURCE_STOPPED,
            SOURCE_OBSERVED,
            FLAPPING,
            CATCH_UP,
            BUDGET,
        ] {
            assert!(beacon.kinds.contains_key(name), "{name}");
        }
        assert_eq!(beacon.kinds[SOURCE_UNOBSERVED].push, PushLevel::None);
        assert_eq!(beacon.kinds[SOURCE_OBSERVED].class, Class::Record);
        let stopped = &beacon.kinds[SOURCE_STOPPED];
        assert_eq!((stopped.class, stopped.push), (Class::Alert, PushLevel::Normal));
        assert!(!stopped.one_off);
        let trail = &all[1];
        assert_eq!(trail.kinds[TRAIL_SEVERE].class, Class::Alert);
        assert_eq!(trail.kinds[TRAIL_SEVERE].push, PushLevel::Urgent);
        assert!(trail.kinds[TRAIL_SEVERE_NOTICE].one_off);
        assert_eq!(trail.kinds[TRAIL_WARNING].push, PushLevel::Silent);
        assert_eq!(trail.kinds[TRAIL_INFO].class, Class::Record);
    }

    #[test]
    fn a_service_may_not_publish_them() {
        let problems = parse(BEACON, &Check::default()).unwrap_err();
        assert!(problems.iter().any(|p| p.message.contains("Beacon's own source")));
        let problems = parse(TRAIL, &Check::default()).unwrap_err();
        assert!(problems.iter().any(|p| p.message.contains("words of its own")));
    }
}
