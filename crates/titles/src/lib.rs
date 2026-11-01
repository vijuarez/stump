//! Helpers for turning on-disk names into something presentable and searchable.
//!
//! Release names are routinely polluted with scanlation groups, edition tags, and
//! catalogue IDs (e.g. `38426-Love Roma v01 c01-04_MangaScreener`). These helpers strip
//! that noise, and can also express a book in terms of the volume and chapter markers
//! found in its name.

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// Standalone tokens which carry no part of a title
const NOISE_WORDS: &[&str] = &[
	"archived",
	"scanlated",
	"scanlation",
	"scan",
	"scans",
	"digital",
	"complete",
	"completed",
	"eng",
	"raw",
	"raws",
	"hq",
	"lq",
	"zip",
	"cbz",
	"cbr",
	"epub",
];

/// Folds accented characters to their ASCII equivalents
pub fn fold_diacritics(input: &str) -> String {
	let decomposed: String = input.nfd().filter(|c| !is_combining_mark(*c)).collect();

	// A few letters do not decompose into a base letter plus a combining mark
	decomposed
		.chars()
		.map(|c| match c {
			'\u{00df}' => "ss".to_string(),
			'\u{00e6}' => "ae".to_string(),
			'\u{00c6}' => "AE".to_string(),
			'\u{00f8}' => "o".to_string(),
			'\u{00d8}' => "O".to_string(),
			'\u{0153}' => "oe".to_string(),
			'\u{0152}' => "OE".to_string(),
			'\u{0142}' => "l".to_string(),
			'\u{0141}' => "L".to_string(),
			other => other.to_string(),
		})
		.collect()
}

/// Strips scanlation and release noise from a name so it can be searched.
///
/// This is deliberately aggressive: every bracketed group is dropped, including
/// parenthesised ones, because search engines do not know about release conventions.
///
/// Handles names like:
/// - `Shoulder-a-Coffin Kuro v01-07 [Uasaha+Rurika] (Yen Press)` -> `Shoulder-a-Coffin Kuro`
/// - `Fare L\u{2019}amore [Media Do]` -> `Fare L\u{2019}amore`
/// - `Drown Yo (2017-2020) (Digital) (danke-Empire)` -> `Drown Yo`
///
/// Returns `None` when nothing meaningful was removed.
pub fn normalize_release_title(input: &str) -> Option<String> {
	let without_ids = remove_catalogue_id(input);
	let without_groups = remove_all_bracket_groups(&without_ids);
	let without_volumes = remove_volume_markers(&without_groups);
	let without_noise = remove_release_noise_words(&without_volumes);
	let cleaned = collapse_whitespace(&without_noise);

	if cleaned.is_empty() || cleaned == input.trim() {
		None
	} else {
		Some(cleaned)
	}
}

/// Builds a display name for a book from its file name.
///
/// Volume and chapter markers are the most useful thing a release name carries, so they
/// are expressed in a consistent way, e.g.
/// - `Love Roma v01 c01-04_MangaScreener` -> `Vol. 1 (Ch. 1-4)`
/// - `90002-Love_Roma_v02BYAAAH` -> `Vol. 2`
/// - `Berserk - c042` -> `Ch. 42`
///
/// Names without any such markers fall back to a cleaned version of the file name, and
/// names which clean up to nothing are returned as-is.
pub fn media_display_name(file_stem: &str) -> String {
	let volume = parse_volume(file_stem);
	let chapters = parse_chapters(file_stem);

	let volume_part = volume.map(|v| format!("Vol. {v}"));
	let chapter_part = chapters.map(|(first, last)| match last {
		Some(last) if last != first => format!("Ch. {first}-{last}"),
		_ => format!("Ch. {first}"),
	});

	let parts = [volume_part, chapter_part]
		.into_iter()
		.flatten()
		.collect::<Vec<_>>();

	if parts.is_empty() {
		let cleaned = clean_file_stem(file_stem);
		return if cleaned.is_empty() {
			file_stem.trim().to_string()
		} else {
			cleaned
		};
	}

	parts
		.iter()
		.enumerate()
		.map(|(idx, part)| {
			if idx == 0 {
				part.clone()
			} else {
				format!("({part})")
			}
		})
		.collect::<Vec<_>>()
		.join(" ")
}

