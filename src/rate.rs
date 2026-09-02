use std::time::Instant;

const TOKEN_SCALE: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RateLimit {
    rate_per_second: u32,
    burst: u32,
}

impl RateLimit {
    pub(crate) const fn new(rate_per_second: u32, burst: u32) -> Self {
        Self {
            rate_per_second,
            burst,
        }
    }

    pub(crate) const fn is_valid(self) -> bool {
        self.rate_per_second != 0 && self.burst != 0
    }
}

#[derive(Debug)]
pub(crate) struct TokenBucket {
    limit: RateLimit,
    tokens: u128,
    updated_at: Instant,
}

impl TokenBucket {
    pub(crate) fn full(limit: RateLimit, now: Instant) -> Self {
        Self {
            limit,
            tokens: u128::from(limit.burst) * TOKEN_SCALE,
            updated_at: now,
        }
    }

    pub(crate) fn try_take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.updated_at);
        let refill = elapsed
            .as_nanos()
            .saturating_mul(u128::from(self.limit.rate_per_second));
        let capacity = u128::from(self.limit.burst) * TOKEN_SCALE;
        self.tokens = self.tokens.saturating_add(refill).min(capacity);
        self.updated_at = now;

        if self.tokens < TOKEN_SCALE {
            return false;
        }
        self.tokens -= TOKEN_SCALE;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn burst_is_available_immediately_and_stays_bounded() {
        let now = Instant::now();
        let limit = RateLimit::new(2, 3);
        let mut bucket = TokenBucket::full(limit, now);

        assert!(bucket.try_take(now));
        assert!(bucket.try_take(now));
        assert!(bucket.try_take(now));
        assert!(!bucket.try_take(now));
    }

    #[test]
    fn fractional_refill_accumulates_without_floating_point() {
        let now = Instant::now();
        let limit = RateLimit::new(2, 1);
        let mut bucket = TokenBucket::full(limit, now);

        assert!(bucket.try_take(now));
        assert!(!bucket.try_take(now + Duration::from_millis(499)));
        assert!(bucket.try_take(now + Duration::from_millis(500)));
        assert!(!bucket.try_take(now + Duration::from_millis(999)));
        assert!(bucket.try_take(now + Duration::from_secs(1)));
    }

    #[test]
    fn long_refill_cannot_exceed_burst() {
        let now = Instant::now();
        let limit = RateLimit::new(u32::MAX, 2);
        let mut bucket = TokenBucket::full(limit, now);

        assert!(bucket.try_take(now));
        assert!(bucket.try_take(now));
        assert!(!bucket.try_take(now));
        let later = now + Duration::from_secs(10);
        assert!(bucket.try_take(later));
        assert!(bucket.try_take(later));
        assert!(!bucket.try_take(later));
    }

    #[test]
    fn invalid_limits_are_rejected() {
        assert!(!RateLimit::new(0, 1).is_valid());
        assert!(!RateLimit::new(1, 0).is_valid());
        assert!(RateLimit::new(1, 1).is_valid());
    }
}
