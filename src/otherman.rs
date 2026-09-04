//! Client for the Otherman Records release API.
//!
//! The list endpoint returns `{"count": N, "0": {...}, "1": {...}, ...}`, so the
//! page payload is decoded as a JSON object and the non-`count` members are
//! collected in key order.

use serde::Deserialize;

const BASE_URL: &str = "https://www.otherman-records.com/index.php/api/releases";
pub const RELEASE_LINK_BASE: &str = "https://www.otherman-records.com/releases/";

const PAGE_SIZE: usize = 12;
/// Track downloads are held in memory before decoding; archive.org MP3s are a
/// few MB, so this is a generous ceiling rather than an expected size.
const MAX_TRACK_BYTES: u64 = 96 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct ListItem {
    pub id: String,
    #[serde(default)]
    pub artist1: String,
    #[serde(default)]
    pub artist2: String,
    #[serde(default)]
    pub title: String,
}

impl ListItem {
    /// Label shown in the release dropdown, mirroring the web demo.
    pub fn label(&self) -> String {
        format!(
            "[{}] {} / {} {}",
            self.id, self.title, self.artist1, self.artist2
        )
        .trim_end()
        .to_string()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    #[serde(default)]
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub id: String,
    #[serde(default)]
    pub artist1: String,
    #[serde(default)]
    pub artist2: String,
    #[serde(default)]
    pub tracklist: Vec<Track>,
}

impl Release {
    pub fn artists(&self) -> String {
        format!("{} {}", self.artist1, self.artist2)
            .trim_end()
            .to_string()
    }
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;

fn fetch_page(page: usize) -> Result<serde_json::Value, Error> {
    let url = format!("{BASE_URL}/list/{page}/sort/release-asc");
    Ok(ureq::get(&url).call()?.body_mut().read_json()?)
}

/// Fetches every release page and returns the flattened list.
pub fn fetch_all_releases() -> Result<Vec<ListItem>, Error> {
    let first = fetch_page(0)?;
    let count = first
        .get("count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let total_pages = count.div_ceil(PAGE_SIZE);

    let mut releases = items_of(&first);
    for page in 1..total_pages {
        releases.extend(items_of(&fetch_page(page)?));
    }
    Ok(releases)
}

/// Collects the numbered members of a list-endpoint payload, skipping `count`
/// and any entry that does not decode as a release summary. Keys are numeric
/// strings, so they are ordered numerically rather than lexicographically.
fn items_of(page: &serde_json::Value) -> Vec<ListItem> {
    let Some(object) = page.as_object() else {
        return Vec::new();
    };
    let mut numbered: Vec<(usize, ListItem)> = object
        .iter()
        .filter_map(|(key, value)| {
            let index = key.parse().ok()?;
            Some((index, serde_json::from_value(value.clone()).ok()?))
        })
        .collect();
    numbered.sort_by_key(|(index, _)| *index);
    numbered.into_iter().map(|(_, item)| item).collect()
}

pub fn fetch_release(id: &str) -> Result<Release, Error> {
    let url = format!("{BASE_URL}/id/{id}");
    Ok(ureq::get(&url).call()?.body_mut().read_json()?)
}

/// Track URLs come back protocol-relative (`//archive.org/...`).
pub fn absolute_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

/// Downloads a track into memory so rodio can decode it from a seekable cursor.
pub fn download(url: &str) -> Result<Vec<u8>, Error> {
    Ok(ureq::get(url)
        .call()?
        .body_mut()
        .with_config()
        .limit(MAX_TRACK_BYTES)
        .read_to_vec()?)
}
