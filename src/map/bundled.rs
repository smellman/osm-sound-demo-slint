//! Answers the style's glyph and sprite requests from data compiled into the
//! binary.
//!
//! The bundled style still names tile.openstreetmap.jp for its glyphs and
//! sprite, and the files under `style/fonts` and `style/sprites` are copies of
//! what that server serves (from osmfj/tileserver-gl-site). Keeping the URLs
//! means nothing here has to be right for the map to draw: a request this
//! module has no file for — a font range, or a style loaded from
//! `MAPLIBRE_STYLE_URL` — goes out to the network as it always did.

#[cfg(not(target_os = "android"))]
use maplibre_native_ffi::{ResourceProvider, RuntimeHandle};
use maplibre_native_ffi::{ResourceRequestHandle, ResourceResponse, ResourceResponseStatus};

/// Every bundled file, keyed by its path under `style/` and sorted by it.
/// Written by `build.rs`.
static FILES: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/bundled_files.rs"));

/// Where the style's URLs point, and the directory under `style/` that holds
/// the copies.
const ROUTES: [(&str, &str); 2] = [
    ("https://tile.openstreetmap.jp/fonts/", "fonts/"),
    (
        "https://tile.openstreetmap.jp/styles/maptiler-toner-ja/",
        "sprites/",
    ),
];

/// The bundled copy of `url`, if there is one.
pub fn lookup(url: &str) -> Option<&'static [u8]> {
    // A query string or fragment is not part of the file name.
    let url = url.split(['?', '#']).next().unwrap_or(url);
    let key = ROUTES.iter().find_map(|(prefix, dir)| {
        url.strip_prefix(prefix)
            .map(|rest| format!("{dir}{}", percent_decode(rest)))
    })?;
    FILES
        .binary_search_by(|(path, _)| (*path).cmp(key.as_str()))
        .ok()
        .map(|index| FILES[index].1)
}

/// Installs a provider that answers bundled files and passes everything else
/// to MapLibre's own networking. Android installs its own provider instead,
/// which asks [`answer`] first; see `fetch`.
///
/// Waits for the runtime to take it, so it is in place before the style load
/// sends the first request.
#[cfg(not(target_os = "android"))]
pub fn install(runtime: &RuntimeHandle) -> maplibre_native_ffi::Result<()> {
    let provider = ResourceProvider::new(|request, handle| {
        if let Err(handle) = answer(&request.resolved_url, handle)
            && let Err((_, error)) = handle.pass_through()
        {
            eprintln!("handing a map request to the network failed: {error}");
        }
    });
    let installed = runtime.set_resource_provider(provider)?;
    match installed.wait(std::time::Duration::from_secs(5)) {
        Ok(result) => result.map(|_| ()),
        Err(_) => {
            eprintln!("the runtime did not take the bundled-file provider in time");
            Ok(())
        }
    }
}

/// Answers `url` from the bundle if it can, completing `handle` inline. A URL
/// the bundle has no file for hands the handle back unanswered.
pub fn answer(url: &str, handle: ResourceRequestHandle) -> Result<(), ResourceRequestHandle> {
    let Some(bytes) = lookup(url) else {
        return Err(handle);
    };
    let response = ResourceResponse {
        status: ResourceResponseStatus::Ok,
        bytes,
        ..Default::default()
    };
    if let Err((_, error)) = handle.complete(&response) {
        eprintln!("completing a bundled map request failed: {error}");
    }
    Ok(())
}

/// Decodes `%XX` escapes. A font stack with a space in its name arrives as
/// `%20`; the bundled ones have none, but a stack that did would still match.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_for_binary_search() {
        assert!(FILES.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    #[test]
    fn glyphs_and_sprites_resolve_to_bundled_files() {
        for url in [
            "https://tile.openstreetmap.jp/fonts/migu1c-regular/0-255.pbf",
            "https://tile.openstreetmap.jp/fonts/migu2m-regular/12288-12543.pbf",
            "https://tile.openstreetmap.jp/styles/maptiler-toner-ja/sprite.json",
            "https://tile.openstreetmap.jp/styles/maptiler-toner-ja/sprite@2x.png",
        ] {
            assert!(lookup(url).is_some_and(|bytes| !bytes.is_empty()), "{url}");
        }
    }

    #[test]
    fn anything_else_goes_to_the_network() {
        for url in [
            "https://tile.openstreetmap.jp/fonts/Noto%20Sans%20Regular/0-255.pbf",
            "https://tile.openstreetmap.jp/data/takeshima.json",
            "https://tile.openstreetmap.jp/static/planet.pmtiles",
            "https://example.com/fonts/migu1c-regular/0-255.pbf",
        ] {
            assert!(lookup(url).is_none(), "{url}");
        }
    }

    #[test]
    fn escapes_and_queries_do_not_hide_a_file() {
        assert!(lookup("https://tile.openstreetmap.jp/fonts/migu1c%2Dregular/0-255.pbf").is_some());
        assert!(
            lookup("https://tile.openstreetmap.jp/styles/maptiler-toner-ja/sprite.png?v=1")
                .is_some()
        );
    }
}
