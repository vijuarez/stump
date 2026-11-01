pub mod client;
pub mod error;
pub mod merge;
mod provider;
mod providers;
pub mod rate_limit;
pub mod scoring;
pub(crate) mod serde_utils;
pub mod types;

pub use client::build_client_with_retry;
pub use error::{MetadataProviderError, MetadataResult};
pub use merge::{AutoApplyConfig, FieldMerger, MergeStrategy, MetadataFieldOverride};
pub use provider::{MetadataProvider, ProviderCredentialVerification};
pub use rate_limit::RateLimiter;
pub use scoring::MatchScorer;
pub use types::{
	ConfidenceFactor, ExternalMediaMetadata, ExternalMetadata, ExternalSeriesMetadata,
	MatchCandidate, MediaType, MetadataField, PublicationStatus, SearchOutcome,
	SearchQuery,
};

use providers::{AnilistClient, ComicVineClient, HardcoverClient};

pub fn create_provider(
	provider_type: &str,
	api_token: String,
) -> MetadataResult<Box<dyn MetadataProvider + Send + Sync>> {
	match provider_type {
		"COMIC_VINE" => Ok(Box::new(ComicVineClient::new(api_token, None))),
		"HARDCOVER" => Ok(Box::new(HardcoverClient::new(api_token, None))),
		// AniList's public API needs no credentials, so any supplied token is ignored
		"ANILIST" => Ok(Box::new(AnilistClient::new())),
		_ => Err(MetadataProviderError::UnsupportedProvider(
			provider_type.to_string(),
		)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn creates_anilist_provider_without_token() {
		let provider = create_provider("ANILIST", String::new()).unwrap();
		assert_eq!(provider.id(), "anilist");
	}
}
