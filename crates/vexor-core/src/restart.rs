//! Restart delay and crash-loop policy.

use std::time::Duration;

/// Hard floor on the time between two *automatic* spawns of one instance,
/// measured spawn to spawn. Whatever the configuration, an instance is never
/// auto-restarted more than once per interval. Operator commands (`start`,
/// `restart`) are not limited.
///
/// Differs from PM2, where `restart_delay: 0` with a sub-second crash restarts
/// immediately.
pub const MIN_RESTART_INTERVAL: Duration = Duration::from_secs(1);

/// PM2 `exp_backoff_restart_delay` multiplier.
const EXP_MULTIPLIER: f64 = 1.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// PM2 `restart_delay`.
    Fixed(Duration),
    /// PM2 `exp_backoff_restart_delay`: `initial * 1.5^attempt`, capped at `max`.
    Exponential { initial: Duration, max: Duration },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jitter {
    /// Use the computed delay as is.
    None,
    /// Uniform in `[delay / 2, delay]`, so instances that crashed together do
    /// not restart together, and the delay never drops below half its base.
    Equal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPolicy {
    /// PM2 `autorestart`.
    pub autorestart: bool,
    pub backoff: Backoff,
    pub jitter: Jitter,
    /// PM2 `min_uptime`: a run shorter than this is *unstable*.
    pub min_uptime: Duration,
    /// PM2 `max_restarts`: automatic restarts allowed after consecutive
    /// unstable runs. The next unstable exit after that marks the instance
    /// errored. A stable run resets the count.
    pub max_restarts: u32,
    /// A run at least this long resets the exponential backoff to `initial`.
    pub backoff_reset_after: Duration,
    /// PM2 `stop_exit_codes`: exit codes that mean "done, don't restart".
    pub stop_exit_codes: Vec<i32>,
}

impl Default for RestartPolicy {
    /// PM2 defaults: autorestart, no delay, `min_uptime` 1 s, `max_restarts` 16.
    fn default() -> Self {
        RestartPolicy {
            autorestart: true,
            backoff: Backoff::Fixed(Duration::ZERO),
            jitter: Jitter::None,
            min_uptime: Duration::from_secs(1),
            max_restarts: 16,
            backoff_reset_after: Duration::from_secs(30),
            stop_exit_codes: Vec::new(),
        }
    }
}

impl RestartPolicy {
    /// Delay from base policy alone, before jitter and the interval floor.
    /// `attempt` counts automatic restarts since the backoff last reset.
    pub fn base_delay(&self, attempt: u32) -> Duration {
        match self.backoff {
            Backoff::Fixed(d) => d,
            Backoff::Exponential { initial, max } => {
                let exp = i32::try_from(attempt.min(256)).unwrap_or(256);
                let secs = initial.as_secs_f64() * EXP_MULTIPLIER.powi(exp);
                if !secs.is_finite() || secs >= max.as_secs_f64() {
                    max
                } else {
                    Duration::from_secs_f64(secs)
                }
            }
        }
    }

    /// Delay before the next automatic spawn after a run that lasted `uptime`.
    /// `rand` is any uniformly distributed value; it only feeds the jitter.
    pub fn delay(&self, attempt: u32, uptime: Duration, rand: u64) -> Duration {
        let base = self.base_delay(attempt);
        let jittered = match self.jitter {
            Jitter::None => base,
            Jitter::Equal => {
                let ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
                let half = ms / 2;
                let spread = rand % half.saturating_add(1);
                Duration::from_millis(ms - half + spread)
            }
        };
        jittered.max(MIN_RESTART_INTERVAL.saturating_sub(uptime))
    }

    pub fn is_stop_exit_code(&self, code: i32) -> bool {
        self.stop_exit_codes.contains(&code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn exp(initial: u64, max: u64) -> RestartPolicy {
        RestartPolicy {
            backoff: Backoff::Exponential {
                initial: MS(initial),
                max: MS(max),
            },
            ..RestartPolicy::default()
        }
    }

    #[test]
    fn exponential_grows_by_one_and_a_half_and_caps() {
        let p = exp(100, 15_000);
        assert_eq!(p.base_delay(0), MS(100));
        assert_eq!(p.base_delay(1), MS(150));
        assert_eq!(p.base_delay(2), MS(225));
        assert_eq!(p.base_delay(50), MS(15_000));
        assert_eq!(p.base_delay(u32::MAX), MS(15_000));
    }

    #[test]
    fn floor_applies_spawn_to_spawn() {
        let p = RestartPolicy::default(); // restart_delay 0
        assert_eq!(p.delay(0, Duration::ZERO, 0), MIN_RESTART_INTERVAL);
        assert_eq!(p.delay(0, MS(400), 0), MS(600));
        assert_eq!(p.delay(0, MS(5_000), 0), Duration::ZERO);
    }

    #[test]
    fn equal_jitter_stays_within_half_and_full() {
        let p = RestartPolicy {
            backoff: Backoff::Fixed(MS(10_000)),
            jitter: Jitter::Equal,
            ..RestartPolicy::default()
        };
        for rand in [0, 1, 4_999, 5_000, 5_001, u64::MAX] {
            let d = p.delay(0, MS(60_000), rand);
            assert!(d >= MS(5_000) && d <= MS(10_000), "{d:?} for rand {rand}");
        }
        assert_eq!(p.delay(0, MS(60_000), 0), MS(5_000));
        assert_eq!(p.delay(0, MS(60_000), 5_000), MS(10_000));
    }

    #[test]
    fn stop_exit_codes() {
        let p = RestartPolicy {
            stop_exit_codes: vec![0, 3],
            ..RestartPolicy::default()
        };
        assert!(p.is_stop_exit_code(3));
        assert!(!p.is_stop_exit_code(1));
    }
}
