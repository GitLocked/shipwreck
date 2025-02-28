// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::rate_limiter::{RateLimiterProps, RateLimiterState};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Helps limit the rate that a particular IP can perform an action.
pub struct IpRateLimiter {
    usage: HashMap<IpAddr, RateLimiterState>,
    props: RateLimiterProps,
    prune_counter: u8,
}

impl IpRateLimiter {
    /// Rate limit must be at least one millisecond.
    /// Burst must be less than 255.
    pub fn new(rate_limit: Duration, burst: u8) -> Self {
        Self {
            usage: HashMap::new(),
            props: RateLimiterProps::new(rate_limit, burst),
            prune_counter: 0,
        }
    }

    /// Marks the action as being performed by the ip address.
    /// Returns true if the action should be blocked (rate limited).
    pub fn should_limit_rate(&mut self, ip: IpAddr) -> bool {
        let now = Instant::now();

        let should_limit_rate = self
            .usage
            .entry(ip)
            .or_insert(RateLimiterState {
                until: now,
                burst_used: 0,
            })
            .should_limit_rate_with_now(&self.props, now);

        self.prune_counter = self.prune_counter.wrapping_add(1);
        if self.prune_counter == 0 {
            self.prune();
        }

        should_limit_rate
    }

    /// Clean up old items. Called automatically, not it is not necessary to call manually.
    pub fn prune(&mut self) {
        let now = Instant::now();
        self.usage
            .retain(|_, rate_limiter_state| rate_limiter_state.until > now)
    }

    /// Returns size of internal data-structure.
    pub fn len(&self) -> usize {
        self.usage.len()
    }
}

#[cfg(test)]
pub mod test {
    use crate::ip_rate_limiter::IpRateLimiter;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::time::Duration;

    #[test]
    pub fn ip_rate_limiter() {
        let ip_one = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        let ip_two = IpAddr::V6(Ipv6Addr::new(1, 2, 3, 4, 5, 6, 7, 8));
        let mut limiter = IpRateLimiter::new(Duration::from_millis(10), 3);

        assert_eq!(limiter.len(), 0);
        assert!(!limiter.should_limit_rate(ip_one));
        assert_eq!(limiter.len(), 1);
        assert!(!limiter.should_limit_rate(ip_one));
        assert_eq!(limiter.len(), 1);

        limiter.prune();
        assert_eq!(limiter.len(), 1);

        assert!(!limiter.should_limit_rate(ip_one));
        assert_eq!(limiter.len(), 1);

        limiter.prune();
        assert_eq!(limiter.len(), 1);

        assert!(limiter.should_limit_rate(ip_one));
        assert_eq!(limiter.len(), 1);

        std::thread::sleep(Duration::from_millis(25));

        assert!(!limiter.should_limit_rate(ip_two));
        assert_eq!(limiter.len(), 2);
        assert!(!limiter.should_limit_rate(ip_two));
        assert_eq!(limiter.len(), 2);

        limiter.prune();
        assert_eq!(limiter.len(), 2);

        std::thread::sleep(Duration::from_millis(10));

        limiter.prune();
        assert_eq!(limiter.len(), 1);

        std::thread::sleep(Duration::from_millis(50));

        limiter.prune();
        assert_eq!(limiter.len(), 0);

        assert!(!limiter.should_limit_rate(ip_one));
        assert_eq!(limiter.len(), 1);
    }
}
