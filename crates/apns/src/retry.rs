//! The retry schedule (`spec/apns.md`, "Results and retries"): pure, so every case is tested with a
//! fake clock.

use crate::client::Answer;

/// What happens to a push after a round of attempts at every device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// A device took it.
    Sent,
    /// Every device finished without taking it: `rejected`.
    Failed,
    /// It would be retried, but not before `give_up` after it was queued: `expired`.
    Expired,
    /// Try again at this time (unix ms).
    RetryAt(i64),
}

/// `results` is this round's answer from every device; `attempts_before` the attempts already
/// made (0 on the first); `retry` the waits in ms between attempts (`apns.retry`, so at most
/// `retry.len() + 1` attempts); `queued_at`, `give_up` and `now` in ms.
pub fn next(
    results: &[Answer],
    attempts_before: i64,
    retry: &[i64],
    queued_at: i64,
    give_up: i64,
    now: i64,
) -> Next {
    if results.iter().any(|a| matches!(a, Answer::Accepted)) {
        return Next::Sent;
    }
    let worth_retrying = results.iter().any(|a| match a {
        Answer::Uncertain { .. } => true,
        Answer::Rejected { retryable, .. } => *retryable,
        Answer::Accepted => false,
    });
    if !worth_retrying {
        return Next::Failed;
    }
    let made = usize::try_from(attempts_before).unwrap_or(0);
    match retry.get(made) {
        None => Next::Failed,
        Some(wait) => {
            let at = now.saturating_add(*wait);
            if at >= queued_at.saturating_add(give_up) {
                Next::Expired
            } else {
                Next::RetryAt(at)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1000;
    const RETRY: [i64; 4] = [10 * S, 30 * S, 120 * S, 300 * S];
    const HOUR: i64 = 3600 * S;
    const T0: i64 = 1_794_571_200_000;

    fn uncertain() -> Answer {
        Answer::Uncertain {
            reason: "timed out".into(),
        }
    }

    fn rejected(reason: &str, retryable: bool) -> Answer {
        Answer::Rejected {
            reason: reason.into(),
            dead: false,
            retryable,
        }
    }

    #[test]
    fn any_device_taking_it_is_sent() {
        let round = [uncertain(), Answer::Accepted, rejected("BadTopic", false)];
        assert_eq!(next(&round, 0, &RETRY, T0, HOUR, T0), Next::Sent);
    }

    #[test]
    fn retries_follow_the_waits_then_fail() {
        let round = [uncertain()];
        let mut now = T0;
        for (made, wait) in RETRY.iter().enumerate() {
            assert_eq!(
                next(&round, made as i64, &RETRY, T0, HOUR, now),
                Next::RetryAt(now + wait)
            );
            now += wait;
        }
        assert_eq!(
            next(&round, 4, &RETRY, T0, HOUR, now),
            Next::Failed,
            "five attempts at most"
        );
        assert_eq!(
            next(&round, 0, &[], T0, HOUR, T0),
            Next::Failed,
            "no waits: one attempt"
        );
    }

    #[test]
    fn retryable_rejections_retry_and_others_fail() {
        for (reason, retryable) in [("TooManyRequests", true), ("HTTP 503", true)] {
            assert_eq!(
                next(&[rejected(reason, retryable)], 1, &RETRY, T0, HOUR, T0),
                Next::RetryAt(T0 + 30 * S)
            );
        }
        assert_eq!(
            next(&[rejected("BadTopic", false)], 0, &RETRY, T0, HOUR, T0),
            Next::Failed
        );
        let dead = Answer::Rejected {
            reason: "Unregistered".into(),
            dead: true,
            retryable: false,
        };
        assert_eq!(
            next(std::slice::from_ref(&dead), 0, &RETRY, T0, HOUR, T0),
            Next::Failed
        );
        assert_eq!(
            next(&[dead, uncertain()], 0, &RETRY, T0, HOUR, T0),
            Next::RetryAt(T0 + 10 * S)
        );
        assert_eq!(next(&[], 0, &RETRY, T0, HOUR, T0), Next::Failed);
    }

    #[test]
    fn nothing_starts_after_give_up() {
        let round = [uncertain()];
        // 59m55s after queueing, a 10 s wait would land past the hour.
        let late = T0 + HOUR - 5 * S;
        assert_eq!(next(&round, 0, &RETRY, T0, HOUR, late), Next::Expired);
        // Exactly at the hour is too late too.
        assert_eq!(
            next(&round, 0, &RETRY, T0, HOUR, T0 + HOUR - 10 * S),
            Next::Expired
        );
        assert_eq!(
            next(&round, 0, &RETRY, T0, HOUR, T0 + HOUR - 11 * S),
            Next::RetryAt(T0 + HOUR - S)
        );
        // A failure for good is failed, not expired, however late.
        assert_eq!(
            next(&[rejected("BadTopic", false)], 0, &RETRY, T0, HOUR, late),
            Next::Failed
        );
    }
}
