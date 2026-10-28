//! The client window: navigation, cover loading, worker pool, playback control.
//!
//! The GTK main loop never blocks: every Subsonic call (and every cover fetch)
//! runs on a small worker pool and comes back as a [`Msg`] that the main loop
//! drains. Cover art is loaded *lazily* — a widget only asks for its bitmap once
//! it scrolls into view — and the decoded bitmaps live in a byte-budgeted cache,
//! because a library with hundreds of albums would otherwise pin every cover in
//! memory at once.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk_pixbuf, gio, glib};

use super::api::{
    format_duration, Album, Artist, Playlist, SearchResults, Song, StarKind, Starred, Subsonic,
};
use super::mpris::{self, LoopStatus, Status};
use super::player::{self, Mpv, PlaybackState};
use super::views;
use crate::config;

/// Cover art size for the album grid (matches the ratio used by the web UI).
pub(super) const COVER_GRID: u32 = 240;
/// Cover art size for list-row thumbnails.
pub(super) const COVER_ROW: u32 = 64;
/// Cover art size for the now-playing image.
pub(super) const COVER_BAR: u32 = 96;
/// How much decoded cover art to keep around before evicting the oldest.
const COVER_CACHE_BYTES: usize = 48 * 1024 * 1024;
/// Extra pixels above/below the viewport that are preloaded.
const LAZY_MARGIN: f64 = 400.0;
const WORKER_THREADS: usize = 4;

// ---------------------------------------------------------------------------
// Requests and messages
// ---------------------------------------------------------------------------

/// One unit of work for the pool.
pub(super) struct Request {
    /// Navigation generation; responses carrying a stale one are dropped.
    pub generation: u64,
    pub op: Op,
}

/// Work the pool can do.
pub(super) enum Op {
    Albums {
        kind: String,
        size: u32,
        offset: u32,
    },
    Album {
        id: String,
    },
    Artists,
    Artist {
        id: String,
    },
    Playlists,
    Playlist {
        id: String,
    },
    Starred,
    Random {
        size: u32,
    },
    Search {
        query: String,
    },
    Cover {
        key: String,
        id: String,
        size: u32,
    },
    /// Fetch the cover of the current track and store it as a file, so the
    /// desktop can show it as the album art.
    NowPlayingCover {
        id: String,
    },
    Star {
        kind: StarKind,
        id: String,
        on: bool,
    },
    Scrobble {
        id: String,
        submission: bool,
    },
    Rescan,
}

/// Results coming back to the main loop.
pub(super) struct Msg {
    pub generation: u64,
    pub body: MsgBody,
}

pub(super) enum MsgBody {
    Albums {
        kind: String,
        items: Vec<Album>,
    },
    Album {
        album: Album,
        songs: Vec<Song>,
    },
    Artists(Vec<Artist>),
    Artist {
        artist: Artist,
        albums: Vec<Album>,
    },
    Playlists(Vec<Playlist>),
    Playlist {
        playlist: Playlist,
        songs: Vec<Song>,
    },
    Starred(Starred),
    Random(Vec<Song>),
    Search(SearchResults),
    Cover {
        key: String,
        bytes: Vec<u8>,
    },
    /// The cover of the current track, stored as a file the desktop may show.
    CoverFile {
        path: String,
    },
    Player(PlaybackState),
    Error(String),
    Status(String),
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

/// How the queue advances when a track ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Repeat {
    Off,
    All,
    One,
}

/// The playback queue.
///
/// `songs` holds every track the user queued, `order` is the order they are
/// played in (a permutation, shuffled or not) and `cursor` is the position
/// *within `order`*. Appending a song never invalidates existing indices, which
/// keeps queue edits simple.
#[derive(Default)]
pub(super) struct Queue {
    pub songs: Vec<Song>,
    pub order: Vec<usize>,
    pub cursor: usize,
}

impl Queue {
    pub(super) fn clear(&mut self) {
        self.songs.clear();
        self.order.clear();
        self.cursor = 0;
    }

    pub(super) fn len(&self) -> usize {
        self.order.len()
    }

    /// Append songs to the end of the queue.
    pub(super) fn push(&mut self, songs: &[Song]) {
        for song in songs {
            self.songs.push(song.clone());
            self.order.push(self.songs.len() - 1);
        }
    }

    /// Insert songs right after the current track.
    pub(super) fn insert_next(&mut self, songs: &[Song]) {
        let at = if self.order.is_empty() {
            0
        } else {
            self.cursor + 1
        };
        for (offset, song) in songs.iter().enumerate() {
            self.songs.push(song.clone());
            self.order.insert(at + offset, self.songs.len() - 1);
        }
    }

    /// Rebuild a shuffled order, keeping the current track in front.
    pub(super) fn reshuffle(&mut self) {
        let mut rest: Vec<usize> = self
            .order
            .iter()
            .copied()
            .enumerate()
            .filter(|(position, _)| *position != self.cursor)
            .map(|(_, index)| index)
            .collect();
        shuffle(&mut rest);
        let current = self.order.get(self.cursor).copied();
        self.order = match current {
            Some(current) => {
                let mut order = vec![current];
                order.extend(rest);
                order
            }
            None => rest,
        };
        self.cursor = 0;
    }

    /// Remove one entry by its position in the playback order.
    pub(super) fn remove_order_position(&mut self, position: usize) {
        if position >= self.order.len() {
            return;
        }
        let index = self.order.remove(position);
        self.songs.remove(index);
        // Every stored index above the removed one shifts down.
        for entry in &mut self.order {
            if *entry > index {
                *entry -= 1;
            }
        }
        self.cursor = self.cursor.min(self.order.len().saturating_sub(1));
    }

    pub(super) fn current(&self) -> Option<Song> {
        let index = *self.order.get(self.cursor)?;
        self.songs.get(index).cloned()
    }

    /// The songs as they will be played, for display.
    pub(super) fn ordered(&self) -> Vec<Song> {
        self.order
            .iter()
            .filter_map(|index| self.songs.get(*index).cloned())
            .collect()
    }
}

fn shuffle(items: &mut [usize]) {
    // Fisher-Yates with a cheap xorshift seeded from the clock: playback order
    // does not need cryptographic randomness.
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.subsec_nanos() as u64 | 1)
        .unwrap_or(0x9E3779B97F4A7C15);
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

/// Everything the views and the player bar read.
pub(super) struct UiState {
    pub queue: RefCell<Queue>,
    pub playing: RefCell<Option<Song>>,
    pub shuffle: Cell<bool>,
    pub repeat: Cell<Repeat>,
    /// Set once the current track has been scrobbled, so it happens only once.
    pub submitted: Cell<bool>,
    /// When the current track was started, used to ignore mpv's pre-load idle.
    pub started_at: Cell<Option<Instant>>,
    /// True while the desktop asked the player to stop: MPRIS knows that as
    /// a state of its own, apart from "paused".
    pub stopped: Cell<bool>,
    /// How the desktop sees the current track.
    pub track: RefCell<mpris::Track>,
}

