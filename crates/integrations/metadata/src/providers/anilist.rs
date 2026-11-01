//! AniList provider (https://docs.anilist.co)
//!
//! Series-level only: AniList has no per-volume or per-issue records, so
//! `search_media` never yields candidates and `fetch_media_metadata` is unsupported.
//!
//! Public read access does not require authentication, so this client does not
//! use an API token.

use std::collections::HashSet;
use std::sync::OnceLock;

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest_middleware::ClientWithMiddleware;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use titles as t;

use crate::{
	client::{build_client_with_retry, RetryClientConfig},
	error::MetadataProviderError,
	provider::ProviderCredentialVerification,
	types::{
		ExternalMediaMetadata, ExternalMetadata, ExternalSeriesMetadata, MatchCandidate,
		MediaType, PublicationStatus, SearchOutcome, SearchQuery,
	},
	MetadataProvider, RateLimiter,
};

/// AniList allows 90 requests per minute normally, but drops to 30 per minute while
/// its API is degraded (which it currently is). A per-second quota of 1 exceeded the
/// degraded budget during bulk fetches and produced 429s, so the quota is per-minute.
const ANILIST_RATE_LIMIT_PER_MINUTE: u32 = 30;
const DEFAULT_PAGE_SIZE: u32 = 10;
/// The maximum `perPage` AniList accepts
const MAX_PAGE_SIZE: u32 = 50;

/// Fields requested for every media. Search and fetch share this selection so both
/// return everything needed to build a candidate without per-hit follow-up requests.
const MEDIA_FIELDS: &str = r#"
	id
	title { romaji english native }
	synonyms
	description
	status
	startDate { year }
	endDate { year }
	volumes
	genres
	tags { name isMediaSpoiler isAdult }
	isAdult
	staff(perPage: 25) { edges { role node { name { full native } } } }
	coverImage { extraLarge large }
"#;

fn search_query() -> String {
	format!(
		r#"query SearchManga($search: String, $perPage: Int, $isAdult: Boolean) {{
			Page(page: 1, perPage: $perPage) {{
				media(search: $search, type: MANGA, isAdult: $isAdult, sort: SEARCH_MATCH) {{
					{MEDIA_FIELDS}
				}}
			}}
		}}"#
	)
}

fn fetch_query() -> String {
	format!(
		r#"query FetchManga($id: Int) {{
			Media(id: $id, type: MANGA) {{
				{MEDIA_FIELDS}
			}}
		}}"#
	)
}

/// AniList's quota is per account (i.e. per IP), not per client instance. Callers such
/// as the series metadata mutation build a fresh client for every request, so a
/// per-instance limiter would hand each request a full budget and the aggregate burst
/// would still be throttled. The limiter is therefore process-wide.
static RATE_LIMITER: OnceLock<RateLimiter> = OnceLock::new();

fn rate_limiter() -> &'static RateLimiter {
	RATE_LIMITER.get_or_init(|| RateLimiter::per_minute(ANILIST_RATE_LIMIT_PER_MINUTE))
}

pub struct AnilistClient {
	client: ClientWithMiddleware,
}

impl AnilistClient {
	const API_URL: &'static str = "https://graphql.anilist.co";

	pub fn new() -> Self {
		Self {
			client: build_client_with_retry(
				reqwest::Client::new(),
				RetryClientConfig::default(),
			),
		}
	}

	async fn execute<T: DeserializeOwned>(
		&self,
		query: &str,
		variables: Value,
	) -> Result<T, MetadataProviderError> {
		rate_limiter().until_ready().await;

		let response = self
			.client
			.post(Self::API_URL)
			.json(&json!({ "query": query, "variables": variables }))
			.send()
			.await?;

		if response.status() == StatusCode::NOT_FOUND {
			return Err(MetadataProviderError::NotFound(
				"AniList resource".to_string(),
			));
		}

		let body: GraphQLResponse<T> = response.error_for_status()?.json().await?;

		if let Some(errors) = body.errors.filter(|errors| !errors.is_empty()) {
			let messages: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
			return Err(MetadataProviderError::Other(format!(
				"GraphQL errors: {}",
				messages.join("; ")
			)));
		}

		body.data.ok_or(MetadataProviderError::EmptyResponse)
	}
}

