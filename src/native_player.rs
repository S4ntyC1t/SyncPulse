//! Experimental native Navidrome player (pilot).
//!
//! This is a proof of concept for a much lighter "client" mode: instead of
//! rendering Navidrome's web UI in an embedded browser engine (which costs a
//! full `WebKitWebProcess`, ~270 MB for the SPA's JavaScript heap), the player
//! draws plain GTK widgets and only talks data over Navidrome's Subsonic API.
//!
//! Audio is played by an `mpv` child process controlled over its JSON IPC
//! socket. That keeps the pilot free of any new Rust dependencies and reuses
//! the codecs already installed on the system; a production version could embed
//! GStreamer instead.
//!
//! Launch with `navidrome-companion --syncpulse-player`.

#![cfg(target_os = "linux")]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk_pixbuf, gio, glib};
use serde_json::json;

use crate::config;

const CLIENT_NAME: &str = "SyncPulse-Player";
const API_VERSION: &str = "1.16.1";
const PLAYER_APP_ID: &str = "org.navidromehub.syncpulse.player";

/// Run the pilot player. Blocks on the GTK main loop until the window closes.
pub(crate) fn run() -> Result<(), String> {
    crate::desktop::set_linux_application_id(PLAYER_APP_ID);

    let config = config::load();
    let subsonic = Arc::new(Subsonic::new(
        config.navidrome_port,
        config.navidrome_username.clone().unwrap_or_default(),
        config.navidrome_password.clone().unwrap_or_default(),
    ));

    gtk::init().map_err(|error| format!("Unable to initialize GTK: {error}"))?;

    let mpv = Rc::new(
        Mpv::spawn()
            .map_err(|error| format!("Unable to start the audio backend (mpv): {error}"))?,
    );

    let application = gtk::Application::new(None, gio::ApplicationFlags::empty());
    {
        let subsonic = Arc::clone(&subsonic);
        let mpv = Rc::clone(&mpv);
        application.connect_activate(move |application| {
            if let Err(error) = build_ui(application, &subsonic, &mpv) {
                tracing::error!("Unable to build the player window: {error}");
                application.quit();
            }
        });
    }
    application.run_with_args(&["syncpulse-player"]);
    Ok(())
}

// ---------------------------------------------------------------------------
// Subsonic API client
// ---------------------------------------------------------------------------

struct Subsonic {
    base: String,
    user: String,
    password: String,
    http: reqwest::blocking::Client,
}