impl UiState {
    pub(super) fn current(&self) -> Option<Song> {
        self.playing.borrow().clone()
    }
}

// ---------------------------------------------------------------------------
// Context shared by the UI and the views
// ---------------------------------------------------------------------------

pub(super) struct Ctx {
    pub api: Arc<Subsonic>,
    pub tx: Sender<Request>,
    pub covers: Covers,
    pub player: Arc<Mpv>,
    pub state: Rc<UiState>,
    /// The desktop integration: the state it reads and the commands it sends.
    pub mpris: mpris::Mpris,
    /// Where pages are swapped in.
    pub content: gtk::Box,
    pub sidebar: gtk::ListBox,
    pub status: gtk::Label,
    pub window: gtk::ApplicationWindow,
    // Now-playing bar.
    pub bar_cover: gtk::Image,
    pub bar_title: gtk::Label,
    pub bar_artist: gtk::Label,
    pub play_button: gtk::Button,
    pub progress: gtk::Scale,
    pub time_label: gtk::Label,
    pub shuffle_button: gtk::ToggleButton,
    pub repeat_button: gtk::ToggleButton,
    pub volume: gtk::Scale,
    /// Guards the progress scale against feedback while it is programmatically
    /// updated.
    pub updating_progress: Cell<bool>,
    /// Guards the repeat button: GTK3 emits "clicked" when the state of a
    /// toggle button is set programmatically, so the button would otherwise
    /// read the update as a click of the user.
    pub repeat_syncing: Cell<bool>,
    pub generation: Cell<u64>,
    pub nav_programmatic: Cell<bool>,
    /// True while the queue page is the visible one.
    pub queue_visible: Cell<bool>,
}

impl Ctx {
    /// Send a *page* request, stepping the navigation generation first.
    ///
    /// Because every page load gets a fresh generation, a slow reply for a page
    /// the user has already navigated away from is dropped instead of replacing
    /// the current one.
    pub(super) fn send_page(&self, op: Op) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let _ = self.tx.send(Request { generation, op });
    }

    /// Send work that must never be dropped as stale (covers, playback,
    /// star actions).
    pub(super) fn send_untagged(&self, op: Op) {
        let _ = self.tx.send(Request { generation: 0, op });
    }

    pub(super) fn set_status(&self, text: &str) {
        self.status.set_text(text);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(super) fn run() -> Result<(), String> {
    let settings = config::load();
    let api = Arc::new(Subsonic::new(
        settings.navidrome_port,
        settings.navidrome_username.clone().unwrap_or_default(),
        settings.navidrome_password.clone().unwrap_or_default(),
    ));

    gtk::init().map_err(|error| format!("Unable to initialize GTK: {error}"))?;

    let player = Arc::new(Mpv::spawn()?);

    let application = gtk::Application::new(None, gio::ApplicationFlags::empty());
    {
        let player = Arc::clone(&player);
        application.connect_activate(move |application| {
            if let Err(error) = build_window(application, &api, &player) {
                tracing::error!("Unable to build the client window: {error}");
                application.quit();
            }
        });
    }
    application.run_with_args(&["syncpulse-client"]);
    Ok(())
}

fn build_window(
    application: &gtk::Application,
    api: &Arc<Subsonic>,
    player: &Arc<Mpv>,
) -> Result<(), String> {
    let window = gtk::ApplicationWindow::new(application);
    window.set_title("SyncPulse — Navidrome");
    window.set_default_size(1180, 760);
    window.set_size_request(900, 560);
    if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(
        &include_bytes!("../../icons/navidrome.png")[..],
    )) {
        window.set_icon(Some(&pixbuf));
    }

    let header = gtk::HeaderBar::new();
    header.set_title(Some("SyncPulse"));
    header.set_subtitle(Some("Navidrome"));
    header.set_show_close_button(true);
    window.set_titlebar(Some(&header));

    let (ui_tx, ui_rx) = mpsc::channel::<Msg>();
    let tx = spawn_workers(Arc::clone(api), ui_tx.clone());
    let covers = Covers::new(tx.clone());

    // The desktop media controls: the client shows up in the system tray and
    // can be driven from there.
    let mpris = mpris::Mpris::start();

    let state = Rc::new(UiState {
        queue: RefCell::new(Queue::default()),
        playing: RefCell::new(None),
        shuffle: Cell::new(false),
        repeat: Cell::new(Repeat::Off),
        submitted: Cell::new(false),
        started_at: Cell::new(None),
        stopped: Cell::new(false),
        track: RefCell::new(mpris::Track::default()),
    });

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);

    // ---- Sidebar ----------------------------------------------------------
    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    sidebar_box.set_margin_top(8);
    sidebar_box.set_margin_bottom(8);
    sidebar_box.set_margin_start(8);
    sidebar_box.set_margin_end(8);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Поиск по библиотеке"));
    sidebar_box.add(&search);

    let sidebar = gtk::ListBox::new();
    sidebar.set_selection_mode(gtk::SelectionMode::Single);
    for (icon, text) in [
        ("media-optical-symbolic", "Альбомы"),
        ("system-users-symbolic", "Исполнители"),
        ("media-playlist-normal-symbolic", "Плейлисты"),
        ("emblem-favorite-symbolic", "Избранное"),
        ("media-shuffle-symbolic", "Случайные треки"),
        ("view-list-symbolic", "Очередь"),
    ] {
        sidebar.add(&nav_row(icon, text));
    }
    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&sidebar)
        .build();
    sidebar_scroll.set_vexpand(true);
    sidebar_box.add(&sidebar_scroll);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_line_wrap(true);
    status.style_context().add_class("dim-label");
    sidebar_box.add(&status);

    paned.pack1(&sidebar_box, false, false);
    paned.set_position(238);

    // ---- Content ----------------------------------------------------------
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    paned.pack2(&content, true, false);
    paned.set_vexpand(true);
    root.add(&paned);

    // ---- Now-playing bar --------------------------------------------------
    let (bar, bar_widgets) = build_player_bar();
    root.add(&bar);

    window.add(&root);

    let ctx = Rc::new(Ctx {
        api: Arc::clone(api),
        tx,
        covers: covers.clone(),
        player: Arc::clone(player),
        state,
        mpris,
        content,
        sidebar,
        status,
        window: window.clone(),
        bar_cover: bar_widgets.cover,
        bar_title: bar_widgets.title,
        bar_artist: bar_widgets.artist,
        play_button: bar_widgets.play,
        progress: bar_widgets.progress,
        time_label: bar_widgets.time_label,
        shuffle_button: bar_widgets.shuffle,
        repeat_button: bar_widgets.repeat,
        volume: bar_widgets.volume,
        updating_progress: Cell::new(false),
        repeat_syncing: Cell::new(false),
        generation: Cell::new(0),
        nav_programmatic: Cell::new(false),
        queue_visible: Cell::new(false),
    });

    wire_sidebar(&ctx);
    wire_player_bar(&ctx, &bar_widgets.prev, &bar_widgets.next);
    wire_search(&ctx, &search);

    // Confirm the credentials/endpoint once, off the UI thread.
    {
        let api = Arc::clone(api);
        std::thread::spawn(move || {
            if let Err(error) = api.ping() {
                tracing::warn!("Navidrome is not answering: {error}");
            }
        });
    }

    // Rescan button in the title bar.
    {
        let rescan =
            gtk::Button::from_icon_name(Some("view-refresh-symbolic"), gtk::IconSize::Button);
        rescan.set_tooltip_text(Some("Пересканировать библиотеку"));
        rescan.set_relief(gtk::ReliefStyle::None);
        let ctx = Rc::clone(&ctx);
        rescan.connect_clicked(move |_| ctx.send_untagged(Op::Rescan));
        header.pack_end(&rescan);
        header.show_all();
    }

    // Worker results.
    {
        let ctx = Rc::clone(&ctx);
        glib::timeout_add_local(Duration::from_millis(40), move || {
            while let Ok(msg) = ui_rx.try_recv() {
                handle(&ctx, msg);
            }
            // The desktop media controls send their commands over the bus;
            // they are answered here, on the thread that owns the player.
            for command in ctx.mpris.drain() {
                handle_mpris(&ctx, command);
            }
            glib::ControlFlow::Continue
        });
    }

    // mpv state, polled off the main thread so a slow socket cannot stall the UI.
    {
        let ui_tx = ui_tx.clone();
        player::watch(player, Duration::from_millis(500), move |state| {
            ui_tx
                .send(Msg {
                    generation: 0,
                    body: MsgBody::Player(state),
                })
                .is_ok()
        });
    }

    {
        let player = Arc::clone(player);
        window.connect_destroy(move |_| player.quit());
    }

    window.show_all();
    ctx.set_status("Загрузка библиотеки…");
    open_albums(&ctx, "alphabeticalByArtist");

    Ok(())
}

