use std::time::Duration;

/// A reading of a monotonic clock, in milliseconds since an arbitrary origin.
///
/// The core never reads the clock itself: callers pass the current reading in
/// with every event, which keeps the state machine deterministic under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Millis(pub u64);

impl Millis {
    /// Time elapsed since `earlier`, or zero if `earlier` is in the future.
    pub fn since(self, earlier: Millis) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }

    pub fn saturating_add(self, d: Duration) -> Millis {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Millis(self.0.saturating_add(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_saturates_at_zero() {
        assert_eq!(Millis(5).since(Millis(10)), Duration::ZERO);
        assert_eq!(Millis(10).since(Millis(4)), Duration::from_millis(6));
    }

    #[test]
    fn add_saturates() {
        assert_eq!(
            Millis(u64::MAX - 1).saturating_add(Duration::from_secs(1)),
            Millis(u64::MAX)
        );
    }
}