impl Subsonic {
    fn new(port: u16, user: String, password: String) -> Self {
        let http = reqwest::blocking::Client::builder()
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

    /// Authentication parameters. Navidrome expects the token to be the MD5 of
    /// the password followed by the salt (the standard Subsonic scheme).
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

    fn get(&self, endpoint: &str, extra: &[(&str, &str)]) -> Result<serde_json::Value, String> {
        let mut params = self.auth();
        for (key, value) in extra {
            params.push(((*key).to_string(), (*value).to_string()));
        }
        let url = format!("{}/{endpoint}", self.base);
        let response = self
            .http
            .get(&url)
            .query(&params)
            .send()
            .map_err(|error| format!("{endpoint}: {error}"))?;
        let body: serde_json::Value = response
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

    /// URL of the cover art image, with auth embedded so a plain HTTP client
    /// (mpv, the image loader) can fetch it.
    fn cover_url(&self, cover_art_id: &str) -> String {
        let params = self
            .auth()
            .iter()
            .map(|(key, value)| format!("{key}={}", urlencode(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!(
            "{}/getCoverArt?id={}&size=512&{}",
            self.base,
            urlencode(cover_art_id),
            params
        )
    }

    /// URL of the audio stream for a track.
    fn stream_url(&self, track_id: &str) -> String {
        let params = self
            .auth()
            .iter()
            .map(|(key, value)| format!("{key}={}", urlencode(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}/stream?id={}&{}", self.base, urlencode(track_id), params)
    }

    fn albums(&self) -> Result<Vec<Album>, String> {
        let response = self.get(
            "getAlbumList2",
            &[("type", "alphabeticalByArtist"), ("size", "500")],
        )?;
        let list = response
            .get("albumList2")
            .and_then(|value| value.get("album"))
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(list.iter().map(Album::from_json).collect())
    }

    fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>, String> {
        let response = self.get("getAlbum", &[("id", album_id)])?;
        let songs = response
            .get("album")
            .and_then(|value| value.get("song"))
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(songs.iter().map(Track::from_json).collect())
    }

    fn cover_bytes(&self, cover_art_id: &str) -> Result<Vec<u8>, String> {
        let response = self
            .http
            .get(self.cover_url(cover_art_id))
            .send()
            .map_err(|error| format!("cover art: {error}"))?;
        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|error| format!("cover art: {error}"))
    }
}

#[derive(Clone, Debug)]
struct Album {
    id: String,
    name: String,
    artist: String,
    cover: Option<String>,
}

impl Album {
    fn from_json(value: &serde_json::Value) -> Self {
        Self {
            id: string_field(value, "id"),
            name: string_field(value, "name"),
            artist: string_field(value, "artist"),
            cover: optional_string_field(value, "coverArt"),
        }
    }
}

#[derive(Clone, Debug)]
struct Track {
    id: String,
    title: String,
    artist: String,
    duration: u64,
    cover: Option<String>,
}

impl Track {
    fn from_json(value: &serde_json::Value) -> Self {
        Self {
            id: string_field(value, "id"),
            title: string_field(value, "title"),
            artist: string_field(value, "artist"),
            duration: value
                .get("duration")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            cover: optional_string_field(value, "coverArt"),
        }
    }
}

fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn optional_string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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

/// Minimal percent-encoding for query values (Subsonic ids and tokens are
/// alphanumeric, but salts and URLs must never break the query string).
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

// ---------------------------------------------------------------------------
// mpv playback backend (JSON IPC)
// ---------------------------------------------------------------------------

struct Mpv {
    socket: PathBuf,
    child: Child,
}

impl Mpv {
    fn spawn() -> Result<Self, String> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let socket = runtime_dir.join(format!("syncpulse-player-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);

        let child = Command::new("mpv")
            .arg("--idle=yes")
            .arg("--no-video")
            .arg("--audio-display=no")
            // Navidrome is on loopback, so the demuxer cache only costs memory.
            .arg("--cache=no")
            .arg("--audio-buffer=0.2")
            .arg("--really-quiet")
            .arg("--no-terminal")
            .arg(format!("--input-ipc-server={}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{error}"))?;

        // mpv creates the socket shortly after start.
        for _ in 0..50 {
            if socket.exists() {
                return Ok(Self { socket, child });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("mpv did not create its IPC socket".to_string())
    }

    /// Send one command and return its result. A fresh connection is opened per
    /// call, and lines are matched by `request_id` so mpv's asynchronous event
    /// lines cannot be mistaken for a reply.
    fn command(&self, args: serde_json::Value) -> Result<Option<serde_json::Value>, String> {
        let stream = UnixStream::connect(&self.socket).map_err(|error| format!("{error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(1200)))
            .ok();
        let request_id: i64 = 1;
        let payload = json!({ "command": args, "request_id": request_id });
        {
            let mut writer = stream.try_clone().map_err(|error| format!("{error}"))?;
            writer
                .write_all(payload.to_string().as_bytes())
                .and_then(|_| writer.write_all(b"\n"))
                .and_then(|_| writer.flush())
                .map_err(|error| format!("{error}"))?;
        }
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => return Ok(None),
                Ok(_) => {}
                Err(_) => return Ok(None), // timeout: treat as "no answer"
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if value.get("request_id").and_then(|id| id.as_i64()) == Some(request_id) {
                return Ok(Some(value));
            }
            // Otherwise it is an event line; keep reading.
        }
    }

    fn load(&self, url: &str) -> Result<(), String> {
        self.command(json!(["loadfile", url, "replace"]))?;
        Ok(())
    }

    fn set_pause(&self, paused: bool) {
        let _ = self.command(json!(["set_property", "pause", paused]));
    }

    fn is_paused(&self) -> Option<bool> {
        self.number_or_bool("pause")
            .and_then(|value| value.as_bool())
    }

    fn seek(&self, seconds: f64) {
        let _ = self.command(json!(["seek", seconds, "absolute"]));
    }

    fn set_volume(&self, volume: f64) {
        let _ = self.command(json!(["set_property", "volume", volume]));
    }

    fn time_pos(&self) -> Option<f64> {
        self.number_or_bool("time-pos")
            .and_then(|value| value.as_f64())
    }

    fn duration(&self) -> Option<f64> {
        self.number_or_bool("duration")
            .and_then(|value| value.as_f64())
    }

    fn eof_reached(&self) -> bool {
        self.number_or_bool("eof-reached")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    fn idle(&self) -> bool {
        self.number_or_bool("idle-active")
            .and_then(|value| value.as_bool())
            .unwrap_or(true)
    }

    fn number_or_bool(&self, property: &str) -> Option<serde_json::Value> {
        let reply = self
            .command(json!(["get_property", property]))
            .ok()
            .flatten()?;
        if reply.get("error").and_then(|value| value.as_str()) != Some("success") {
            return None;
        }
        reply.get("data").cloned()
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.command(json!(["quit"]));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

/// Messages sent from worker threads to the GTK main loop.
enum Msg {
    Albums(Vec<Album>),
    Tracks(Vec<Track>),
    Cover(String, Vec<u8>),
    Error(String),
}

struct Ui {
    /// Every album returned by the server.
    all_albums: RefCell<Vec<Album>>,
    /// Albums currently shown (after filtering).
    albums: RefCell<Vec<Album>>,
    tracks: RefCell<Vec<Track>>,
    current: Cell<Option<usize>>,
    covers: RefCell<HashMap<String, gdk_pixbuf::Pixbuf>>,
    updating_progress: Cell<bool>,
    /// Test hook: load the first album and play its first track automatically.
    autoplay: bool,
    autoplay_done: Cell<bool>,
}

#[derive(Clone)]
struct Ctx {
    subsonic: Arc<Subsonic>,
    mpv: Rc<Mpv>,
    state: Rc<Ui>,
    tx: Sender<Msg>,
    cover: gtk::Image,
    title: gtk::Label,
    artist: gtk::Label,
    progress: gtk::Scale,
    time_label: gtk::Label,
    play_button: gtk::Button,
    status: gtk::Label,
    album_list: gtk::ListBox,
    track_list: gtk::ListBox,
}

fn build_ui(
    application: &gtk::Application,
    subsonic: &Arc<Subsonic>,
    mpv: &Rc<Mpv>,
) -> Result<(), String> {
    let window = gtk::ApplicationWindow::new(application);
    window.set_title("SyncPulse Player (pilot)");
    window.set_default_size(960, 620);
    if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(
        &include_bytes!("../icons/navidrome.png")[..],
    )) {
        window.set_icon(Some(&pixbuf));
    }

    let header = gtk::HeaderBar::new();
    header.set_title(Some("Native player"));
    header.set_subtitle(Some("Navidrome via Subsonic API"));
    window.set_titlebar(Some(&header));

    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    // Now playing.
    let now_playing = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let cover = gtk::Image::from_pixbuf(None);
    cover.set_pixel_size(96);
    now_playing.add(&cover);
    let meta = gtk::Box::new(gtk::Orientation::Vertical, 2);
    meta.set_valign(gtk::Align::Center);
    let title = gtk::Label::new(Some("Ничего не играет"));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let artist = gtk::Label::new(Some(""));
    artist.set_xalign(0.0);
    artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
    artist.style_context().add_class("dim-label");
    meta.add(&title);
    meta.add(&artist);
    now_playing.add(&meta);
    root.add(&now_playing);

    // Filter + album/track panes.
    let filter = gtk::SearchEntry::new();
    filter.set_placeholder_text(Some("Фильтр альбомов"));
    root.add(&filter);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_position(320);
    let album_list = gtk::ListBox::new();
    album_list.set_selection_mode(gtk::SelectionMode::Single);
    let album_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&album_list)
        .build();
    let track_list = gtk::ListBox::new();
    track_list.set_selection_mode(gtk::SelectionMode::Single);
    let track_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&track_list)
        .build();
    paned.pack1(&album_scroll, true, false);
    paned.pack2(&track_scroll, true, false);
    paned.set_vexpand(true);
    root.add(&paned);

    // Transport.
    let transport = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let prev =
        gtk::Button::from_icon_name(Some("media-skip-backward-symbolic"), gtk::IconSize::Button);
    let play_button =
        gtk::Button::from_icon_name(Some("media-playback-start-symbolic"), gtk::IconSize::Button);
    let next =
        gtk::Button::from_icon_name(Some("media-skip-forward-symbolic"), gtk::IconSize::Button);
    let progress = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
    progress.set_draw_value(false);
    progress.set_hexpand(true);
    let time_label = gtk::Label::new(Some("0:00 / 0:00"));
    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    volume.set_draw_value(false);
    volume.set_value(100.0);
    volume.set_size_request(120, -1);
    transport.add(&prev);
    transport.add(&play_button);
    transport.add(&next);
    transport.add(&progress);
    transport.add(&time_label);
    transport.add(&volume);
    root.add(&transport);

    let status = gtk::Label::new(Some("Загрузка библиотеки…"));
    status.set_xalign(0.0);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    status.style_context().add_class("dim-label");
    root.add(&status);

    window.add(&root);

    let (tx, rx) = mpsc::channel::<Msg>();
    let state = Rc::new(Ui {
        all_albums: RefCell::new(Vec::new()),
        albums: RefCell::new(Vec::new()),
        tracks: RefCell::new(Vec::new()),
        current: Cell::new(None),
        covers: RefCell::new(HashMap::new()),
        updating_progress: Cell::new(false),
        autoplay: std::env::var_os("SYNCPULSE_PLAYER_AUTOPLAY").is_some(),
        autoplay_done: Cell::new(false),
    });

    let ctx = Ctx {
        subsonic: Arc::clone(subsonic),
        mpv: Rc::clone(mpv),
        state: Rc::clone(&state),
        tx: tx.clone(),
        cover,
        title,
        artist,
        progress,
        time_label,
        play_button,
        status,
        album_list,
        track_list,
    };

    // Fetch the album list.
    spawn_albums(ctx.subsonic.clone(), tx.clone());

    // Opening an album loads its tracks.
    {
        let ctx = ctx.clone();
        ctx.album_list.clone().connect_row_activated(move |_, row| {
            let index = row.index();
            if index < 0 {
                return;
            }
            let album = ctx.state.albums.borrow().get(index as usize).cloned();
            if let Some(album) = album {
                ctx.status.set_text(&format!("Загрузка «{}»…", album.name));
                if let Some(cover) = album.cover.clone() {
                    let cached = ctx.state.covers.borrow().get(&cover).cloned();
                    match cached {
                        Some(pixbuf) => ctx.cover.set_pixbuf(Some(&pixbuf)),
                        None => spawn_cover(ctx.subsonic.clone(), ctx.tx.clone(), cover),
                    }
                }
                spawn_tracks(ctx.subsonic.clone(), ctx.tx.clone(), album.id);
            }
        });
    }

    // Double-click (or Enter) on a track plays it.
    {
        let ctx = ctx.clone();
        ctx.track_list.clone().connect_row_activated(move |_, row| {
            let index = row.index();
            if index >= 0 {
                play_index(&ctx, index as usize);
            }
        });
    }

    // Transport buttons.
    {
        let ctx = ctx.clone();
        ctx.play_button.clone().connect_clicked(move |_| {
            let current = ctx.state.current.get();
            match current {
                None => {
                    if !ctx.state.tracks.borrow().is_empty() {
                        play_index(&ctx, 0);
                    }
                }
                Some(_) => {
                    let paused = ctx.mpv.is_paused().unwrap_or(false);
                    ctx.mpv.set_pause(!paused);
                }
            }
        });
    }
    {
        let ctx = ctx.clone();
        next.connect_clicked(move |_| play_relative(&ctx, 1));
    }
    {
        let ctx = ctx.clone();
        prev.connect_clicked(move |_| play_relative(&ctx, -1));
    }

    // Seeking.
    {
        let ctx = ctx.clone();
        ctx.progress.clone().connect_value_changed(move |scale| {
            if ctx.state.updating_progress.get() {
                return;
            }
            ctx.mpv.seek(scale.value());
        });
    }

    // Volume.
    {
        let ctx = ctx.clone();
        volume.connect_value_changed(move |scale| ctx.mpv.set_volume(scale.value()));
    }

    // Filter box.
    {
        let ctx = ctx.clone();
        filter.connect_search_changed(move |entry| {
            let query = entry.text().to_lowercase();
            let all = ctx.state.all_albums.borrow().clone();
            let filtered: Vec<Album> = if query.is_empty() {
                all
            } else {
                all.into_iter()
                    .filter(|album| {
                        album.name.to_lowercase().contains(&query)
                            || album.artist.to_lowercase().contains(&query)
                    })
                    .collect()
            };
            *ctx.state.albums.borrow_mut() = filtered;
            populate_albums(&ctx.album_list, &ctx.state.albums.borrow());
        });
    }

    // Receive worker results on the GTK thread.
    {
        let ctx = ctx.clone();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            while let Ok(msg) = rx.try_recv() {
                handle_msg(&ctx, msg);
            }
            glib::ControlFlow::Continue
        });
    }

    // Progress / now-playing refresh.
    {
        let ctx = ctx.clone();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            refresh_progress(&ctx);
            glib::ControlFlow::Continue
        });
    }

    window.show_all();
    Ok(())
}

fn handle_msg(ctx: &Ctx, msg: Msg) {
    match msg {
        Msg::Albums(albums) => {
            tracing::info!("player: loaded {} albums from Navidrome", albums.len());
            ctx.status.set_text(&format!("Альбомов: {}", albums.len()));
            *ctx.state.all_albums.borrow_mut() = albums.clone();
            *ctx.state.albums.borrow_mut() = albums.clone();
            populate_albums(&ctx.album_list, &albums);
            if ctx.state.autoplay && !ctx.state.autoplay_done.get() {
                if let Some(album) = albums.first() {
                    spawn_tracks(ctx.subsonic.clone(), ctx.tx.clone(), album.id.clone());
                }
            }
        }
        Msg::Tracks(tracks) => {
            tracing::info!("player: loaded {} tracks", tracks.len());
            ctx.status.set_text(&format!("Треков: {}", tracks.len()));
            let has_tracks = !tracks.is_empty();
            *ctx.state.tracks.borrow_mut() = tracks;
            ctx.state.current.set(None);
            populate_tracks(&ctx.track_list, &ctx.state.tracks.borrow());
            if ctx.state.autoplay && !ctx.state.autoplay_done.get() {
                ctx.state.autoplay_done.set(true);
                if has_tracks {
                    play_index(ctx, 0);
                }
            }
        }
        Msg::Cover(id, bytes) => {
            if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(bytes)) {
                ctx.cover.set_pixbuf(Some(&pixbuf));
                ctx.state.covers.borrow_mut().insert(id, pixbuf);
            }
        }
        Msg::Error(error) => {
            tracing::warn!("player: {error}");
            ctx.status.set_text(&format!("Ошибка: {error}"));
        }
    }
}

fn play_index(ctx: &Ctx, index: usize) {
    let track = match ctx.state.tracks.borrow().get(index).cloned() {
        Some(track) => track,
        None => return,
    };
    ctx.state.current.set(Some(index));
    tracing::info!("player: playing {} — {}", track.artist, track.title);
    ctx.title.set_text(&track.title);
    ctx.artist.set_text(&track.artist);
    ctx.status
        .set_text(&format!("Играет: {} — {}", track.artist, track.title));
    ctx.cover.set_pixbuf(None);
    ctx.mpv.set_pause(false);
    if let Err(error) = ctx.mpv.load(&ctx.subsonic.stream_url(&track.id)) {
        ctx.status
            .set_text(&format!("Ошибка воспроизведения: {error}"));
    }
    if let Some(cover) = track.cover.clone() {
        if let Some(pixbuf) = ctx.state.covers.borrow().get(&cover) {
            ctx.cover.set_pixbuf(Some(pixbuf));
        } else {
            spawn_cover(ctx.subsonic.clone(), ctx.tx.clone(), cover);
        }
    }
    // Reflect the selected row in the list.
    if let Some(row) = ctx.track_list.row_at_index(index as i32) {
        ctx.track_list.select_row(Some(&row));
    }
}

fn play_relative(ctx: &Ctx, delta: i32) {
    let count = ctx.state.tracks.borrow().len();
    if count == 0 {
        return;
    }
    let current = ctx.state.current.get().unwrap_or(0) as i32;
    let next = (current + delta).rem_euclid(count as i32) as usize;
    play_index(ctx, next);
}

fn refresh_progress(ctx: &Ctx) {
    // The player has moved on to the next track automatically.
    if ctx.state.current.get().is_some() && ctx.mpv.eof_reached() && ctx.mpv.idle() {
        play_relative(ctx, 1);
        return;
    }

    let paused = ctx.mpv.is_paused().unwrap_or(false);
    if paused {
        ctx.play_button.set_image(Some(&gtk::Image::from_icon_name(
            Some("media-playback-start-symbolic"),
            gtk::IconSize::Button,
        )));
    } else {
        ctx.play_button.set_image(Some(&gtk::Image::from_icon_name(
            Some("media-playback-pause-symbolic"),
            gtk::IconSize::Button,
        )));
    }

    let position = ctx.mpv.time_pos().unwrap_or(0.0);
    let duration = ctx.mpv.duration().unwrap_or(0.0);
    if duration > 0.0 {
        ctx.state.updating_progress.set(true);
        ctx.progress.set_range(0.0, duration);
        if (ctx.progress.value() - position).abs() > 0.5 {
            ctx.progress.set_value(position);
        }
        ctx.state.updating_progress.set(false);
    }
    ctx.time_label.set_text(&format!(
        "{} / {}",
        format_time(position),
        format_time(duration)
    ));
}

fn populate_albums(listbox: &gtk::ListBox, albums: &[Album]) {
    for child in listbox.children() {
        listbox.remove(&child);
    }
    for album in albums {
        let row = gtk::ListBoxRow::new();
        let label = gtk::Label::new(None);
        label.set_markup(&format!(
            "<b>{}</b>\n<span size='small'>{}</span>",
            escape_markup(&album.name),
            escape_markup(&album.artist)
        ));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_margin_top(4);
        label.set_margin_bottom(4);
        label.set_margin_start(6);
        label.set_margin_end(6);
        row.add(&label);
        listbox.add(&row);
    }
    listbox.show_all();
}

fn populate_tracks(listbox: &gtk::ListBox, tracks: &[Track]) {
    for child in listbox.children() {
        listbox.remove(&child);
    }
    for track in tracks {
        let row = gtk::ListBoxRow::new();
        let label = gtk::Label::new(None);
        label.set_markup(&format!(
            "{} <span size='small' color='#888888'>{}</span>",
            escape_markup(&track.title),
            escape_markup(&format_time(track.duration as f64))
        ));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_margin_top(3);
        label.set_margin_bottom(3);
        label.set_margin_start(6);
        label.set_margin_end(6);
        row.add(&label);
        listbox.add(&row);
    }
    listbox.show_all();
}

fn spawn_albums(subsonic: Arc<Subsonic>, tx: Sender<Msg>) {
    std::thread::spawn(move || match subsonic.albums() {
        Ok(albums) => {
            let _ = tx.send(Msg::Albums(albums));
        }
        Err(error) => {
            let _ = tx.send(Msg::Error(error));
        }
    });
}

fn spawn_tracks(subsonic: Arc<Subsonic>, tx: Sender<Msg>, album_id: String) {
    std::thread::spawn(move || match subsonic.album_tracks(&album_id) {
        Ok(tracks) => {
            let _ = tx.send(Msg::Tracks(tracks));
        }
        Err(error) => {
            let _ = tx.send(Msg::Error(error));
        }
    });
}

fn spawn_cover(subsonic: Arc<Subsonic>, tx: Sender<Msg>, cover_id: String) {
    std::thread::spawn(move || match subsonic.cover_bytes(&cover_id) {
        Ok(bytes) => {
            let _ = tx.send(Msg::Cover(cover_id, bytes));
        }
        Err(error) => {
            let _ = tx.send(Msg::Error(error));
        }
    });
}

fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

fn escape_markup(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