/// Widgets of the now-playing bar, kept together while building.
struct BarWidgets {
    cover: gtk::Image,
    title: gtk::Label,
    artist: gtk::Label,
    prev: gtk::Button,
    play: gtk::Button,
    next: gtk::Button,
    progress: gtk::Scale,
    time_label: gtk::Label,
    shuffle: gtk::ToggleButton,
    repeat: gtk::ToggleButton,
    volume: gtk::Scale,
}

fn build_player_bar() -> (gtk::Box, BarWidgets) {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    bar.set_margin_top(6);
    bar.set_margin_bottom(6);
    bar.set_margin_start(10);
    bar.set_margin_end(10);

    let cover = gtk::Image::from_pixbuf(None);
    cover.set_pixel_size(COVER_BAR as i32);
    cover.set_size_request(COVER_BAR as i32, COVER_BAR as i32);
    let cover_box = gtk::EventBox::new();
    cover_box.add(&cover);
    bar.add(&cover_box);

    let meta = gtk::Box::new(gtk::Orientation::Vertical, 0);
    meta.set_valign(gtk::Align::Center);
    meta.set_size_request(210, -1);
    let title = gtk::Label::new(Some("Ничего не играет"));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let artist = gtk::Label::new(None);
    artist.set_xalign(0.0);
    artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
    artist.style_context().add_class("dim-label");
    meta.add(&title);
    meta.add(&artist);
    bar.add(&meta);

    let prev = icon_button("media-skip-backward-symbolic", "Предыдущий");
    let play = icon_button("media-playback-start-symbolic", "Играть / пауза");
    let next = icon_button("media-skip-forward-symbolic", "Следующий");
    bar.add(&prev);
    bar.add(&play);
    bar.add(&next);

    let progress = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
    progress.set_draw_value(false);
    progress.set_hexpand(true);
    bar.add(&progress);

    let time_label = gtk::Label::new(Some("0:00 / 0:00"));
    time_label.style_context().add_class("dim-label");
    time_label.set_size_request(110, -1);
    bar.add(&time_label);

    let shuffle = toggle_button("media-playlist-shuffle-symbolic", "Перемешать");
    let repeat = toggle_button("media-playlist-repeat-symbolic", "Повтор");
    bar.add(&shuffle);
    bar.add(&repeat);

    let volume_icon =
        gtk::Image::from_icon_name(Some("audio-volume-high-symbolic"), gtk::IconSize::Button);
    bar.add(&volume_icon);
    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    volume.set_draw_value(false);
    volume.set_value(100.0);
    volume.set_size_request(110, -1);
    bar.add(&volume);

    (
        bar,
        BarWidgets {
            cover,
            title,
            artist,
            prev,
            play,
            next,
            progress,
            time_label,
            shuffle,
            repeat,
            volume,
        },
    )
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(Some(icon), gtk::IconSize::Button);
    button.set_tooltip_text(Some(tooltip));
    button.set_relief(gtk::ReliefStyle::None);
    button
}

fn toggle_button(icon: &str, tooltip: &str) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.set_image(Some(&gtk::Image::from_icon_name(
        Some(icon),
        gtk::IconSize::Button,
    )));
    button.set_tooltip_text(Some(tooltip));
    button.set_relief(gtk::ReliefStyle::None);
    button
}

fn nav_row(icon: &str, text: &str) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.set_margin_top(7);
    content.set_margin_bottom(7);
    content.set_margin_start(10);
    content.set_margin_end(10);
    content.add(&gtk::Image::from_icon_name(Some(icon), gtk::IconSize::Menu));
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    content.add(&label);
    row.add(&content);
    row
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

fn wire_sidebar(ctx: &Rc<Ctx>) {
    let ctx = Rc::clone(ctx);
    ctx.sidebar.clone().connect_row_selected(move |_, row| {
        if ctx.nav_programmatic.get() {
            return;
        }
        let Some(row) = row else { return };
        match row.index() {
            0 => open_albums(&ctx, "alphabeticalByArtist"),
            1 => {
                ctx.set_status("Загрузка исполнителей…");
                ctx.send_page(Op::Artists);
            }
            2 => {
                ctx.set_status("Загрузка плейлистов…");
                ctx.send_page(Op::Playlists);
            }
            3 => {
                ctx.set_status("Загрузка избранного…");
                ctx.send_page(Op::Starred);
            }
            4 => {
                ctx.set_status("Случайные треки…");
                ctx.send_page(Op::Random { size: 50 });
            }
            _ => open_queue(&ctx),
        }
    });
}