/// Cleans a file stem for display: drops catalogue IDs, bracketed groups, trailing
/// edition tags, and turns underscore separators into spaces
///
/// Unlike [`normalize_release_title`], this keeps parenthesised text which looks like it
/// belongs to the title, e.g. `Pink (Kyoko Okazaki)`.
pub fn clean_file_stem(file_stem: &str) -> String {
	let without_ids = remove_catalogue_id(file_stem);
	let without_square = remove_square_bracket_groups(&without_ids);
	let without_edition = remove_edition_groups(&without_square);
	let without_noise = remove_release_noise_words(&without_edition);
	let spaced = without_noise.replace('_', " ");

	trim_separators(&collapse_whitespace(&spaced))
}

/// Removes leading catalogue IDs such as `38426-` or `117634_`
fn remove_catalogue_id(input: &str) -> String {
	let trimmed = input.trim_start();
	let end = trimmed
		.find(|c: char| !c.is_ascii_digit())
		.unwrap_or(trimmed.len());

	if end < 4 {
		return input.to_string();
	}

	let (digits, rest) = trimmed.split_at(end);
	let rest = rest.trim_start_matches(['-', '_', '.', ' ', ':']);

	if digits.len() >= 4 && !rest.trim().is_empty() {
		rest.to_string()
	} else {
		input.to_string()
	}
}

/// Removes `[...]` and `(...)` groups, including nested ones
fn remove_all_bracket_groups(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	let mut depth = 0usize;

	for c in input.chars() {
		match c {
			'[' | '(' => depth += 1,
			']' | ')' => depth = depth.saturating_sub(1),
			_ if depth == 0 => out.push(c),
			_ => {},
		}
	}

	out
}

/// Removes `[...]` groups, including nested ones
fn remove_square_bracket_groups(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	let mut depth = 0usize;

	for c in input.chars() {
		match c {
			'[' => depth += 1,
			']' => depth = depth.saturating_sub(1),
			_ if depth == 0 => out.push(c),
			_ => {},
		}
	}

	out
}

/// Removes parenthesised release information such as `(2013)`, `(Digital)`, or `(danke-Empire)`
///
/// Release names follow a rough convention: the title comes first, then edition tags and the
/// scanlation group, each in their own parentheses. So when a name has several groups, only the
/// first one can plausibly be part of the title.
fn remove_edition_groups(input: &str) -> String {
	let segments = split_parenthesised(input);
	let groups = segments.iter().filter(|s| s.is_group).count();

	let mut out = String::with_capacity(input.len());
	let mut group_index = 0usize;
	for segment in segments.iter() {
		if !segment.is_group {
			out.push_str(&segment.text);
			continue;
		}

		let drop = is_edition_group(&segment.text)
			|| (groups >= 2 && group_index > 0)
			|| (groups == 1 && looks_like_group_handle(&segment.text));
		group_index += 1;

		if !drop {
			out.push('(');
			out.push_str(&segment.text);
			out.push(')');
		}
	}

	out
}

struct Segment {
	text: String,
	is_group: bool,
}

/// Splits a name into plain text and parenthesised groups, keeping the groups' contents
fn split_parenthesised(input: &str) -> Vec<Segment> {
	let mut segments = Vec::new();
	let mut text = String::new();
	let mut group = String::new();
	let mut depth = 0usize;

	for c in input.chars() {
		match c {
			'(' => {
				depth += 1;
				if depth == 1 {
					if !text.is_empty() {
						segments.push(Segment {
							text: std::mem::take(&mut text),
							is_group: false,
						});
					}
					continue;
				}
			},
			')' => {
				if depth == 0 {
					continue;
				}
				depth -= 1;
				if depth == 0 {
					segments.push(Segment {
						text: std::mem::take(&mut group),
						is_group: true,
					});
					continue;
				}
			},
			_ => {},
		}

		if depth == 0 {
			text.push(c);
		} else {
			group.push(c);
		}
	}

	if !text.is_empty() {
		segments.push(Segment {
			text,
			is_group: false,
		});
	}

	segments
}

/// True for a lone parenthetical that looks like a scanlation group handle rather than a title
/// part, e.g. `(kickthekancho)` or `(apocalyp7e)`
fn looks_like_group_handle(group: &str) -> bool {
	let trimmed = group.trim();
	if trimmed.is_empty() || trimmed.split_whitespace().count() > 1 {
		return false;
	}

	let has_digit = trimmed.chars().any(|c| c.is_ascii_digit());
	let has_letter = trimmed.chars().any(|c| c.is_alphabetic());
	let all_lower = trimmed
		.chars()
		.all(|c| !c.is_alphabetic() || c.is_ascii_lowercase());

	has_letter && (has_digit || all_lower)
}