#[async_trait]
impl MetadataProvider for AnilistClient {
	fn id(&self) -> &'static str {
		"anilist"
	}

	fn name(&self) -> &'static str {
		"AniList"
	}

	fn supported_media_types(&self) -> Vec<MediaType> {
		vec![
			MediaType::Manga,
			MediaType::Manhwa,
			MediaType::Webtoon,
			MediaType::LightNovel,
			MediaType::WebNovel,
		]
	}

	/// Search AniList manga (including novels and one-shots) for series
	#[tracing::instrument(skip(self))]
	async fn search_series(
		&self,
		query: &SearchQuery,
	) -> Result<SearchOutcome, MetadataProviderError> {
		let search = query.title.trim();
		if search.is_empty() {
			return Ok(SearchOutcome::default());
		}

		let per_page = query
			.limit
			.unwrap_or(DEFAULT_PAGE_SIZE)
			.clamp(1, MAX_PAGE_SIZE);

		let mut data: SearchData = self
			.execute(
				&search_query(),
				json!({ "search": search, "perPage": per_page, "isAdult": false }),
			)
			.await?;

		// Search is ordered from most to least faithful. Release names are commonly
		// polluted with volume ranges, scanlation groups, and edition tags (e.g.
		// "Foo v01-07 [Group] (Yen Press)"), and romanised titles often use
		// diacritics AniList does not, so fall back to cleaned and accent-folded
		// variants before treating the search as a miss. Adult entries are excluded
		// unless nothing else matches, in which case they are returned with an
		// "Adult" age rating so callers can still restrict them.
		let mut matched: Option<String> = None;

		if data.page.media.is_empty() {
			let mut attempts = search_variants(search)
				.into_iter()
				.map(|variant| (variant, false))
				.collect::<Vec<_>>();
			attempts.extend(
				search_variants(search)
					.into_iter()
					.map(|variant| (variant, true)),
			);

			for (variant, is_adult) in attempts {
				// The faithful, non-adult search has already been made above
				if variant == search && !is_adult {
					continue;
				}

				tracing::trace!(search, variant, is_adult, "Retrying AniList search");
				data = self
					.execute(
						&search_query(),
						json!({ "search": variant, "perPage": per_page, "isAdult": is_adult }),
					)
					.await?;
				if !data.page.media.is_empty() {
					matched = Some(variant);
					break;
				}
			}
		}

		let requested = data.page.media.len();
		let candidates = data.page.media.iter().map(to_candidate).collect();

		// Score against the variant that actually matched. A query like
		// "Foo v01-07 [Group]" would otherwise be compared as-is against "Foo" and
		// score far below what the match deserves.
		let scoring_query = match matched {
			Some(variant) if variant != search => SearchQuery {
				title: variant,
				..query.clone()
			},
			_ => query.clone(),
		};

		Ok(SearchOutcome {
			candidates: self.score_search(&scoring_query, candidates),
			requested,
		})
	}

	/// AniList has no per-volume or per-issue records, so there is never a media match
	async fn search_media(
		&self,
		_query: &SearchQuery,
	) -> Result<SearchOutcome, MetadataProviderError> {
		Ok(SearchOutcome::default())
	}

	#[tracing::instrument(skip(self))]
	async fn fetch_series_metadata(
		&self,
		external_id: &str,
	) -> Result<ExternalSeriesMetadata, MetadataProviderError> {
		let id: i64 = external_id.parse().map_err(|_| {
			MetadataProviderError::Other(format!("Invalid AniList ID: {external_id}"))
		})?;

		let data: FetchData = self
			.execute(&fetch_query(), json!({ "id": id }))
			.await
			.map_err(|e| match e {
				MetadataProviderError::NotFound(_) => {
					MetadataProviderError::NotFound(format!("Series {external_id}"))
				},
				other => other,
			})?;

		let media = data.media.ok_or_else(|| {
			MetadataProviderError::NotFound(format!("Series {external_id}"))
		})?;

		Ok(map_series(&media))
	}

	async fn fetch_media_metadata(
		&self,
		_external_id: &str,
	) -> Result<ExternalMediaMetadata, MetadataProviderError> {
		Err(MetadataProviderError::OperationNotSupported)
	}

	/// AniList's public API needs no credentials, so this only checks reachability
	#[tracing::instrument(skip(self))]
	async fn verify_credentials(
		&self,
	) -> Result<ProviderCredentialVerification, MetadataProviderError> {
		let _: Value = self
			.execute(
				"query { Page(page: 1, perPage: 1) { media(type: MANGA) { id } } }",
				json!({}),
			)
			.await?;

		Ok(ProviderCredentialVerification {
			response_status: 200,
			is_valid: true,
			error: None,
		})
	}
}

