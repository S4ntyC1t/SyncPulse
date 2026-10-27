//! Minimal Subsonic/OpenSubsonic client for Navidrome.
//!
//! Navidrome speaks the Subsonic REST API (`/rest/...`); this module wraps the
//! handful of endpoints the native client needs and deserializes them into
//! plain structs. Authentication is the standard Subsonic scheme: a per-request
//! salt plus `md5(password + salt)`.
//!
//! The client is deliberately synchronous (`reqwest::blocking`) because every
//! caller already runs on a worker thread, and it reuses one `Client` so
//! connections to the local server are kept alive.

use std::time::Duration;

use serde::Deserialize;

/// Client name reported to the server (`c=` parameter).
const CLIENT_NAME: &str = "SyncPulse";
/// Subsonic API version we implement.
const API_VERSION: &str = "1.16.1";

pub(crate) type ApiResult<T> = Result<T, String>;

/// A Subsonic API client bound to one Navidrome instance.
pub(crate) struct Subsonic {
    base: String,
    user: String,
    password: String,
    http: reqwest::blocking::Client,
}

impl Subsonic {
    pub(crate) fn new(port: u16, user: String, password: String) -> Self {
        let http = reqwest::blocking::Client::builder()
            // Metadata calls are tiny; covers are small too. A short timeout
            // keeps a wedged server from freezing a worker thread.
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self {
            base: format!("http://127.0.0.1:{port}/rest"),
            user,
            password,
            http,
        }
    }

    /// Authentication parameters for one request.
    fn auth(&self) -> Vec<(String, String)> {
        let salt = random_salt();
        let token = md5_hex(&format!("{}{}", self.password, salt));
        vec![
            ("u".into(), self.user.clone()),
            ("t".into(), token),
            ("s".into(), salt),
            ("v".into(), API_VERSION.into()),
            ("c".into(), CLIENT_NAME.into()),
            ("f".into(), "json".into()),
        ]
    }