/// True for parenthesised content which is edition information rather than part of a title
fn is_edition_group(group: &str) -> bool {
	let bare = group.trim().to_lowercase();

	if bare.is_empty() {
		return false;
	}

	// A bare year, or a year range such as `2017-2020`
	if bare
		.split('-')
		.all(|part| part.len() == 4 && part.chars().all(|c| c.is_ascii_digit()))
	{
		return true;
	}

	bare.split(|c: char| !c.is_alphanumeric())
		.filter(|token| !token.is_empty())
		.any(|token| NOISE_WORDS.contains(&token))
}

/// Removes standalone release tags such as `Archived`, `Scanlated`, or `Digital`
fn remove_release_noise_words(input: &str) -> String {
	input
		.split_whitespace()
		.filter(|token| {
			let bare = token
				.trim_matches(|c: char| !c.is_alphanumeric())
				.to_lowercase();
			!NOISE_WORDS.contains(&bare.as_str())
		})
		.collect::<Vec<_>>()
		.join(" ")
}

/// Removes volume markers such as `v01-07`, `v1`, `vol. 3`, and `volume 3-5`
fn remove_volume_markers(input: &str) -> String {
	let mut out: Vec<String> = Vec::new();
	let mut skip_number = false;

	for token in input.split_whitespace() {
		let bare = token
			.trim_matches(|c: char| !c.is_alphanumeric())
			.to_lowercase();

		if skip_number && bare.chars().all(|c| c.is_ascii_digit() || c == '-') {
			skip_number = false;
			continue;
		}
		skip_number = false;

		if bare == "vol" || bare == "vols" || bare == "volume" || bare == "volumes" {
			skip_number = true;
			continue;
		}
		if is_volume_token(&bare) {
			continue;
		}

		out.push(token.to_string());
	}

	out.join(" ")
}

/// True for tokens like `v01`, `v1`, `v01-07`, or `v01-04`
fn is_volume_token(bare: &str) -> bool {
	let Some(rest) = bare.strip_prefix('v') else {
		return false;
	};

	!rest.is_empty()
		&& rest
			.chars()
			.all(|c| c.is_ascii_digit() || c == '-' || c == '_')
		&& rest.chars().any(|c| c.is_ascii_digit())
}

/// Parses the volume number out of a name, e.g. `v01`, `vol. 3`, `Volume 12`, or `(v02)`
pub fn parse_volume(input: &str) -> Option<u32> {
	scan_marker(input, &["volume", "vol", "v"], false).map(|(volume, _)| volume)
}

/// Parses a chapter or chapter range out of a name, e.g. `c001-006`, `ch. 12`, or `Chapter 5`
///
/// Chapter markers must be explicit (`c`, `ch`, `chap`, `chapter`). A bare number is not
/// enough, since release names often lead with a catalogue ID like `38426-`.
pub fn parse_chapters(input: &str) -> Option<(u32, Option<u32>)> {
	scan_marker(input, &["chapters", "chapter", "chap", "ch", "c"], true)
}

/// True when whatever follows a chapter number still reads as that number, i.e. it is a
/// separator, the end of the name, or a version suffix like `v2`
fn has_clean_suffix(after: &str) -> bool {
	let Some(next) = after.chars().next() else {
		return true;
	};

	if !next.is_ascii_alphabetic() {
		return true;
	}

	let mut chars = after.chars();
	chars.next();
	let is_version =
		matches!(next, 'v' | 'V') && chars.next().is_some_and(|c| c.is_ascii_digit());

	is_version
}

/// Scans for `keyword` at a word boundary followed by a number, optionally parsing a trailing
/// range (`-4`, `-v02`) when `allow_range` is set
fn scan_marker(
	input: &str,
	keywords: &[&str],
	allow_range: bool,
) -> Option<(u32, Option<u32>)> {
	let lower = input.to_lowercase();

	for (pos, _) in lower.char_indices() {
		let at_boundary = pos == 0
			|| lower[..pos]
				.chars()
				.next_back()
				.is_some_and(|c| !c.is_alphanumeric());

		if !at_boundary {
			continue;
		}

		let Some(keyword) = keywords.iter().find(|kw| lower[pos..].starts_with(**kw))
		else {
			continue;
		};

		let rest = &lower[pos + keyword.len()..];
		let rest = rest.trim_start_matches(['.', ':', ' ']);
		let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();

		if digits.is_empty() {
			continue;
		}

		let Ok(first) = digits.parse::<u32>() else {
			continue;
		};

		if !allow_range {
			return Some((first, None));
		}

		let after = &rest[digits.len()..];

		// Reject glued suffixes such as `(c2c)` (cover-to-cover) while still accepting
		// version suffixes like `c04v2`
		if !has_clean_suffix(after) {
			continue;
		}

		let last = after
			.strip_prefix('-')
			.map(|tail| {
				let tail = tail.trim_start_matches(['c', 'h', 'v', 'o', 'l', '.', ' ']);
				tail.chars()
					.take_while(|c| c.is_ascii_digit())
					.collect::<String>()
			})
			.filter(|tail| !tail.is_empty())
			.and_then(|tail| tail.parse::<u32>().ok());

		return Some((first, last));
	}

	None
}