fn wire_player_bar(ctx: &Rc<Ctx>, prev: &gtk::Button, next: &gtk::Button) {
    {
        let ctx = Rc::clone(ctx);
        ctx.play_button.clone().connect_clicked(move |_| {
            toggle_playback(&ctx);
        });
    }
    {
        let ctx = Rc::clone(ctx);
        ctx.progress.clone().connect_value_changed(move |scale| {
            if ctx.updating_progress.get() {
                return;
            }
            ctx.player.seek(scale.value());
        });
    }
    {
        let ctx = Rc::clone(ctx);
        next.connect_clicked(move |_| advance(&ctx, 1));
    }
    {
        let ctx = Rc::clone(ctx);
        prev.connect_clicked(move |_| advance(&ctx, -1));
    }
    {
        let ctx = Rc::clone(ctx);
        ctx.volume.clone().connect_value_changed(move |scale| {
            ctx.player.set_volume(scale.value());
            // The desktop shows the volume too.
            let volume = (scale.value() / 100.0).clamp(0.0, 1.0);
            ctx.mpris.publish(|snapshot| snapshot.volume = volume);
        });
    }
    {
        let ctx = Rc::clone(ctx);
        ctx.shuffle_button.clone().connect_toggled(move |button| {
            let on = button.is_active();
            ctx.state.shuffle.set(on);
            ctx.mpris.publish(|snapshot| snapshot.shuffle = on);
            if on {
                ctx.state.queue.borrow_mut().reshuffle();
                open_queue(&ctx);
            } else {
                // Restore natural order, keeping the current track first.
                let mut queue = ctx.state.queue.borrow_mut();
                let current = queue.current();
                queue.order = (0..queue.songs.len()).collect();
                queue.cursor = current
                    .and_then(|song| queue.songs.iter().position(|item| item.id == song.id))
                    .unwrap_or(0);
            }
        });
    }
    {
        let ctx = Rc::clone(ctx);
        ctx.repeat_button.clone().connect_clicked(move |_| {
            if ctx.repeat_syncing.get() {
                // The state was set from the outside, not clicked.
                return;
            }
            let next = match ctx.state.repeat.get() {
                Repeat::Off => Repeat::All,
                Repeat::All => Repeat::One,
                Repeat::One => Repeat::Off,
            };
            set_repeat(&ctx, next);
        });
    }
}

/// Change the repeat mode and tell the desktop about it.
fn set_repeat(ctx: &Rc<Ctx>, repeat: Repeat) {
    ctx.state.repeat.set(repeat);
    // Setting the state of a toggle button emits "clicked" in GTK3, which
    // would cycle the mode once more.
    ctx.repeat_syncing.set(true);
    apply_repeat_button(&ctx.repeat_button, repeat);
    ctx.repeat_syncing.set(false);

    let loop_status = match repeat {
        Repeat::Off => LoopStatus::None,
        Repeat::All => LoopStatus::Playlist,
        Repeat::One => LoopStatus::Track,
    };
    ctx.mpris
        .publish(|snapshot| snapshot.loop_status = loop_status);
}
fn apply_repeat_button(button: &gtk::ToggleButton, repeat: Repeat) {
    let (icon, tooltip) = match repeat {
        Repeat::Off => ("media-playlist-repeat-symbolic", "Повтор: выключен"),
        Repeat::All => ("media-playlist-repeat-symbolic", "Повтор: вся очередь"),
        Repeat::One => (
            "media-playlist-repeat-song-symbolic",
            "Повтор: текущий трек",
        ),
    };
    button.set_image(Some(&gtk::Image::from_icon_name(
        Some(icon),
        gtk::IconSize::Button,
    )));
    button.set_tooltip_text(Some(tooltip));
    button.set_active(repeat != Repeat::Off);
}

