//! Client for the Otherman Records release API.
//!
//! The list endpoint returns `{"count": N, "0": {...}, "1": {...}, ...}`, so the
//! page payload is decoded as a JSON object and the non-`count` members are
//! collected in key order.

use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use crate::stream::{self, StreamingRead};

const BASE_URL: &str = "https://www.otherman-records.com/index.php/api/releases";
pub const RELEASE_LINK_BASE: &str = "https://www.otherman-records.com/releases/";

const PAGE_SIZE: usize = 12;

/// Ceilings on how long a request may take. Without them a stalled connection
/// leaves the UI showing "Loading…" indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const METADATA_TIMEOUT: Duration = Duration::from_secs(20);
/// Covers a whole track fetch. The body is streamed, but it is still pulled as
/// fast as the network allows rather than in real time, so a few megabytes
/// should be well inside this.
const TRACK_TIMEOUT: Duration = Duration::from_secs(300);
/// How much of a track to buffer before handing it to the player.
///
/// Roughly six seconds of a 320 kbps MP3, which is the cushion the playhead has
/// if the network briefly falls behind. Tiny next to a whole track, and the
/// request itself costs far more than fetching it.
const PREBUFFER_BYTES: usize = 256 * 1024;

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

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(timeout))
        .build()
        .new_agent()
}

/// Shared agent for the JSON endpoints, so the connection pool is reused across
/// the ten-odd list pages.
fn metadata_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| agent(METADATA_TIMEOUT))
}

fn fetch_page(page: usize) -> Result<serde_json::Value, Error> {
    let url = format!("{BASE_URL}/list/{page}/sort/release-asc");
    Ok(metadata_agent().get(&url).call()?.body_mut().read_json()?)
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
    Ok(metadata_agent().get(&url).call()?.body_mut().read_json()?)
}

/// Track URLs come back protocol-relative (`//archive.org/...`).
pub fn absolute_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

/// Starts streaming a track and returns a reader the decoder can begin on
/// straight away.
///
/// The body is pumped on a thread of its own, so this returns once
/// [`PREBUFFER_BYTES`] have landed rather than once the whole track has. The
/// download stops by itself when the reader is dropped.
pub fn stream(url: &str) -> Result<StreamingRead, Error> {
    let response = agent(TRACK_TIMEOUT).get(url).call()?;
    let byte_len = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok());

    let (reader, writer) = stream::channel(byte_len);
    // An owned reader, so the pump thread can outlive this call. The default
    // read limit is meant for whole-body reads; `crate::stream` caps the buffer
    // itself.
    let body = response
        .into_body()
        .into_with_config()
        .limit(u64::MAX)
        .reader();
    std::thread::Builder::new()
        .name("track-download".to_owned())
        .spawn(move || writer.pump(body))?;

    reader.wait_for(PREBUFFER_BYTES)?;
    Ok(reader)
}
