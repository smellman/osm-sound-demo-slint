//! Local Music: albums found under a music directory, `~/Music` by default.
//!
//! Only the desktop has one. The scan reads tags and nothing else — cover art
//! and stream properties are skipped — so a collection of a thousand-odd FLAC
//! files is listed in well under a second. Playback goes through the same
//! rodio player as a streamed track, which is what keeps the skyline moving: the
//! spectrum is taken from the samples on their way to the speaker.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use lofty::config::ParseOptions;
use lofty::prelude::*;
use lofty::probe::Probe;

/// Points Local Music at a directory other than `~/Music`.
const MUSIC_VAR: &str = "OSM_SOUND_DEMO_MUSIC";

/// What rodio is built to decode here; see the `rodio` features in Cargo.toml.
const PLAYABLE: [&str; 2] = ["flac", "mp3"];

#[derive(Debug, Clone, PartialEq)]
pub struct LocalTrack {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    disc: u32,
    number: u32,
}

impl LocalTrack {
    /// Label shown in the track dropdown.
    pub fn label(&self) -> String {
        if self.number > 0 {
            format!("{:02}. {}", self.number, self.title)
        } else {
            self.title.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalAlbum {
    pub title: String,
    pub artist: String,
    /// The directory holding the album's first track, which Open Folder shows.
    pub dir: PathBuf,
    pub tracks: Vec<LocalTrack>,
}

impl LocalAlbum {
    /// Label shown in the album dropdown, in the same shape as a release's.
    pub fn label(&self) -> String {
        format!("{} / {}", self.title, self.artist)
    }
}

/// The directory Local Music reads, if there is one to read.
///
/// `~/Music` is only assumed where it is a plain tree of files. On macOS it is
/// Music.app's library — its own database, its own layout, mostly AAC and ALAC
/// that rodio is not built to decode here — so there Local Music stays off
/// unless `OSM_SOUND_DEMO_MUSIC` names a directory. The phones have no music
/// directory the app can list at all.
pub fn music_dir() -> Option<PathBuf> {
    if cfg!(any(target_os = "android", target_os = "ios")) {
        return None;
    }
    let dir = match std::env::var_os(MUSIC_VAR).filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None if cfg!(target_os = "macos") => return None,
        None => PathBuf::from(std::env::var_os("HOME")?).join("Music"),
    };
    dir.is_dir().then_some(dir)
}

/// Every album under `dir`, sorted by artist and then title.
///
/// An album is the tracks that share an album artist (or, without one, an
/// artist) and an album title. A file with no album tag is grouped with the
/// others in its directory, named after it. Files that cannot be read are left
/// out rather than failing the scan.
pub fn scan(dir: &Path) -> Vec<LocalAlbum> {
    let mut files = Vec::new();
    collect(dir, &mut files);

    let mut albums: BTreeMap<(String, String, PathBuf), LocalAlbum> = BTreeMap::new();
    for path in files {
        let Some(tags) = read_tags(&path) else {
            continue;
        };
        let folder = path.parent().unwrap_or(dir).to_path_buf();
        let artist = tags
            .album_artist
            .clone()
            .or_else(|| tags.artist.clone())
            .unwrap_or_else(|| "Unknown Artist".to_owned());
        let (title, grouping) = match tags.album {
            // Grouped across directories, so a two-disc album split into
            // folders is still one album.
            Some(album) => (album, PathBuf::new()),
            None => (
                folder
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Unknown Album".to_owned()),
                folder.clone(),
            ),
        };
        let track = LocalTrack {
            title: tags.title.unwrap_or_else(|| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
            artist: tags.artist.unwrap_or_else(|| artist.clone()),
            disc: tags.disc,
            number: tags.number,
            path,
        };
        let key = (artist.to_lowercase(), title.to_lowercase(), grouping);
        albums
            .entry(key)
            .or_insert_with(|| LocalAlbum {
                title,
                artist,
                dir: folder,
                tracks: Vec::new(),
            })
            .tracks
            .push(track);
    }

    let mut albums: Vec<LocalAlbum> = albums.into_values().collect();
    for album in &mut albums {
        album
            .tracks
            .sort_by(|a, b| (a.disc, a.number, &a.path).cmp(&(b.disc, b.number, &b.path)));
        if let Some(first) = album.tracks.first().and_then(|track| track.path.parent()) {
            album.dir = first.to_path_buf();
        }
    }
    albums
}

/// Every album artist in `albums`, once each, in the order `scan` sorts them.
pub fn artists(albums: &[LocalAlbum]) -> Vec<&str> {
    let mut artists: Vec<&str> = Vec::new();
    for album in albums {
        if artists
            .last()
            .is_none_or(|last| !same_artist(last, &album.artist))
        {
            artists.push(&album.artist);
        }
    }
    artists
}

/// Indices into `albums` of `artist`'s albums, or of all of them for `None`.
pub fn by_artist(albums: &[LocalAlbum], artist: Option<&str>) -> Vec<usize> {
    (0..albums.len())
        .filter(|&index| artist.is_none_or(|artist| same_artist(artist, &albums[index].artist)))
        .collect()
}

/// Whether two album artists are one, the way `scan` groups them: ignoring
/// case.
fn same_artist(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Playable files under `dir`, at any depth. Hidden entries are skipped, which
/// keeps a `.Trash` or a sync tool's state directory out of the library.
fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect(&path, files);
        } else if is_playable(&path) {
            files.push(path);
        }
    }
}

fn is_playable(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| PLAYABLE.iter().any(|ok| extension.eq_ignore_ascii_case(ok)))
}

