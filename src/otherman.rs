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

/// Turns a track URL from the API into one that can actually be requested.
///
/// The API hands these back in three shapes, and a release will happily mix
/// them: protocol-relative (`//archive.org/...`), absolute and already
/// percent-encoded, and — this is the one that used to fail — protocol-relative
/// with the file name left raw, spaces and all. A raw one never reached the
/// network at all: `ureq` rejected it as `invalid uri character` before opening
/// a connection, so every track on such a release was silently unplayable.
///
/// Encoding has to leave the already-encoded ones alone, because `%20` run
/// through a naive encoder becomes `%2520` and asks for a file that does not
/// exist. So a `%` that already introduces a valid escape is passed through
/// whole, and only a stray one is encoded.
pub fn absolute_url(url: &str) -> String {
    let absolute = match url.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    };

    // Only the part after the authority is escaped; a scheme and a host have
    // their own rules and arrive well-formed.
    let split = absolute
        .find("://")
        .and_then(|scheme| absolute[scheme + 3..].find('/').map(|at| scheme + 3 + at));
    let Some(split) = split else {
        return absolute;
    };
    let (head, path) = absolute.split_at(split);
    format!("{head}{}", escape_path(path))
}

/// Percent-encodes what a URL path may not carry literally, passing existing
/// escapes through untouched.
fn escape_path(path: &str) -> String {
    /// `pchar` plus the delimiters that separate a path from a query, none of
    /// which should be touched where they appear.
    const KEEP: &str = "-._~!$&'()*+,;=:@/?#";

    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let escaped = byte == b'%'
            && bytes
                .get(index + 1..index + 3)
                .is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit));
        if escaped {
            out.push_str(&path[index..index + 3]);
            index += 3;
        } else if byte.is_ascii_alphanumeric() || KEEP.as_bytes().contains(&byte) {
            out.push(byte as char);
            index += 1;
        } else {
            out.push_str(&format!("%{byte:02X}"));
            index += 1;
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_protocol_relative_url_gains_a_scheme() {
        assert_eq!(
            absolute_url("//www.archive.org/download/OTMN001/01_Ca5_-_cyberSP.mp3"),
            "https://www.archive.org/download/OTMN001/01_Ca5_-_cyberSP.mp3"
        );
    }

    #[test]
    fn a_raw_path_is_escaped() {
        // The shape that used to be unplayable: spaces and non-ASCII straight
        // from the API, which `ureq` refuses as an invalid URI before it opens
        // a connection.
        let escaped = absolute_url("//example.org/download/OTMN083/01. A - 劇.mp3");
        assert_eq!(
            escaped,
            "https://example.org/download/OTMN083/01.%20A%20-%20%E5%8A%87.mp3"
        );
    }

    #[test]
    fn an_escaped_path_is_left_alone() {
        // Some releases come back already encoded. Encoding again would turn
        // `%20` into `%2520` and ask for a file that is not there.
        let url = "https://archive.org/download/OTMN100/01.%20bypass%20%26%20co.mp3";
        assert_eq!(absolute_url(url), url);
    }

    #[test]
    fn a_stray_percent_is_escaped() {
        // A percent that introduces nothing is a literal one, and has to be
        // encoded or it reads as the start of an escape that is not there.
        assert_eq!(
            absolute_url("https://example.org/a/100%25/b%zz/c"),
            "https://example.org/a/100%25/b%25zz/c"
        );
    }

    #[test]
    fn a_url_without_a_path_is_untouched() {
        assert_eq!(absolute_url("https://example.org"), "https://example.org");
    }

    /// Opt-in: the shape that started this, end to end.
    #[test]
    fn the_release_that_would_not_play_now_streams() {
        if std::env::var_os("OSM_SOUND_DEMO_NETWORK_TESTS").is_none() {
            eprintln!("skipped: set OSM_SOUND_DEMO_NETWORK_TESTS=1 to run");
            return;
        }
        let release = fetch_release("OTMN083").expect("the release");
        let track = release.tracklist.first().expect("a track");
        let url = absolute_url(&track.url);
        assert!(url.is_ascii(), "the request URL is still raw: {url}");
        let reader = stream(&url).expect("the track streams");
        assert!(reader.byte_len().is_some_and(|len| len > 0), "empty track");
    }
}