// Response types

#[derive(Debug, Deserialize)]
struct GraphQLResponse<T> {
	data: Option<T>,
	errors: Option<Vec<GraphQLError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
	message: String,
}

#[derive(Debug, Deserialize)]
struct SearchData {
	#[serde(rename = "Page")]
	page: MediaPage,
}

#[derive(Debug, Deserialize)]
struct MediaPage {
	media: Vec<Media>,
}

#[derive(Debug, Deserialize)]
struct FetchData {
	#[serde(rename = "Media")]
	media: Option<Media>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Media {
	id: i64,
	title: MediaTitle,
	synonyms: Option<Vec<String>>,
	description: Option<String>,
	status: Option<MediaStatus>,
	start_date: Option<FuzzyDate>,
	end_date: Option<FuzzyDate>,
	volumes: Option<i32>,
	genres: Option<Vec<String>>,
	tags: Option<Vec<MediaTag>>,
	is_adult: Option<bool>,
	staff: Option<StaffConnection>,
	cover_image: Option<CoverImage>,
}

#[derive(Debug, Deserialize)]
struct MediaTitle {
	romaji: Option<String>,
	english: Option<String>,
	native: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum MediaStatus {
	Finished,
	Releasing,
	NotYetReleased,
	Cancelled,
	Hiatus,
	#[serde(other)]
	Unknown,
}

#[derive(Debug, Deserialize)]
struct FuzzyDate {
	year: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaTag {
	name: String,
	is_media_spoiler: Option<bool>,
	is_adult: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct StaffConnection {
	edges: Option<Vec<StaffEdge>>,
}

#[derive(Debug, Deserialize)]
struct StaffEdge {
	/// Free text, e.g. "Story & Art (vols 1-41)" or "Translator (Spanish)"
	role: Option<String>,
	node: Option<StaffNode>,
}

#[derive(Debug, Deserialize)]
struct StaffNode {
	name: Option<StaffName>,
}

#[derive(Debug, Deserialize)]
struct StaffName {
	full: Option<String>,
	native: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CoverImage {
	extra_large: Option<String>,
	large: Option<String>,
}

// Mapping

fn to_candidate(media: &Media) -> MatchCandidate {
	MatchCandidate {
		provider: "anilist".to_string(),
		external_id: media.id.to_string(),
		metadata: ExternalMetadata::Series(map_series(media)),
		confidence: 0.0,
		confidence_factors: Vec::new(),
	}
}

fn map_series(media: &Media) -> ExternalSeriesMetadata {
	let title = preferred_title(&media.title)
		.unwrap_or_default()
		.to_string();
	let (authors, artists) = classify_staff(media.staff.as_ref());

	ExternalSeriesMetadata {
		provider: "anilist".to_string(),
		external_id: media.id.to_string(),
		alternative_titles: alternative_titles(media, &title),
		summary: media.description.as_deref().and_then(clean_description),
		status: map_status(media.status),
		year: media.start_date.as_ref().and_then(|d| d.year),
		end_year: media.end_date.as_ref().and_then(|d| d.year),
		genres: non_empty(media.genres.clone().unwrap_or_default()),
		tags: map_tags(media.tags.as_deref()),
		age_rating: (media.is_adult == Some(true)).then(|| "Adult".to_string()),
		authors: non_empty(authors),
		artists: non_empty(artists),
		publisher: None,
		cover_url: media
			.cover_image
			.as_ref()
			.and_then(|c| {
				non_blank(c.extra_large.as_deref()).or(non_blank(c.large.as_deref()))
			})
			.map(str::to_string),
		volume_count: media.volumes,
		title,
	}
}

/// Builds the ordered list of strings to search for, most faithful first
fn search_variants(title: &str) -> Vec<String> {
	let mut variants = vec![title.trim().to_string()];

	if let Some(cleaned) = t::normalize_release_title(title) {
		let folded = t::fold_diacritics(&cleaned);
		variants.push(cleaned);
		if folded != *variants.last().unwrap() {
			variants.push(folded);
		}
	}

	variants.dedup();
	variants
}

fn non_blank(value: Option<&str>) -> Option<&str> {
	value.map(str::trim).filter(|s| !s.is_empty())
}

fn non_empty(values: Vec<String>) -> Option<Vec<String>> {
	(!values.is_empty()).then_some(values)
}

/// English is the most likely language for a Western library; fall back to romaji
fn preferred_title(title: &MediaTitle) -> Option<&str> {
	non_blank(title.english.as_deref())
		.or(non_blank(title.romaji.as_deref()))
		.or(non_blank(title.native.as_deref()))
}

/// Every other known title and synonym, deduplicated case-insensitively
fn alternative_titles(media: &Media, primary: &str) -> Vec<String> {
	let mut seen: HashSet<String> = HashSet::new();
	seen.insert(primary.to_lowercase());

	let names = [
		media.title.romaji.as_deref(),
		media.title.english.as_deref(),
		media.title.native.as_deref(),
	]
	.into_iter()
	.flatten()
	.chain(media.synonyms.iter().flatten().map(String::as_str));

	let mut alternatives = Vec::new();
	for name in names.filter_map(|n| non_blank(Some(n))) {
		if seen.insert(name.to_lowercase()) {
			alternatives.push(name.to_string());
		}
	}
	alternatives
}

fn map_status(status: Option<MediaStatus>) -> Option<PublicationStatus> {
	match status? {
		MediaStatus::Finished => Some(PublicationStatus::Completed),
		MediaStatus::Releasing => Some(PublicationStatus::Ongoing),
		MediaStatus::NotYetReleased => Some(PublicationStatus::Upcoming),
		MediaStatus::Cancelled => Some(PublicationStatus::Cancelled),
		MediaStatus::Hiatus => Some(PublicationStatus::Hiatus),
		MediaStatus::Unknown => None,
	}
}

/// Drops spoiler and adult-flagged tags so they are never shown or matched on
fn map_tags(tags: Option<&[MediaTag]>) -> Option<Vec<String>> {
	let names = tags?
		.iter()
		.filter(|t| !t.is_media_spoiler.unwrap_or(false) && !t.is_adult.unwrap_or(false))
		.map(|t| t.name.clone())
		.collect();
	non_empty(names)
}

/// Splits staff into (writers, artists) using whole words from the free-text role.
/// Translators, supervisors, and editors match neither and are excluded. Roles for
/// localisation credits (e.g. "Touch-up Art & Lettering (English)") are skipped
/// entirely, since their wording would otherwise read as an art credit for a release
/// which is not the original work.
fn classify_staff(staff: Option<&StaffConnection>) -> (Vec<String>, Vec<String>) {
	const WRITER_ROLES: &[&str] = &["story", "author", "writer", "creator"];
	const ARTIST_ROLES: &[&str] = &["art", "artist", "illustrator", "illustration"];
	const LOCALISATION_MARKERS: &[&str] = &[
		"translat",
		"letter",
		"touch-up",
		"edit",
		"adapt",
		"localis",
		"localiz",
		"proofread",
	];

	let mut writers = Vec::new();
	let mut artists = Vec::new();

	let edges = staff.and_then(|s| s.edges.as_deref()).unwrap_or_default();
	for edge in edges {
		let Some(role) = edge.role.as_deref() else {
			continue;
		};
		if is_localisation_role(role, LOCALISATION_MARKERS) {
			continue;
		}

		let Some(name) = edge
			.node
			.as_ref()
			.and_then(|n| n.name.as_ref())
			.and_then(|n| {
				non_blank(n.full.as_deref()).or(non_blank(n.native.as_deref()))
			})
		else {
			continue;
		};

		let words: Vec<String> = role
			.split(|c: char| !c.is_alphabetic())
			.filter(|w| !w.is_empty())
			.map(str::to_lowercase)
			.collect();
		let has_role = |roles: &[&str]| words.iter().any(|w| roles.contains(&w.as_str()));

		if has_role(WRITER_ROLES) {
			push_unique(&mut writers, name);
		}
		if has_role(ARTIST_ROLES) {
			push_unique(&mut artists, name);
		}
	}

	(writers, artists)
}

fn push_unique(names: &mut Vec<String>, name: &str) {
	if !names.iter().any(|existing| existing == name) {
		names.push(name.to_string());
	}
}

fn is_localisation_role(role: &str, markers: &[&str]) -> bool {
	let role = role.to_lowercase();
	markers.iter().any(|marker| role.contains(marker))
}

/// Converts AniList's description (HTML with spoilers) to plain text
fn clean_description(raw: &str) -> Option<String> {
	let visible = strip_spoilers(raw);
	let text = decode_entities(&strip_html_tags(&visible));
	non_blank(Some(&collapse_blank_lines(&text))).map(str::to_string)
}

/// Removes `~!spoiler!~` blocks. An unterminated block hides the rest of the text.
fn strip_spoilers(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	let mut rest = input;

	while let Some(start) = rest.find("~!") {
		out.push_str(&rest[..start]);
		match rest[start + 2..].find("!~") {
			Some(end) => rest = &rest[start + 2 + end + 2..],
			None => return out,
		}
	}

	out.push_str(rest);
	out
}

/// Removes HTML tags, turning `<br>` and paragraph tags into line breaks
fn strip_html_tags(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	let mut rest = input;

	while let Some(start) = rest.find('<') {
		let Some(len) = rest[start..].find('>') else {
			break;
		};
		out.push_str(&rest[..start]);

		let inner = &rest[start + 1..start + len];
		let name = inner
			.trim()
			.trim_start_matches('/')
			.split(|c: char| c.is_whitespace() || c == '/')
			.next()
			.unwrap_or_default()
			.to_ascii_lowercase();
		if name == "br" || name == "p" {
			out.push('\n');
		}

		rest = &rest[start + len + 1..];
	}

	out.push_str(rest);
	out
}

fn decode_entities(input: &str) -> String {
	input
		.replace("&nbsp;", " ")
		.replace("&quot;", "\"")
		.replace("&#39;", "'")
		.replace("&#x27;", "'")
		.replace("&apos;", "'")
		.replace("&lt;", "<")
		.replace("&gt;", ">")
		.replace("&mdash;", "—")
		.replace("&ndash;", "–")
		.replace("&hellip;", "…")
		// Last, so that "&amp;lt;" becomes "&lt;" rather than "<"
		.replace("&amp;", "&")
}

fn collapse_blank_lines(input: &str) -> String {
	let mut lines: Vec<&str> = Vec::new();
	for line in input.lines().map(str::trim) {
		if line.is_empty() && lines.last().is_none_or(|last| last.is_empty()) {
			continue;
		}
		lines.push(line);
	}
	lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Shape matches a real AniList `Page.media` item (trimmed for brevity)
	const BERSERK_SEARCH: &str = r#"{
		"Page": { "media": [{
			"id": 30002,
			"title": { "romaji": "Berserk", "english": "Berserk", "native": "ベルセルク" },
			"synonyms": ["Berserk Max", "berserk"],
			"description": "A feared warrior.<br><br>(Source: Dark Horse)<br><i>Notes:</i>&nbsp;&quot;x&quot; ~!Guts dies!~ end",
			"status": "RELEASING",
			"startDate": { "year": 1989 },
			"endDate": { "year": null },
			"volumes": null,
			"genres": ["Action", "Drama"],
			"tags": [
				{ "name": "Tragedy", "isMediaSpoiler": false, "isAdult": false },
				{ "name": "Rape", "isMediaSpoiler": true, "isAdult": true }
			],
			"isAdult": false,
			"staff": { "edges": [
				{ "role": "Story & Art (vols 1-41)", "node": { "name": { "full": "Kentarou Miura", "native": "三浦建太郎" } } },
				{ "role": "Supervisor (vols 41- )", "node": { "name": { "full": "Kouji Mori", "native": null } } },
				{ "role": "Story & Art (vols 41- )", "node": { "name": { "full": "Studio Gaga", "native": null } } },
				{ "role": "Translator (Spanish)", "node": { "name": { "full": "Marc Bernabé", "native": null } } }
			] },
			"coverImage": { "extraLarge": "https://example.test/xl.jpg", "large": "https://example.test/l.jpg" }
		}] }
	}"#;

	fn berserk() -> Media {
		let data: SearchData = serde_json::from_str(BERSERK_SEARCH).unwrap();
		data.page.media.into_iter().next().unwrap()
	}

	#[test]
	fn maps_series_fields() {
		let series = map_series(&berserk());

		assert_eq!(series.provider, "anilist");
		assert_eq!(series.external_id, "30002");
		assert_eq!(series.title, "Berserk");
		assert_eq!(series.alternative_titles, vec!["ベルセルク", "Berserk Max"]);
		assert_eq!(series.status, Some(PublicationStatus::Ongoing));
		assert_eq!(series.year, Some(1989));
		assert_eq!(series.end_year, None);
		assert_eq!(series.volume_count, None);
		assert_eq!(series.genres, Some(vec!["Action".into(), "Drama".into()]));
		assert_eq!(series.tags, Some(vec!["Tragedy".into()]));
		assert_eq!(series.age_rating, None);
		assert_eq!(
			series.authors,
			Some(vec!["Kentarou Miura".into(), "Studio Gaga".into()])
		);
		assert_eq!(
			series.artists,
			Some(vec!["Kentarou Miura".into(), "Studio Gaga".into()])
		);
		assert_eq!(
			series.cover_url.as_deref(),
			Some("https://example.test/xl.jpg")
		);
	}

	#[test]
	fn summary_hides_spoilers_and_markup() {
		let summary = map_series(&berserk()).summary.unwrap();

		assert!(summary.starts_with("A feared warrior.\n\n(Source: Dark Horse)"));
		assert!(summary.contains("\"x\""));
		assert!(!summary.contains("Guts dies"));
		assert!(!summary.contains('<'));
	}

	#[test]
	fn preferred_title_falls_back() {
		let only_native = MediaTitle {
			romaji: None,
			english: Some("  ".into()),
			native: Some("ベルセルク".into()),
		};
		assert_eq!(preferred_title(&only_native), Some("ベルセルク"));

		let none = MediaTitle {
			romaji: None,
			english: None,
			native: None,
		};
		assert_eq!(preferred_title(&none), None);
	}

	#[test]
	fn alternative_titles_are_deduplicated() {
		let mut media = berserk();
		media.title.romaji = Some("BERSERK".into());
		media.synonyms = Some(vec!["berserk".into(), "Berserk Max".into(), "".into()]);

		let alts = alternative_titles(&media, "Berserk");
		assert_eq!(alts, vec!["ベルセルク", "Berserk Max"]);
	}

	#[test]
	fn maps_every_known_status() {
		let cases = [
			(MediaStatus::Finished, Some(PublicationStatus::Completed)),
			(MediaStatus::Releasing, Some(PublicationStatus::Ongoing)),
			(
				MediaStatus::NotYetReleased,
				Some(PublicationStatus::Upcoming),
			),
			(MediaStatus::Cancelled, Some(PublicationStatus::Cancelled)),
			(MediaStatus::Hiatus, Some(PublicationStatus::Hiatus)),
			(MediaStatus::Unknown, None),
		];
		for (input, expected) in cases {
			assert_eq!(map_status(Some(input)), expected);
		}
		assert_eq!(map_status(None), None);
	}

	#[test]
	fn unknown_status_string_deserializes_to_unknown() {
		let status: MediaStatus = serde_json::from_str("\"SOMETHING_NEW\"").unwrap();
		assert_eq!(map_status(Some(status)), None);
	}

	#[test]
	fn staff_roles_are_matched_by_word() {
		let staff = StaffConnection {
			edges: Some(vec![
				staff_edge("Story (vols 1-3)", "Writer One"),
				staff_edge("Art Assistant", "Assistant"),
				staff_edge("Original Creator", "Creator"),
				staff_edge("Translator (English)", "Translator"),
				staff_edge("Editor", "Editor"),
				staff_edge("Supervisor", "Supervisor"),
			]),
		};

		let (writers, artists) = classify_staff(Some(&staff));
		assert_eq!(writers, vec!["Writer One", "Creator"]);
		assert_eq!(artists, vec!["Assistant"]);
	}

	#[test]
	fn localisation_credits_are_not_authors_or_artists() {
		// Real roles from AniList's Berserk entry
		let staff = StaffConnection {
			edges: Some(vec![
				staff_edge("Story & Art (vols 1-41)", "Kentarou Miura"),
				staff_edge(
					"Touch-up Art & Lettering (English: vols 1-8)",
					"Dan Nakrosis",
				),
				staff_edge("Lettering (Portuguese)", "Di\u{f3}genes Dih"),
				staff_edge("Editing (Portuguese)", "Diego Rodeguero"),
				staff_edge("Translator (Spanish)", "Marc Bernab\u{e9}"),
			]),
		};

		let (writers, artists) = classify_staff(Some(&staff));
		assert_eq!(writers, vec!["Kentarou Miura"]);
		assert_eq!(artists, vec!["Kentarou Miura"]);
	}

	#[test]
	fn staff_without_names_is_skipped() {
		let staff = StaffConnection {
			edges: Some(vec![StaffEdge {
				role: Some("Story".into()),
				node: Some(StaffNode {
					name: Some(StaffName {
						full: None,
						native: Some("   ".into()),
					}),
				}),
			}]),
		};
		let (writers, artists) = classify_staff(Some(&staff));
		assert!(writers.is_empty());
		assert!(artists.is_empty());
	}

	#[test]
	fn tags_drop_spoiler_and_adult() {
		let tags = vec![
			MediaTag {
				name: "Keep".into(),
				is_media_spoiler: Some(false),
				is_adult: None,
			},
			MediaTag {
				name: "Spoiler".into(),
				is_media_spoiler: Some(true),
				is_adult: None,
			},
			MediaTag {
				name: "Adult".into(),
				is_media_spoiler: None,
				is_adult: Some(true),
			},
		];
		assert_eq!(map_tags(Some(&tags)), Some(vec!["Keep".into()]));
		assert_eq!(map_tags(None), None);
	}

	#[test]
	fn unterminated_spoiler_hides_remainder() {
		assert_eq!(strip_spoilers("Start ~!never closed"), "Start ");
	}

	#[test]
	fn description_cleanup_handles_breaks_and_entities() {
		assert_eq!(
			clean_description("One<br />Two<br><br><br>Three &amp;lt; &#39;x&#39;"),
			Some("One\nTwo\n\nThree &lt; 'x'".to_string())
		);
		assert_eq!(clean_description("<br> <i></i>  "), None);
	}

	#[test]
	fn search_title_normalisation_strips_release_noise() {
		let cases = [
			(
				"Shoulder-a-Coffin Kuro v01-07 [Uasaha+Rurika] (Yen Press)",
				"Shoulder-a-Coffin Kuro",
			),
			("Fare L\u{2019}amore [Media Do]", "Fare L\u{2019}amore"),
			(
				"Drowning Love (2017-2020) (Digital) (danke-Empire)",
				"Drowning Love",
			),
			(
				"\u{d4}oku - The Inner Chambers (2009-2022) (Digital) (1r0n)",
				"\u{d4}oku - The Inner Chambers",
			),
			("Joker e [eng] [AB]", "Joker e"),
			("Sample Series Archived", "Sample Series"),
			("Vol. 3 Foo v1", "Foo"),
		];

		for (input, expected) in cases {
			assert_eq!(
				t::normalize_release_title(input).as_deref(),
				Some(expected).filter(|e| *e != input),
				"input: {input}"
			);
		}
	}

	#[test]
	fn diacritics_fold_for_search() {
		assert_eq!(
			t::fold_diacritics("\u{d4}oku - The Inner Chambers"),
			"Ooku - The Inner Chambers"
		);
		assert_eq!(t::fold_diacritics("Berserk"), "Berserk");
		assert_eq!(t::fold_diacritics("Pok\u{e9}mon"), "Pokemon");
	}

	#[test]
	fn search_variants_are_ordered_and_deduplicated() {
		let variants = search_variants(
			"\u{d4}oku - The Inner Chambers (2009-2022) (Digital) (1r0n)",
		);
		assert_eq!(
			variants,
			vec![
				"\u{d4}oku - The Inner Chambers (2009-2022) (Digital) (1r0n)",
				"\u{d4}oku - The Inner Chambers",
				"Ooku - The Inner Chambers",
			]
		);
		assert_eq!(search_variants("Berserk"), vec!["Berserk"]);
	}

	#[test]
	fn release_noise_words_are_stripped() {
		assert_eq!(
			t::normalize_release_title("Sample Series Archived").as_deref(),
			Some("Sample Series")
		);
		assert_eq!(
			t::normalize_release_title("Best Manga Digital Scanlated").as_deref(),
			Some("Best Manga")
		);
	}

	#[test]
	fn search_title_normalisation_returns_none_when_unchanged() {
		assert_eq!(t::normalize_release_title("Berserk"), None);
		assert_eq!(t::normalize_release_title("  Berserk  "), None);
		assert_eq!(t::normalize_release_title("[Group]"), None);
	}

	#[test]
	fn adult_series_is_labelled() {
		let mut media = berserk();
		media.is_adult = Some(true);
		assert_eq!(map_series(&media).age_rating.as_deref(), Some("Adult"));
	}

	fn staff_edge(role: &str, name: &str) -> StaffEdge {
		StaffEdge {
			role: Some(role.into()),
			node: Some(StaffNode {
				name: Some(StaffName {
					full: Some(name.into()),
					native: None,
				}),
			}),
		}
	}

	mod live {
		use super::*;

		fn client() -> AnilistClient {
			AnilistClient::new()
		}

		#[ignore = "Requires network access to graphql.anilist.co"]
		#[tokio::test]
		async fn test_search_series() {
			let query = SearchQuery {
				title: "Berserk".to_string(),
				limit: Some(5),
				..Default::default()
			};

			let outcome = client().search_series(&query).await.unwrap();
			assert!(!outcome.candidates.is_empty());
			assert_eq!(outcome.failed(), 0);
		}

		#[ignore = "Requires network access to graphql.anilist.co"]
		#[tokio::test]
		async fn test_fetch_series_metadata() {
			let meta = client().fetch_series_metadata("30002").await.unwrap();
			assert_eq!(meta.title, "Berserk");
		}

		#[ignore = "Requires network access to graphql.anilist.co"]
		#[tokio::test]
		async fn test_fetch_series_not_found() {
			let result = client().fetch_series_metadata("999999999").await;
			assert!(matches!(result, Err(MetadataProviderError::NotFound(_))));
		}

		#[ignore = "Requires network access to graphql.anilist.co"]
		#[tokio::test]
		async fn test_verify_credentials() {
			let verification = client().verify_credentials().await.unwrap();
			assert!(verification.is_valid);
		}
	}
}