fn collapse_whitespace(input: &str) -> String {
	input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn trim_separators(input: &str) -> String {
	input
		.trim()
		.trim_matches(['-', '_', '.', ',', ':'])
		.split_whitespace()
		.collect::<Vec<_>>()
		.join(" ")
		.trim_end_matches(['-', '_', '.', ',', ':'])
		.to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn release_noise_is_removed() {
		assert_eq!(
			normalize_release_title("Sample Series Archived").as_deref(),
			Some("Sample Series")
		);
		assert_eq!(
			normalize_release_title("Best Manga Digital Scanlated").as_deref(),
			Some("Best Manga")
		);
	}

	#[test]
	fn normalisation_returns_none_when_unchanged() {
		assert_eq!(normalize_release_title("Berserk"), None);
		assert_eq!(normalize_release_title("  Berserk  "), None);
		assert_eq!(normalize_release_title("[Group]"), None);
	}

	#[test]
	fn search_normalisation_drops_every_bracket_group() {
		assert_eq!(
			normalize_release_title("Pink (Kyoko Okazaki) [1989]").as_deref(),
			Some("Pink")
		);
		// display cleaning is more careful, and keeps the author
		assert_eq!(
			clean_file_stem("Pink (Kyoko Okazaki) [1989]"),
			"Pink (Kyoko Okazaki)"
		);
	}

	#[test]
	fn diacritics_fold() {
		assert_eq!(fold_diacritics("\u{d4}oku"), "Ooku");
		assert_eq!(fold_diacritics("Le Th\u{e9}\u{e2}tre"), "Le Theatre");
	}

	#[test]
	fn volume_and_chapter_markers_are_parsed() {
		assert_eq!(parse_volume("Love Roma v01 c01-04"), Some(1));
		assert_eq!(parse_volume("90002-Love_Roma_v02BYAAAH"), Some(2));
		assert_eq!(parse_volume("Vol. 3 - Something"), Some(3));
		assert_eq!(parse_volume("Berserk"), None);

		assert_eq!(parse_chapters("Berserk - c042"), Some((42, None)));
		assert_eq!(parse_chapters("Love Roma v01 c01-04"), Some((1, Some(4))));
		assert_eq!(parse_chapters("Something"), None);
	}

	#[test]
	fn cover_to_cover_is_not_a_chapter() {
		assert_eq!(parse_chapters("A Dream (2010) (c2c) (Trite)"), None);
		assert_eq!(parse_chapters("Something c04v2"), Some((4, None)));
	}

	#[test]
	fn book_names_prefer_volume_markers() {
		assert_eq!(
			media_display_name("Love Roma v01 c01-04_MangaScreener"),
			"Vol. 1 (Ch. 1-4)"
		);
		assert_eq!(media_display_name("90002-Love_Roma_v02BYAAAH"), "Vol. 2");
		assert_eq!(media_display_name("Berserk - c042"), "Ch. 42");
	}

	#[test]
	fn book_names_fall_back_to_a_cleaned_file_name() {
		assert_eq!(
			media_display_name("117634-Banana_Bread_Pudding_"),
			"Banana Bread Pudding"
		);
		assert_eq!(
			media_display_name("I_Say_A_Little_Prayer_[lililicious]"),
			"I Say A Little Prayer"
		);
		assert_eq!(media_display_name("Ayako (2013) (digital)"), "Ayako");
		assert_eq!(media_display_name("Paranoia Street"), "Paranoia Street");
	}

	#[test]
	fn author_in_parentheses_survives_cleaning() {
		assert_eq!(
			media_display_name("Pink (Kyoko Okazaki)"),
			"Pink (Kyoko Okazaki)"
		);
	}
}