#[derive(Debug, Default)]
struct Tags {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    disc: u32,
    number: u32,
}

/// A file's tags, or `None` if it cannot be parsed at all. A file that parses
/// but carries no tags gives empty `Tags`, and is named from its path.
fn read_tags(path: &Path) -> Option<Tags> {
    let file = File::open(path).ok()?;
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    let tagged = Probe::new(BufReader::new(file))
        .options(options)
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return Some(Tags::default());
    };
    let text = |value: Option<std::borrow::Cow<'_, str>>| {
        value
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    Some(Tags {
        title: text(tag.title()),
        artist: text(tag.artist()),
        album: text(tag.album()),
        album_artist: text(tag.get_string(ItemKey::AlbumArtist).map(Into::into)),
        disc: tag.disk().unwrap_or(0),
        number: tag.track().unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str, disc: u32, number: u32) -> LocalTrack {
        LocalTrack {
            path: PathBuf::from(path),
            title: path.to_owned(),
            artist: String::new(),
            disc,
            number,
        }
    }

    #[test]
    fn only_decodable_files_are_listed() {
        assert!(is_playable(Path::new("a/01 Song.flac")));
        assert!(is_playable(Path::new("a/01 Song.FLAC")));
        assert!(is_playable(Path::new("a/01 Song.mp3")));
        assert!(!is_playable(Path::new("a/cover.jpg")));
        assert!(!is_playable(Path::new("a/notes.txt")));
        assert!(!is_playable(Path::new("a/flac")));
    }

    #[test]
    fn a_track_label_carries_its_number_when_it_has_one() {
        assert_eq!(track("Intro", 1, 3).label(), "03. Intro");
        assert_eq!(track("Intro", 0, 0).label(), "Intro");
    }

    fn album(artist: &str, title: &str) -> LocalAlbum {
        LocalAlbum {
            title: title.to_owned(),
            artist: artist.to_owned(),
            dir: PathBuf::new(),
            tracks: Vec::new(),
        }
    }

    #[test]
    fn each_artist_is_listed_once_in_album_order() {
        let albums = [
            album("Aphex Twin", "Drukqs"),
            album("aphex twin", "Syro"),
            album("Boards of Canada", "Geogaddi"),
        ];
        assert_eq!(artists(&albums), ["Aphex Twin", "Boards of Canada"]);
        assert!(artists(&[]).is_empty());
    }

    #[test]
    fn an_artist_narrows_the_albums_to_theirs() {
        let albums = [
            album("Aphex Twin", "Drukqs"),
            album("aphex twin", "Syro"),
            album("Boards of Canada", "Geogaddi"),
        ];
        assert_eq!(by_artist(&albums, None), [0, 1, 2]);
        assert_eq!(by_artist(&albums, Some("Aphex Twin")), [0, 1]);
        assert_eq!(by_artist(&albums, Some("Boards of Canada")), [2]);
        assert!(by_artist(&albums, Some("Autechre")).is_empty());
    }

    #[test]
    fn a_missing_directory_scans_to_nothing() {
        assert!(scan(Path::new("/nonexistent/osm-sound-demo-music")).is_empty());
    }

    /// Against the real collection, when there is one; nothing to check on a
    /// machine without `~/Music`.
    #[test]
    fn the_music_directory_scans_into_ordered_albums() {
        let Some(dir) = music_dir() else { return };
        for album in scan(&dir) {
            assert!(!album.tracks.is_empty(), "{}", album.label());
            assert!(
                album.tracks.windows(2).all(|pair| {
                    (pair[0].disc, pair[0].number, &pair[0].path)
                        <= (pair[1].disc, pair[1].number, &pair[1].path)
                }),
                "{} is out of order",
                album.label()
            );
        }
    }
}