fn wire_search(ctx: &Rc<Ctx>, entry: &gtk::SearchEntry) {
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let ctx = Rc::clone(ctx);
    entry.connect_search_changed(move |entry| {
        let query = entry.text().trim().to_owned();
        if let Some(source) = pending.borrow_mut().take() {
            source.remove();
        }
        if query.len() < 2 {
            return;
        }
        let ctx = Rc::clone(&ctx);
        let clear = Rc::clone(&pending);
        // Debounce: search3 hits the server on every keystroke otherwise, and
        // Navidrome ignores one-character queries anyway.
        let source = glib::timeout_add_local(Duration::from_millis(250), move || {
            clear.borrow_mut().take();
            ctx.set_status("Поиск…");
            ctx.send_page(Op::Search {
                query: query.clone(),
            });
            glib::ControlFlow::Break
        });
        *pending.borrow_mut() = Some(source);
    });
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

pub(super) fn open_albums(ctx: &Rc<Ctx>, kind: &str) {
    ctx.set_status("Загрузка альбомов…");
    ctx.send_page(Op::Albums {
        kind: kind.to_owned(),
        size: 500,
        offset: 0,
    });
}

pub(super) fn open_album(ctx: &Rc<Ctx>, id: &str) {
    ctx.set_status("Загрузка альбома…");
    ctx.send_page(Op::Album { id: id.to_owned() });
}

pub(super) fn open_artist(ctx: &Rc<Ctx>, id: &str) {
    ctx.set_status("Загрузка исполнителя…");
    ctx.send_page(Op::Artist { id: id.to_owned() });
}

pub(super) fn open_playlist(ctx: &Rc<Ctx>, id: &str) {
    ctx.set_status("Загрузка плейлиста…");
    ctx.send_page(Op::Playlist { id: id.to_owned() });
}

pub(super) fn open_queue(ctx: &Rc<Ctx>) {
    let songs = ctx.state.queue.borrow().ordered();
    let playing = ctx.state.current();
    let page = views::song_page(
        ctx,
        "Очередь воспроизведения",
        &format!("{} треков", songs.len()),
        songs,
        playing.as_ref(),
        true,
    );
    show_page(ctx, &page);
    ctx.queue_visible.set(true);
    ctx.set_status("");
}

pub(super) fn show_page(ctx: &Ctx, widget: &impl IsA<gtk::Widget>) {
    ctx.queue_visible.set(false);
    for child in ctx.content.children() {
        ctx.content.remove(&child);
    }
    // `expand` matters: a plain `add` would leave the page at its minimum
    // size (a scroll window asks for almost nothing) instead of filling the
    // content area.
    ctx.content.pack_start(widget, true, true, 0);
    ctx.content.show_all();
    // GTK lays the page out on the next frame the compositor sends, and a
    // covered window does not get any. The page would then keep its initial
    // 1x1 allocation forever, so the layout is done right here instead.
    let area = ctx.content.allocation();
    widget.queue_allocate();
    widget.size_allocate(&gtk::Allocation::new(0, 0, area.width(), area.height()));
}

/// Select a sidebar section without re-triggering navigation.
pub(super) fn select_section(ctx: &Ctx, index: i32) {
    ctx.nav_programmatic.set(true);
    if let Some(row) = ctx.sidebar.row_at_index(index) {
        ctx.sidebar.select_row(Some(&row));
    }
    ctx.nav_programmatic.set(false);
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

fn handle(ctx: &Rc<Ctx>, msg: Msg) {
    // Playback telemetry and cover art are always current.
    let stale = msg.generation != 0 && msg.generation != ctx.generation.get();
    match msg.body {
        MsgBody::Player(state) => handle_player(ctx, state),
        MsgBody::Cover { key, bytes } => ctx.covers.deliver(&key, &bytes),
        MsgBody::CoverFile { path } => handle_cover_file(ctx, &path),
        MsgBody::Error(error) => {
            ctx.set_status(&format!("Ошибка: {error}"));
            tracing::warn!("client: {error}");
        }
        MsgBody::Status(text) => ctx.set_status(&text),
        _ if stale => {}
        body => handle_page(ctx, body),
    }
}

fn handle_page(ctx: &Rc<Ctx>, body: MsgBody) {
    match body {
        MsgBody::Albums { kind, items } => {
            tracing::info!("client: albums ({kind}): {} items", items.len());
            select_section(ctx, 0);
            let page = views::album_grid(ctx, items);
            show_page(ctx, &page);
            ctx.set_status("");
        }
        MsgBody::Album { album, songs } => {
            tracing::info!("client: album {}: {} tracks", album.name, songs.len());
            select_section(ctx, 0);
            let page = views::album_detail(ctx, album, songs);
            show_page(ctx, &page);
            ctx.set_status("");
        }
        MsgBody::Artists(artists) => {
            tracing::info!("client: artists: {}", artists.len());
            select_section(ctx, 1);
            let page = views::artist_list(ctx, artists);
            show_page(ctx, &page);
            ctx.set_status("");
        }
        MsgBody::Artist { artist, albums } => {
            tracing::info!("client: artist {}: {} albums", artist.name, albums.len());
            select_section(ctx, 1);
            let page = views::artist_detail(ctx, artist, albums);
            show_page(ctx, &page);
            ctx.set_status("");
        }
        MsgBody::Playlists(playlists) => {
            tracing::info!("client: playlists: {}", playlists.len());
            select_section(ctx, 2);
            let page = views::playlist_list(ctx, playlists);
            show_page(ctx, &page);
            ctx.set_status("");
        }
        MsgBody::Playlist { playlist, songs } => {
            tracing::info!("client: playlist {}: {} tracks", playlist.name, songs.len());
            select_section(ctx, 2);
            let page = views::playlist_detail(ctx, playlist, songs);
            show_page(ctx, &page);
            ctx.set_status("");
        }
        MsgBody::Starred(starred) => {
            tracing::info!(
                "client: starred: {} artists, {} albums, {} songs",
                starred.artists.len(),
                starred.albums.len(),
                starred.songs.len()
            );
            select_section(ctx, 3);
            let page = views::starred_page(ctx, starred);
            show_page(ctx, &page);
            ctx.set_status("");
        }
        MsgBody::Random(songs) => {
            tracing::info!("client: random: {} songs", songs.len());
            select_section(ctx, 4);
            play_now(ctx, songs.clone(), 0);
            let playing = ctx.state.current();
            let page = views::song_page(
                ctx,
                "Случайные треки",
                &format!("{} треков", songs.len()),
                songs,
                playing.as_ref(),
                false,
            );
            show_page(ctx, &page);
            ctx.set_status("");
        }
        MsgBody::Search(results) => {
            tracing::info!(
                "client: search: {} artists, {} albums, {} songs",
                results.artists.len(),
                results.albums.len(),
                results.songs.len()
            );
            let page = views::search_page(ctx, results);
            show_page(ctx, &page);
            ctx.set_status("");
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Playback control
// ---------------------------------------------------------------------------

/// Replace the queue with `songs` and start at `start`.
pub(super) fn play_now(ctx: &Rc<Ctx>, songs: Vec<Song>, start: usize) {
    {
        let mut queue = ctx.state.queue.borrow_mut();
        queue.clear();
        queue.push(&songs);
        let start = start.min(queue.order.len().saturating_sub(1));
        if ctx.state.shuffle.get() && queue.order.len() > 1 {
            // Keep the chosen track first, shuffle everything after it.
            let first = queue.order[start];
            let mut rest: Vec<usize> = queue
                .order
                .iter()
                .copied()
                .filter(|index| *index != first)
                .collect();
            shuffle(&mut rest);
            queue.order = std::iter::once(first).chain(rest).collect();
            queue.cursor = 0;
        } else {
            queue.cursor = start;
        }
    }
    play_cursor(ctx);
}

/// Add songs to the queue, either at the end or right after the current track.
pub(super) fn enqueue(ctx: &Rc<Ctx>, songs: Vec<Song>, next: bool) {
    let mut queue = ctx.state.queue.borrow_mut();
    if next {
        queue.insert_next(&songs);
    } else {
        queue.push(&songs);
    }
    let total = queue.len();
    drop(queue);
    ctx.set_status(&format!("В очереди {total} треков"));
}

/// Move `delta` tracks through the queue and play the result.
pub(super) fn advance(ctx: &Rc<Ctx>, delta: i32) {
    let repeat = ctx.state.repeat.get();
    let moved = {
        let mut queue = ctx.state.queue.borrow_mut();
        if queue.order.is_empty() {
            false
        } else if delta > 0 && repeat == Repeat::One {
            // Repeat-one replays the same track.
            true
        } else {
            let last = queue.order.len() as i32 - 1;
            let mut cursor = queue.cursor as i32 + delta;
            if cursor < 0 {
                cursor = if repeat == Repeat::All || delta > 0 {
                    last
                } else {
                    0
                };
            }
            if cursor > last {
                if repeat == Repeat::All {
                    queue.cursor = 0;
                    true
                } else {
                    // End of the queue: stop at the last track instead of
                    // silently looping.
                    queue.cursor = last as usize;
                    false
                }
            } else {
                queue.cursor = cursor as usize;
                true
            }
        }
    };
    if moved {
        play_cursor(ctx);
    } else {
        ctx.player.set_paused(true);
    }
}

/// Play a specific position in the playback order.
pub(super) fn play_position(ctx: &Rc<Ctx>, position: usize) {
    {
        let mut queue = ctx.state.queue.borrow_mut();
        if position >= queue.order.len() {
            return;
        }
        queue.cursor = position;
    }
    play_cursor(ctx);
}

pub(super) fn remove_position(ctx: &Rc<Ctx>, position: usize) {
    {
        let mut queue = ctx.state.queue.borrow_mut();
        queue.remove_order_position(position);
    }
    open_queue(ctx);
}

pub(super) fn toggle_playback(ctx: &Rc<Ctx>) {
    if ctx.state.current().is_none() {
        // Nothing loaded yet: start the queue, if there is one.
        let empty = ctx.state.queue.borrow().order.is_empty();
        if !empty {
            play_cursor(ctx);
        }
        return;
    }
    ctx.state.stopped.set(false);
    ctx.player.set_paused(!ctx.player.paused());
}

/// Start the track at the queue cursor.
fn play_cursor(ctx: &Rc<Ctx>) {
    let song = ctx.state.queue.borrow().current();
    let Some(song) = song else {
        return;
    };
    start_song(ctx, song);
}

fn start_song(ctx: &Rc<Ctx>, song: Song) {
    *ctx.state.playing.borrow_mut() = Some(song.clone());
    ctx.state.submitted.set(false);
    ctx.state.started_at.set(Some(Instant::now()));
    ctx.state.stopped.set(false);

    ctx.bar_title.set_text(&song.title);
    ctx.bar_artist.set_text(&song.subtitle());
    ctx.window.set_title(&format!("{} — SyncPulse", song.title));

    ctx.bar_cover.set_pixbuf(None);
    if !song.cover_art.is_empty() {
        ctx.covers
            .get_sized(&ctx.bar_cover, &song.cover_art, COVER_BAR);
        // The desktop shows the album art of the current track; it needs a
        // file it can read on its own.
        ctx.send_untagged(Op::NowPlayingCover {
            id: song.cover_art.clone(),
        });
    }

    // Tell the desktop what is playing.
    let track = mpris::Track::of(&song);
    *ctx.state.track.borrow_mut() = track.clone();
    ctx.mpris.publish(|snapshot| {
        snapshot.metadata = track.metadata.clone();
        snapshot.track_id = track.id.clone();
        snapshot.status = Status::Playing;
        snapshot.position = 0;
    });

    if let Err(error) = ctx.player.play(&ctx.api.stream_url(&song.id)) {
        ctx.set_status(&format!("Не удалось воспроизвести: {error}"));
        return;
    }
    ctx.player.set_paused(false);
    ctx.player.set_volume(ctx.volume.value());
    ctx.player.seek(0.0);
    ctx.send_untagged(Op::Scrobble {
        id: song.id.clone(),
        submission: false,
    });
    ctx.set_status("");
    open_queue_if_visible(ctx);
}

/// Keep the queue page in sync while it is the visible page.
fn open_queue_if_visible(ctx: &Rc<Ctx>) {
    if ctx.queue_visible.get() {
        open_queue(ctx);
    }
}

fn handle_player(ctx: &Rc<Ctx>, state: PlaybackState) {
    ctx.updating_progress.set(true);
    if state.duration > 0.0 {
        ctx.progress.set_range(0.0, state.duration);
        if (ctx.progress.value() - state.position).abs() > 0.5 {
            ctx.progress.set_value(state.position);
        }
    }
    ctx.updating_progress.set(false);
    ctx.time_label.set_text(&format!(
        "{} / {}",
        format_duration(state.position.max(0.0) as u64),
        format_duration(state.duration.max(0.0) as u64)
    ));

    let icon = if state.paused {
        "media-playback-start-symbolic"
    } else {
        "media-playback-pause-symbolic"
    };
    ctx.play_button.set_image(Some(&gtk::Image::from_icon_name(
        Some(icon),
        gtk::IconSize::Button,
    )));

    // Publish the playback state for the desktop media controls.
    publish_playback(ctx, &state);

    let Some(song) = ctx.state.current() else {
        return;
    };

    // Scrobble once, halfway through (or after four minutes, whichever is
    // sooner) — the Subsonic convention.
    let threshold = (song.duration as f64 / 2.0).min(240.0);
    if !ctx.state.submitted.get() && threshold > 0.0 && state.position >= threshold {
        ctx.state.submitted.set(true);
        ctx.send_untagged(Op::Scrobble {
            id: song.id.clone(),
            submission: true,
        });
    }

    // mpv reports idle before the file is actually loaded; ignore that window.
    let loaded = ctx
        .state
        .started_at
        .get()
        .map(|at| at.elapsed() > Duration::from_millis(1500))
        .unwrap_or(false);
    if loaded && state.idle && !state.paused {
        advance(ctx, 1);
    }
}

/// Tell the desktop media controls what the player is doing.
fn publish_playback(ctx: &Rc<Ctx>, state: &PlaybackState) {
    let status = if ctx.state.current().is_none() || ctx.state.stopped.get() {
        Status::Stopped
    } else if state.paused {
        Status::Paused
    } else {
        Status::Playing
    };
    // MPRIS measures the position in microseconds.
    let position = (state.position.max(0.0) * 1_000_000.0) as i64;
    // The volume scale runs from 0 to 100, MPRIS expects 0 to 1.
    let volume = (ctx.volume.value() / 100.0).clamp(0.0, 1.0);
    let queued = ctx.state.queue.borrow().len() > 1;
    let shuffle = ctx.state.shuffle.get();
    let loop_status = match ctx.state.repeat.get() {
        Repeat::Off => LoopStatus::None,
        Repeat::All => LoopStatus::Playlist,
        Repeat::One => LoopStatus::Track,
    };

    ctx.mpris.publish(|snapshot| {
        snapshot.status = status;
        snapshot.position = position;
        snapshot.volume = volume;
        snapshot.can_go_next = queued;
        snapshot.can_go_previous = queued;
        snapshot.shuffle = shuffle;
        snapshot.loop_status = loop_status;
    });
}

/// Act on a command the desktop sent over MPRIS.
fn handle_mpris(ctx: &Rc<Ctx>, command: mpris::Command) {
    match command {
        mpris::Command::Play => {
            ctx.state.stopped.set(false);
            if ctx.state.current().is_none() {
                play_cursor(ctx);
            } else if ctx.player.paused() {
                ctx.player.set_paused(false);
            }
        }
        mpris::Command::Pause => ctx.player.set_paused(true),
        mpris::Command::PlayPause => toggle_playback(ctx),
        mpris::Command::Stop => {
            // "Stopped" is a state of its own in MPRIS: the desktop shows the
            // player as idle, but the track is kept.
            ctx.state.stopped.set(true);
            ctx.player.set_paused(true);
        }
        mpris::Command::Next => {
            ctx.state.stopped.set(false);
            advance(ctx, 1);
        }
        mpris::Command::Previous => {
            ctx.state.stopped.set(false);
            advance(ctx, -1);
        }
        mpris::Command::Seek(offset) => {
            let position = ctx.player.position() + offset as f64 / 1_000_000.0;
            ctx.player.seek(position.max(0.0));
        }
        mpris::Command::SetPosition(position) => {
            ctx.player.seek((position as f64 / 1_000_000.0).max(0.0));
        }
        mpris::Command::SetVolume(volume) => {
            let volume = volume.clamp(0.0, 1.0);
            ctx.player.set_volume(volume);
            ctx.volume.set_value(volume * 100.0);
        }
        mpris::Command::SetShuffle(on) => {
            ctx.state.shuffle.set(on);
            ctx.shuffle_button.set_active(on);
        }
        mpris::Command::SetLoopStatus(status) => {
            let repeat = match status {
                LoopStatus::None => Repeat::Off,
                LoopStatus::Track => Repeat::One,
                LoopStatus::Playlist => Repeat::All,
            };
            set_repeat(ctx, repeat);
        }
        mpris::Command::Raise => ctx.window.present(),
        mpris::Command::Quit => ctx.window.close(),
    }
}

/// The cover of the current track arrived: show it to the desktop.
fn handle_cover_file(ctx: &Rc<Ctx>, path: &str) {
    let track = ctx.state.track.borrow().clone();
    let track = track.with_art(if path.is_empty() { None } else { Some(path) });
    *ctx.state.track.borrow_mut() = track.clone();
    ctx.mpris
        .publish(|snapshot| snapshot.metadata = track.metadata.clone());
}

// ---------------------------------------------------------------------------
// Cover art
// ---------------------------------------------------------------------------

/// Lazily-loaded cover-art cache with a decoded-byte budget.
#[derive(Clone)]
pub(super) struct Covers {
    inner: Rc<CoverInner>,
}

struct CoverInner {
    tx: Sender<Request>,
    cache: RefCell<HashMap<String, gdk_pixbuf::Pixbuf>>,
    bytes: Cell<usize>,
    order: RefCell<VecDeque<String>>,
    pending: RefCell<HashSet<String>>,
    waiting: RefCell<HashMap<String, Vec<glib::WeakRef<gtk::Image>>>>,
}

impl Covers {
    fn new(tx: Sender<Request>) -> Self {
        Self {
            inner: Rc::new(CoverInner {
                tx,
                cache: RefCell::new(HashMap::new()),
                bytes: Cell::new(0),
                order: RefCell::new(VecDeque::new()),
                pending: RefCell::new(HashSet::new()),
                waiting: RefCell::new(HashMap::new()),
            }),
        }
    }

    /// Ask for a cover at a specific size, delivering it to `image` when ready.
    pub(super) fn get_sized(&self, image: &gtk::Image, cover_id: &str, size: u32) {
        if cover_id.is_empty() {
            return;
        }
        let key = cover_key(cover_id, size);
        if let Some(pixbuf) = self.inner.cache.borrow().get(&key) {
            image.set_pixbuf(Some(pixbuf));
            return;
        }
        self.inner
            .waiting
            .borrow_mut()
            .entry(key.clone())
            .or_default()
            .push(image.downgrade());
        if self.inner.pending.borrow_mut().insert(key.clone()) {
            let _ = self.inner.tx.send(Request {
                generation: 0,
                op: Op::Cover {
                    key,
                    id: cover_id.to_owned(),
                    size,
                },
            });
        }
    }

    /// Store a fetched cover and hand it to every widget waiting for it.
    pub(super) fn deliver(&self, key: &str, bytes: &[u8]) {
        self.inner.pending.borrow_mut().remove(key);
        let pixbuf = match gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(bytes.to_vec())) {
            Ok(pixbuf) => pixbuf,
            Err(_) => {
                self.inner.waiting.borrow_mut().remove(key);
                return;
            }
        };
        let size = pixbuf.width() as usize * pixbuf.height() as usize * 4;
        self.inner
            .cache
            .borrow_mut()
            .insert(key.to_owned(), pixbuf.clone());
        self.inner.order.borrow_mut().push_back(key.to_owned());
        self.inner.bytes.set(self.inner.bytes.get() + size);
        self.evict();

        for weak in self
            .inner
            .waiting
            .borrow_mut()
            .remove(key)
            .unwrap_or_default()
        {
            if let Some(image) = weak.upgrade() {
                image.set_pixbuf(Some(&pixbuf));
            }
        }
    }

    /// Drop the oldest covers until the cache fits its budget again.
    fn evict(&self) {
        while self.inner.bytes.get() > COVER_CACHE_BYTES {
            let key = match self.inner.order.borrow_mut().pop_front() {
                Some(key) => key,
                None => break,
            };
            if let Some(pixbuf) = self.inner.cache.borrow_mut().remove(&key) {
                let size = pixbuf.width() as usize * pixbuf.height() as usize * 4;
                self.inner
                    .bytes
                    .set(self.inner.bytes.get().saturating_sub(size));
            }
        }
    }
}

fn cover_key(cover_id: &str, size: u32) -> String {
    format!("{cover_id}@{size}")
}

/// Store the cover art in the cache directory.
///
/// The desktop cannot show an image that needs a login, so the cover of the
/// current track is written to a file and handed over as a `file://` URL.
fn write_cover(bytes: &[u8]) -> Result<String, String> {
    let dir = config::cache_dir();
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let extension = if bytes.starts_with(b"\x89PNG") {
        "png"
    } else {
        "jpg"
    };
    let path = dir.join(format!("now-playing.{extension}"));
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    let url = url::Url::from_file_path(&path)
        .map_err(|_| format!("{} is not a file path", path.display()))?;
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// Lazy cover loading for scrollable pages
// ---------------------------------------------------------------------------

/// Loads cover art only for widgets that are actually on screen.
///
/// A `FlowBox` (or `ListBox`) materializes every child immediately, so asking
/// each image for its bitmap during construction would decode the whole
/// library. Instead the page registers `(widget, image, cover id)` triples here
/// and [`LazyCovers::refresh`] loads the ones intersecting the viewport, plus a
/// margin, whenever the scroll position or size changes.
pub(super) struct LazyCovers {
    covers: Covers,
    scrolled: gtk::ScrolledWindow,
    entries: RefCell<Vec<LazyEntry>>,
    size: u32,
}

struct LazyEntry {
    widget: gtk::Widget,
    image: gtk::Image,
    cover_id: String,
    loaded: Cell<bool>,
}

impl LazyCovers {
    pub(super) fn new(covers: Covers, scrolled: &gtk::ScrolledWindow, size: u32) -> Rc<Self> {
        let lazy = Rc::new(Self {
            covers,
            scrolled: scrolled.clone(),
            entries: RefCell::new(Vec::new()),
            size,
        });
        {
            let weak = Rc::downgrade(&lazy);
            scrolled.vadjustment().connect_value_changed(move |_| {
                if let Some(lazy) = weak.upgrade() {
                    lazy.refresh();
                }
            });
        }
        {
            let weak = Rc::downgrade(&lazy);
            scrolled.connect_size_allocate(move |_, _| {
                if let Some(lazy) = weak.upgrade() {
                    lazy.refresh();
                }
            });
        }
        {
            // The first pass runs before the widgets are realized, when
            // `translate_coordinates` still fails; the map signal is the
            // earliest point at which the geometry is meaningful.
            let weak = Rc::downgrade(&lazy);
            scrolled.connect_map(move |_| {
                if let Some(lazy) = weak.upgrade() {
                    lazy.refresh();
                }
            });
        }
        lazy
    }

    pub(super) fn register(
        &self,
        widget: &impl IsA<gtk::Widget>,
        image: &gtk::Image,
        cover_id: &str,
    ) {
        if cover_id.is_empty() {
            return;
        }
        self.entries.borrow_mut().push(LazyEntry {
            widget: widget.clone().upcast(),
            image: image.clone(),
            cover_id: cover_id.to_owned(),
            loaded: Cell::new(false),
        });
    }

    /// Load the covers of everything near the visible area.
    pub(super) fn refresh(&self) {
        let adjustment = self.scrolled.vadjustment();
        let page_size = adjustment.page_size();
        if page_size <= 1.0 {
            // The viewport has no size yet, so nothing is known about what is
            // on screen. Wait for the next pass instead of loading everything.
            return;
        }
        let top = adjustment.value() - LAZY_MARGIN;
        let bottom = adjustment.value() + page_size + LAZY_MARGIN;
        let mut pending = Vec::new();
        for entry in self.entries.borrow().iter() {
            if entry.loaded.get() {
                continue;
            }
            let allocation = entry.widget.allocation();
            if allocation.height() <= 1 {
                // Not laid out yet; its position is still unknown.
                continue;
            }
            let Some(y) = content_offset(&entry.widget, &self.scrolled) else {
                continue;
            };
            if y + (allocation.height() as f64) < top {
                continue;
            }
            if y > bottom {
                continue;
            }
            entry.loaded.set(true);
            pending.push((entry.image.clone(), entry.cover_id.clone()));
        }
        for (image, cover_id) in pending {
            self.covers.get_sized(&image, &cover_id, self.size);
        }
    }
}

/// Vertical position of `widget` inside the scrolled content.
///
/// GTK's own `translate_coordinates` needs a GdkWindow, which the widgets in a
/// flow box do not have before they are realized. Adding up the allocations of
/// the parent chain gives the same coordinate without that requirement.
fn content_offset(widget: &gtk::Widget, scrolled: &gtk::ScrolledWindow) -> Option<f64> {
    let mut offset = 0.0;
    let mut current = widget.clone();
    loop {
        if current == *scrolled {
            return Some(offset);
        }
        offset += current.allocation().y() as f64;
        current = current.parent()?;
    }
}

// ---------------------------------------------------------------------------
// Worker pool
// ---------------------------------------------------------------------------

/// Spin up the pool and return the channel the UI sends work to.
fn spawn_workers(api: Arc<Subsonic>, ui_tx: Sender<Msg>) -> Sender<Request> {
    let (tx, rx) = mpsc::channel::<Request>();
    let rx = Arc::new(Mutex::new(rx));
    for index in 0..WORKER_THREADS {
        let api = Arc::clone(&api);
        let ui_tx = ui_tx.clone();
        let rx = Arc::clone(&rx);
        std::thread::Builder::new()
            .name(format!("syncpulse-client-{index}"))
            .spawn(move || loop {
                // Hold the lock only to take one job; processing happens after
                // it is released so the other workers can keep draining.
                let request = {
                    let guard = match rx.lock() {
                        Ok(guard) => guard,
                        Err(_) => break,
                    };
                    guard.recv()
                };
                let Ok(request) = request else { break };
                let body = perform(&api, request.op);
                if ui_tx
                    .send(Msg {
                        generation: request.generation,
                        body,
                    })
                    .is_err()
                {
                    break;
                }
            })
            .ok();
    }
    tx
}

fn perform(api: &Subsonic, op: Op) -> MsgBody {
    match op {
        Op::Albums { kind, size, offset } => match api.albums(&kind, size, offset) {
            Ok(items) => MsgBody::Albums { kind, items },
            Err(error) => MsgBody::Error(error),
        },
        Op::Album { id } => match api.album(&id) {
            Ok((album, songs)) => MsgBody::Album { album, songs },
            Err(error) => MsgBody::Error(error),
        },
        Op::Artists => match api.artists() {
            Ok(artists) => MsgBody::Artists(artists),
            Err(error) => MsgBody::Error(error),
        },
        Op::Artist { id } => match api.artist(&id) {
            Ok((artist, albums)) => MsgBody::Artist { artist, albums },
            Err(error) => MsgBody::Error(error),
        },
        Op::Playlists => match api.playlists() {
            Ok(playlists) => MsgBody::Playlists(playlists),
            Err(error) => MsgBody::Error(error),
        },
        Op::Playlist { id } => match api.playlist(&id) {
            Ok((playlist, songs)) => MsgBody::Playlist { playlist, songs },
            Err(error) => MsgBody::Error(error),
        },
        Op::Starred => match api.starred() {
            Ok(starred) => MsgBody::Starred(starred),
            Err(error) => MsgBody::Error(error),
        },
        Op::Random { size } => match api.random_songs(size) {
            Ok(songs) => MsgBody::Random(songs),
            Err(error) => MsgBody::Error(error),
        },
        Op::Search { query } => match api.search(&query) {
            Ok(results) => MsgBody::Search(results),
            Err(error) => MsgBody::Error(error),
        },
        Op::Cover { key, id, size } => match api.cover_bytes(&id, size) {
            Ok(bytes) => {
                tracing::debug!("client: cover {id}@{size} -> {} bytes", bytes.len());
                MsgBody::Cover { key, bytes }
            }
            Err(error) => {
                tracing::warn!("client: cover {id}@{size} failed: {error}");
                MsgBody::Cover {
                    key,
                    bytes: Vec::new(),
                }
            }
        },
        Op::NowPlayingCover { id } => match api.cover_bytes(&id, 512) {
            Ok(bytes) => match write_cover(&bytes) {
                Ok(path) => MsgBody::CoverFile { path },
                Err(error) => {
                    tracing::warn!("client: cover file failed: {error}");
                    MsgBody::CoverFile {
                        path: String::new(),
                    }
                }
            },
            Err(error) => {
                tracing::warn!("client: now playing cover failed: {error}");
                MsgBody::CoverFile {
                    path: String::new(),
                }
            }
        },
        Op::Star { kind, id, on } => match api.set_star(kind, &id, on) {
            Ok(()) => MsgBody::Status(if on {
                "Добавлено в избранное".to_owned()
            } else {
                "Убрано из избранного".to_owned()
            }),
            Err(error) => MsgBody::Error(error),
        },
        Op::Scrobble { id, submission } => {
            api.scrobble(&id, submission);
            MsgBody::Status(String::new())
        }
        Op::Rescan => match api.start_scan() {
            Ok(()) => MsgBody::Status("Сканирование библиотеки запущено".to_owned()),
            Err(error) => MsgBody::Error(error),
        },
    }
}
