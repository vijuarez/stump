use governor::{
	clock::DefaultClock,
	state::{InMemoryState, NotKeyed},
	Quota, RateLimiter as GovernorLimiter,
};
use std::{num::NonZeroU32, sync::Arc};

/// A simple rate limiter wrapping [`governor::RateLimiter`]
#[derive(Clone)]
pub struct RateLimiter {
	inner: Arc<GovernorLimiter<NotKeyed, InMemoryState, DefaultClock>>,
}

impl RateLimiter {
	/// Creates a new rate limiter with the specified requests per second
	///
	/// # Panics
	/// Panics if `requests_per_second` is 0
	pub fn new(requests_per_second: u32) -> Self {
		Self::from_quota(Quota::per_second(
			NonZeroU32::new(requests_per_second)
				.expect("requests_per_second must be > 0"),
		))
	}

	/// Creates a new rate limiter with the specified requests per minute
	///
	/// Some providers publish their limits per minute (e.g. AniList allows 90/min and
	/// drops to 30/min while its API is degraded), which a per-second quota cannot
	/// express without over-fetching.
	///
	/// # Panics
	/// Panics if `requests_per_minute` is 0
	pub fn per_minute(requests_per_minute: u32) -> Self {
		Self::from_quota(
			Quota::per_minute(
				NonZeroU32::new(requests_per_minute)
					.expect("requests_per_minute must be > 0"),
			)
			// Without this the quota allows a burst of `requests_per_minute` up
			// front, which providers still answer with 429s.
			.allow_burst(NonZeroU32::new(1).expect("burst must be > 0")),
		)
	}

	fn from_quota(quota: Quota) -> Self {
		Self {
			inner: Arc::new(GovernorLimiter::direct(quota)),
		}
	}

	/// Waits until a request is permitted by the rate limiter
	pub async fn until_ready(&self) {
		self.inner.until_ready().await;
	}

	/// Attempts to acquire permission for a request without waiting
	///
	/// Returns `false` if rate limited
	pub fn try_acquire(&self) -> bool {
		self.inner.check().is_ok()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_rate_limiter_creation() {
		let limiter = RateLimiter::new(10);
		assert!(limiter.try_acquire());
	}

	#[test]
	fn test_rate_limiter_per_minute() {
		let limiter = RateLimiter::per_minute(30);
		assert!(limiter.try_acquire());
	}

	/// A per-minute quota must not hand out its whole allowance at once
	#[test]
	fn test_rate_limiter_per_minute_does_not_burst() {
		let limiter = RateLimiter::per_minute(30);
		assert!(limiter.try_acquire());
		assert!(!limiter.try_acquire());
	}

	#[test]
	#[should_panic(expected = "requests_per_minute must be > 0")]
	fn test_rate_limiter_zero_per_minute() {
		let _ = RateLimiter::per_minute(0);
	}

	#[test]
	#[should_panic(expected = "requests_per_second must be > 0")]
	fn test_rate_limiter_zero_rps() {
		let _ = RateLimiter::new(0);
	}
}