    /// Percent-encoded `key=value&...` authentication suffix.
    fn auth_query(&self) -> String {
        self.auth()
            .iter()
            .map(|(key, value)| format!("{key}={}", urlencode(value)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Call one endpoint and return its `subsonic-response` object.
    fn get(&self, endpoint: &str, extra: &[(&str, &str)]) -> ApiResult<serde_json::Value> {
        let mut params = self.auth();
        for (key, value) in extra {
            params.push(((*key).to_string(), (*value).to_string()));
        }
        let body: serde_json::Value = self
            .http
            .get(format!("{}/{endpoint}", self.base))
            .query(&params)
            .send()
            .map_err(|error| format!("{endpoint}: {error}"))?
            .json()
            .map_err(|error| format!("{endpoint}: invalid response: {error}"))?;
        let response = body
            .get("subsonic-response")
            .ok_or_else(|| format!("{endpoint}: not a Subsonic response"))?;
        if response.get("status").and_then(|value| value.as_str()) != Some("ok") {
            let message = response
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|value| value.as_str())
                .unwrap_or("unknown error");
            return Err(format!("{endpoint}: {message}"));
        }
        Ok(response.clone())
    }

    /// URL of one cover-art image with credentials embedded, so a plain HTTP
    /// client (the image loader) can fetch it.
    pub(crate) fn cover_url(&self, cover_art_id: &str, size: u32) -> String {
        format!(
            "{}/getCoverArt?id={}&size={size}&{}",
            self.base,
            urlencode(cover_art_id),
            self.auth_query()
        )
    }

    /// URL of a track's audio stream, with credentials embedded (mpv fetches
    /// it directly).
    pub(crate) fn stream_url(&self, song_id: &str) -> String {
        format!(
            "{}/stream?id={}&{}",
            self.base,
            urlencode(song_id),
            self.auth_query()
        )
    }

    /// `ping`, used to confirm the server is reachable and the credentials work.
    pub(crate) fn ping(&self) -> ApiResult<()> {
        self.get("ping", &[]).map(|_| ())
    }

    pub(crate) fn cover_bytes(&self, cover_art_id: &str, size: u32) -> ApiResult<Vec<u8>> {
        let response = self
            .http
            .get(self.cover_url(cover_art_id, size))
            .send()
            .map_err(|error| format!("cover art: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("cover art: HTTP {}", response.status()));
        }
        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|error| format!("cover art: {error}"))
    }

    /// One page of albums. `list_type` is a Subsonic `getAlbumList2` type such
    /// as `newest`, `alphabeticalByArtist`, `alphabeticalByName`, `frequent`,
    /// `recent`, `random` or `starred`.
    pub(crate) fn albums(&self, list_type: &str, size: u32, offset: u32) -> ApiResult<Vec<Album>> {
        let size = size.to_string();
        let offset = offset.to_string();
        let response = self.get(
            "getAlbumList2",
            &[("type", list_type), ("size", &size), ("offset", &offset)],
        )?;
        Ok(deserialize_list(&response, &["albumList2", "album"]))
    }

    /// An album plus its tracks.
    pub(crate) fn album(&self, id: &str) -> ApiResult<(Album, Vec<Song>)> {
        let response = self.get("getAlbum", &[("id", id)])?;
        let node = response
            .get("album")
            .ok_or_else(|| "getAlbum: missing album".to_owned())?;
        let album = parse::<Album>(node);
        let songs = node
            .get("song")
            .and_then(|value| value.as_array())
            .map(|songs| songs.iter().map(parse::<Song>).collect())
            .unwrap_or_default();
        Ok((album, songs))
    }

    /// Every artist, flattened out of the `index` buckets.
    pub(crate) fn artists(&self) -> ApiResult<Vec<Artist>> {
        let response = self.get("getArtists", &[])?;
        let mut artists = Vec::new();
        let indexes = response
            .get("artists")
            .and_then(|value| value.get("index"))
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        for index in &indexes {
            if let Some(list) = index.get("artist").and_then(|value| value.as_array()) {
                artists.extend(list.iter().map(parse::<Artist>));
            }
        }
        Ok(artists)
    }

    /// An artist plus their albums.
    pub(crate) fn artist(&self, id: &str) -> ApiResult<(Artist, Vec<Album>)> {
        let response = self.get("getArtist", &[("id", id)])?;
        let node = response
            .get("artist")
            .ok_or_else(|| "getArtist: missing artist".to_owned())?;
        let artist = parse::<Artist>(node);
        let albums = node
            .get("album")
            .and_then(|value| value.as_array())
            .map(|albums| albums.iter().map(parse::<Album>).collect())
            .unwrap_or_default();
        Ok((artist, albums))
    }

    pub(crate) fn playlists(&self) -> ApiResult<Vec<Playlist>> {
        let response = self.get("getPlaylists", &[])?;
        Ok(deserialize_list(&response, &["playlists", "playlist"]))
    }

    /// A playlist plus its tracks.
    pub(crate) fn playlist(&self, id: &str) -> ApiResult<(Playlist, Vec<Song>)> {
        let response = self.get("getPlaylist", &[("id", id)])?;
        let node = response
            .get("playlist")
            .ok_or_else(|| "getPlaylist: missing playlist".to_owned())?;
        let playlist = parse::<Playlist>(node);
        let songs = node
            .get("entry")
            .and_then(|value| value.as_array())
            .map(|songs| songs.iter().map(parse::<Song>).collect())
            .unwrap_or_default();
        Ok((playlist, songs))
    }

    pub(crate) fn starred(&self) -> ApiResult<Starred> {
        let response = self.get("getStarred2", &[])?;
        let node = response.get("starred2").cloned().unwrap_or_default();
        Ok(Starred {
            artists: list_of(&node, "artist"),
            albums: list_of(&node, "album"),
            songs: list_of(&node, "song"),
        })
    }

    pub(crate) fn random_songs(&self, size: u32) -> ApiResult<Vec<Song>> {
        let size = size.to_string();
        let response = self.get("getRandomSongs", &[("size", &size)])?;
        Ok(deserialize_list(&response, &["randomSongs", "song"]))
    }

    /// Global search. Navidrome ignores single-character queries.
    pub(crate) fn search(&self, query: &str) -> ApiResult<SearchResults> {
        let response = self.get(
            "search3",
            &[
                ("query", query),
                ("artistCount", "30"),
                ("albumCount", "60"),
                ("songCount", "80"),
            ],
        )?;
        let node = response.get("searchResult3").cloned().unwrap_or_default();
        Ok(SearchResults {
            artists: list_of(&node, "artist"),
            albums: list_of(&node, "album"),
            songs: list_of(&node, "song"),
        })
    }

    /// Report a play (or a "now playing" ping) for scrobbling.
    pub(crate) fn scrobble(&self, song_id: &str, submission: bool) {
        let _ = self.get(
            "scrobble",
            &[
                ("id", song_id),
                ("submission", if submission { "true" } else { "false" }),
            ],
        );
    }

    /// Star or unstar an item (`id`/`albumId`/`artistId` all take the same id).
    pub(crate) fn set_star(&self, kind: StarKind, id: &str, on: bool) -> ApiResult<()> {
        let endpoint = match (kind, on) {
            (_, true) => "star",
            (_, false) => "unstar",
        };
        let key = match kind {
            StarKind::Song => "id",
            StarKind::Album => "albumId",
            StarKind::Artist => "artistId",
        };
        self.get(endpoint, &[(key, id)]).map(|_| ())
    }

    /// Trigger a library rescan.
    pub(crate) fn start_scan(&self) -> ApiResult<()> {
        self.get("startScan", &[]).map(|_| ())
    }
}

/// What kind of item a star action refers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StarKind {
    Song,
    Album,
    Artist,
}

/// Results of a global search.
#[derive(Clone, Debug, Default)]
pub(crate) struct SearchResults {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub songs: Vec<Song>,
}

impl SearchResults {
    pub(crate) fn is_empty(&self) -> bool {
        self.artists.is_empty() && self.albums.is_empty() && self.songs.is_empty()
    }
}

/// Starred items, by kind.
#[derive(Clone, Debug, Default)]
pub(crate) struct Starred {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub songs: Vec<Song>,
}

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Album {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub artist_id: String,
    pub cover_art: String,
    pub song_count: u32,
    pub duration: u64,
    pub year: Option<u32>,
    pub play_count: u64,
    pub starred: Option<String>,
}

impl Album {
    /// The artist, or the album name when the artist is unknown.
    pub(crate) fn subtitle(&self) -> String {
        if self.artist.is_empty() {
            self.name.clone()
        } else {
            self.artist.clone()
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Song {
    pub id: String,
    pub title: String,
    pub album: String,
    pub artist: String,
    pub album_id: String,
    pub artist_id: String,
    pub track: Option<u32>,
    pub duration: u64,
    pub cover_art: String,
    pub suffix: String,
    pub bit_rate: u32,
    pub starred: Option<String>,
    pub play_count: u64,
}

impl Song {
    /// `Artist — Album`, or the artist alone when there is no album.
    pub(crate) fn subtitle(&self) -> String {
        match (self.artist.is_empty(), self.album.is_empty()) {
            (false, false) => format!("{} — {}", self.artist, self.album),
            (false, true) => self.artist.clone(),
            (true, false) => self.album.clone(),
            (true, true) => String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Artist {
    pub id: String,
    pub name: String,
    pub cover_art: String,
    pub album_count: u32,
    pub artist_image_url: String,
    pub starred: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Playlist {
    pub id: String,
    pub name: String,
    pub comment: String,
    pub owner: String,
    pub song_count: u32,
    pub duration: u64,
    pub cover_art: String,
    pub public: bool,
}

impl Playlist {
    /// A one-line description for list rows.
    pub(crate) fn subtitle(&self) -> String {
        let count = match self.song_count {
            1 => "1 трек".to_owned(),
            n => format!("{n} треков"),
        };
        if self.comment.is_empty() {
            count
        } else {
            format!("{count} · {}", self.comment)
        }
    }
}

// ---------------------------------------------------------------------------
// Deserialization helpers
// ---------------------------------------------------------------------------

fn parse<T: for<'de> Deserialize<'de> + Default>(value: &serde_json::Value) -> T {
    T::deserialize(value).unwrap_or_default()
}

fn list_of<T: for<'de> Deserialize<'de> + Default>(node: &serde_json::Value, key: &str) -> Vec<T> {
    node.get(key)
        .and_then(|value| value.as_array())
        .map(|items| items.iter().map(parse::<T>).collect())
        .unwrap_or_default()
}

/// Deserialize `path` (a chain of object keys ending in an array).
///
/// Subsonic nests lists behind varying wrappers (`albumList2.album`,
/// `randomSongs.song`, ...), so callers pass the key chain.
fn deserialize_list<T: for<'de> Deserialize<'de> + Default>(
    response: &serde_json::Value,
    path: &[&str],
) -> Vec<T> {
    let mut node = response;
    for key in path {
        match node.get(*key) {
            Some(next) => node = next,
            None => return Vec::new(),
        }
    }
    node.as_array()
        .map(|items| items.iter().map(parse::<T>).collect())
        .unwrap_or_default()
}

fn md5_hex(input: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn random_salt() -> String {
    let mut bytes = [0u8; 12];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Percent-encode a query value. Subsonic ids and tokens are alphanumeric, but
/// salts and cover-art ids must never break the query string.
fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if safe {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Format a duration in seconds as `m:ss` (or `h:mm:ss` past an hour).
pub(crate) fn format_duration(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Format a playtime in seconds as a long human string ("3 ч 12 мин").
pub(crate) fn format_playtime(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("{hours} ч {minutes} мин")
    } else if minutes > 0 {
        format!("{minutes} мин")
    } else {
        format!("{seconds} с")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formats_with_and_without_hours() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(270), "4:30");
        assert_eq!(format_duration(3725), "1:02:05");
    }

    #[test]
    fn playtime_is_human_readable() {
        assert_eq!(format_playtime(45), "45 с");
        assert_eq!(format_playtime(600), "10 мин");
        assert_eq!(format_playtime(11_520), "3 ч 12 мин");
    }

    #[test]
    fn urlencode_escapes_but_keeps_ids_intact() {
        assert_eq!(urlencode("al-abc_12"), "al-abc_12");
        assert_eq!(urlencode("a b+c"), "a%20b%2Bc");
    }

    #[test]
    fn nested_lists_are_flattened() {
        let response = serde_json::json!({
            "albumList2": {
                "album": [
                    {"id": "1", "name": "One", "artist": "A"},
                    {"id": "2", "name": "Two", "artist": "B"}
                ]
            }
        });
        let albums: Vec<Album> = deserialize_list(&response, &["albumList2", "album"]);
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[1].name, "Two");
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let album: Album = parse(&serde_json::json!({"id": "x", "name": "N"}));
        assert_eq!(album.id, "x");
        assert_eq!(album.song_count, 0);
        assert!(album.starred.is_none());
    }
}
