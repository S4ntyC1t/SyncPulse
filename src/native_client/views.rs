//! Page builders for the native client.
//!
//! Each function returns a ready-to-add widget for the content area. Pages that
//! can grow long (album grids, track lists, search results) register their cover
//! images with a [`LazyCovers`] so bitmaps are only decoded once they scroll
//! into view.

use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use super::api::{
    format_duration, format_playtime, Album, Artist, Playlist, SearchResults, Song, StarKind,
    Starred,
};
use super::ui::{self, Ctx, LazyCovers, Op, COVER_ROW};

/// Wrap a widget in a vertically scrolling viewport.
fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(child)
        .build()
}

fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_markup(&format!("<b><big>{}</big></b>", escape(text)));
    label
}

fn section(text: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_margin_top(12);
    label.set_markup(&format!("<b>{}</b>", escape(text)));
    label
}

fn dim(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.style_context().add_class("dim-label");
    label
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn titled_root() -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root
}

/// Re-run lazy cover loading once GTK has allocated the widgets.
fn refresh_when_idle(lazy: &Rc<LazyCovers>) {
    // The first pass runs while the page is still being built; the later ones
    // cover the frames in which GTK realizes the widgets and gives them their
    // final size - until then the viewport geometry is still unknown.
    for delay in [0_u32, 120, 400] {
        let lazy = lazy.clone();
        if delay == 0 {
            glib::idle_add_local(move || {
                lazy.refresh();
                glib::ControlFlow::Break
            });
        } else {
            glib::timeout_add_local(std::time::Duration::from_millis(delay as u64), move || {
                lazy.refresh();
                glib::ControlFlow::Break
            });
        }
    }
}
// ---------------------------------------------------------------------------
// Album grid
// ---------------------------------------------------------------------------

/// A grid of album cards, optionally preceded by a header (artist page).
pub(super) fn album_grid(ctx: &Rc<Ctx>, albums: Vec<Album>) -> gtk::Widget {
    album_grid_with_header(ctx, albums, None)
}

pub(super) fn album_grid_with_header(
    ctx: &Rc<Ctx>,
    albums: Vec<Album>,
    header: Option<gtk::Widget>,
) -> gtk::Widget {
    let content = titled_root();
    if let Some(header) = header {
        content.add(&header);
    }

    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_homogeneous(true);
    flow.set_column_spacing(8);
    flow.set_row_spacing(10);
    flow.set_max_children_per_line(12);
    flow.set_min_children_per_line(2);
    content.add(&flow);

    let scrolled = scrolled(&content);
    let lazy = LazyCovers::new(ctx.covers.clone(), &scrolled, ui::COVER_GRID);

    for album in albums {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
        card.set_size_request(148, -1);

        let image = gtk::Image::from_pixbuf(None);
        image.set_size_request(148, 148);
        image.set_pixel_size(148);
        card.add(&image);

        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(20);
        name.set_markup(&format!("<b>{}</b>", escape(&album.name)));
        card.add(&name);

        let artist = dim(&album.subtitle());
        artist.set_max_width_chars(20);
        card.add(&artist);

        let event = gtk::EventBox::new();
        event.set_visible_window(false);
        event.add(&card);
        event.set_tooltip_text(Some(&format!(
            "{}\n{}\n{} треков · {}",
            album.name,
            album.artist,
            album.song_count,
            format_playtime(album.duration)
        )));
        {
            let ctx = Rc::clone(ctx);
            let album_id = album.id.clone();
            event.connect_button_release_event(move |_, event| {
                if event.button() == 1 {
                    ui::open_album(&ctx, &album_id);
                }
                glib::Propagation::Proceed
            });
        }
        flow.add(&event);
        lazy.register(&event, &image, &album.cover_art);
    }

    refresh_when_idle(&lazy);
    scrolled.upcast()
}

// ---------------------------------------------------------------------------
// Detail pages
// ---------------------------------------------------------------------------

/// A star toggle for an album or artist.
fn star_button(ctx: &Rc<Ctx>, kind: StarKind, id: &str, starred: bool) -> gtk::Button {
    let button = gtk::Button::with_label(if starred {
        "Убрать из избранного"
    } else {
        "В избранное"
    });
    button.set_image(Some(&gtk::Image::from_icon_name(
        Some("emblem-favorite-symbolic"),
        gtk::IconSize::Button,
    )));
    button.set_always_show_image(true);
    let ctx = Rc::clone(ctx);
    let id = id.to_owned();
    button.connect_clicked(move |_| {
        ctx.send_untagged(Op::Star {
            kind,
            id: id.clone(),
            on: !starred,
        });
    });
    button
}

/// A large cover + title block used at the top of detail pages.
/// What a detail page header shows above the track list.
struct Detail<'a> {
    cover_art: &'a str,
    title: &'a str,
    subtitle: &'a str,
    meta: &'a str,
    /// The artist the subtitle links to, if there is one.
    artist_id: Option<&'a str>,
    /// The kind of item, its id and whether it is starred.
    star: Option<(StarKind, &'a str, bool)>,
}

fn detail_header(ctx: &Rc<Ctx>, detail: Detail<'_>, songs: &[Song]) -> gtk::Widget {
    let Detail {
        cover_art,
        title,
        subtitle,
        meta,
        artist_id,
        star,
    } = detail;
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    header.set_margin_bottom(10);

    let image = gtk::Image::from_pixbuf(None);
    image.set_size_request(168, 168);
    image.set_pixel_size(168);
    ctx.covers.get_sized(&image, cover_art, ui::COVER_GRID);
    header.add(&image);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.set_valign(gtk::Align::Center);
    column.set_hexpand(true);
    column.add(&heading(title));

    if let Some(artist_id) = artist_id.filter(|id| !id.is_empty()) {
        let button = gtk::Button::with_label(subtitle);
        button.set_halign(gtk::Align::Start);
        button.set_relief(gtk::ReliefStyle::None);
        {
            let ctx = Rc::clone(ctx);
            let artist_id = artist_id.to_owned();
            button.connect_clicked(move |_| ui::open_artist(&ctx, &artist_id));
        }
        column.add(&button);
    } else if !subtitle.is_empty() {
        column.add(&dim(subtitle));
    }
    if !meta.is_empty() {
        column.add(&dim(meta));
    }

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    if !songs.is_empty() {
        let play = gtk::Button::with_label("Играть");
        play.set_image(Some(&gtk::Image::from_icon_name(
            Some("media-playback-start-symbolic"),
            gtk::IconSize::Button,
        )));
        play.set_always_show_image(true);
        {
            let ctx = Rc::clone(ctx);
            let songs = songs.to_vec();
            play.connect_clicked(move |_| ui::play_now(&ctx, songs.clone(), 0));
        }
        actions.add(&play);

        let queue = gtk::Button::with_label("В очередь");
        {
            let ctx = Rc::clone(ctx);
            let songs = songs.to_vec();
            queue.connect_clicked(move |_| ui::enqueue(&ctx, songs.clone(), false));
        }
        actions.add(&queue);

        let random = gtk::Button::with_label("Перемешать");
        {
            let ctx = Rc::clone(ctx);
            let songs = songs.to_vec();
            random.connect_clicked(move |_| {
                ctx.state.shuffle.set(true);
                ctx.shuffle_button.set_active(true);
                ui::play_now(&ctx, songs.clone(), 0);
            });
        }
        actions.add(&random);
    }
    if let Some((kind, id, starred)) = star {
        actions.add(&star_button(ctx, kind, id, starred));
    }
    column.add(&actions);
    header.add(&column);
    header.upcast()
}

pub(super) fn album_detail(ctx: &Rc<Ctx>, album: Album, songs: Vec<Song>) -> gtk::Widget {
    let root = titled_root();
    let meta = format!(
        "{} · {} треков · {}",
        album
            .year
            .map(|year| year.to_string())
            .unwrap_or_else(|| "—".to_owned()),
        album.song_count,
        format_playtime(album.duration)
    );
    root.add(&detail_header(
        ctx,
        Detail {
            cover_art: &album.cover_art,
            title: &album.name,
            subtitle: &album.artist,
            meta: &meta,
            artist_id: Some(&album.artist_id),
            star: Some((StarKind::Album, &album.id, album.starred.is_some())),
        },
        &songs,
    ));

    let scrolled = scrolled(&root);
    let lazy = LazyCovers::new(ctx.covers.clone(), &scrolled, COVER_ROW);
    let playing = ctx.state.current();
    root.add(&song_list(ctx, &songs, playing.as_ref(), false, &lazy));
    refresh_when_idle(&lazy);
    scrolled.upcast()
}

pub(super) fn playlist_detail(ctx: &Rc<Ctx>, playlist: Playlist, songs: Vec<Song>) -> gtk::Widget {
    let root = titled_root();
    let meta = format!(
        "{} · {} · {}",
        playlist.owner,
        playlist.song_count,
        format_playtime(playlist.duration)
    );
    root.add(&detail_header(
        ctx,
        Detail {
            cover_art: &playlist.cover_art,
            title: &playlist.name,
            subtitle: &playlist.comment,
            meta: &meta,
            artist_id: None,
            star: None,
        },
        &songs,
    ));

    let scrolled = scrolled(&root);
    let lazy = LazyCovers::new(ctx.covers.clone(), &scrolled, COVER_ROW);
    let playing = ctx.state.current();
    root.add(&song_list(ctx, &songs, playing.as_ref(), false, &lazy));
    refresh_when_idle(&lazy);
    scrolled.upcast()
}

pub(super) fn artist_detail(ctx: &Rc<Ctx>, artist: Artist, albums: Vec<Album>) -> gtk::Widget {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    header.set_margin_bottom(10);
    let image = gtk::Image::from_pixbuf(None);
    image.set_size_request(140, 140);
    image.set_pixel_size(140);
    ctx.covers
        .get_sized(&image, &artist.cover_art, ui::COVER_GRID);
    header.add(&image);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.set_valign(gtk::Align::Center);
    column.add(&heading(&artist.name));
    column.add(&dim(&format!("{} альбомов", artist.album_count)));
    let star = star_button(ctx, StarKind::Artist, &artist.id, artist.starred.is_some());
    star.set_halign(gtk::Align::Start);
    column.add(&star);
    header.add(&column);

    album_grid_with_header(ctx, albums, Some(header.upcast()))
}

// ---------------------------------------------------------------------------
// Simple lists
// ---------------------------------------------------------------------------

pub(super) fn artist_list(ctx: &Rc<Ctx>, artists: Vec<Artist>) -> gtk::Widget {
    let root = titled_root();
    root.add(&section(&format!("Исполнители · {}", artists.len())));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    root.add(&list);

    let scrolled = scrolled(&root);
    let lazy = LazyCovers::new(ctx.covers.clone(), &scrolled, COVER_ROW);

    let mut ids = Vec::new();
    for artist in artists {
        let row = gtk::ListBoxRow::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_top(4);
        hbox.set_margin_bottom(4);
        hbox.set_margin_start(6);
        hbox.set_margin_end(6);
        let image = thumb_image();
        hbox.add(&image);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.set_valign(gtk::Align::Center);
        column.add(&strong_label(&artist.name));
        column.add(&dim(&format!("{} альбомов", artist.album_count)));
        hbox.add(&column);
        row.add(&hbox);
        list.add(&row);
        lazy.register(&row, &image, &artist.cover_art);
        ids.push(artist.id.clone());
    }
    {
        let ctx = Rc::clone(ctx);
        list.connect_row_activated(move |_, row| {
            if let Some(id) = ids.get(row.index().max(0) as usize) {
                ui::open_artist(&ctx, id);
            }
        });
    }

    refresh_when_idle(&lazy);
    scrolled.upcast()
}

pub(super) fn playlist_list(ctx: &Rc<Ctx>, playlists: Vec<Playlist>) -> gtk::Widget {
    let root = titled_root();
    root.add(&section(&format!("Плейлисты · {}", playlists.len())));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    root.add(&list);

    let scrolled = scrolled(&root);
    let lazy = LazyCovers::new(ctx.covers.clone(), &scrolled, COVER_ROW);

    let mut ids = Vec::new();
    for playlist in playlists {
        let row = gtk::ListBoxRow::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_top(4);
        hbox.set_margin_bottom(4);
        hbox.set_margin_start(6);
        hbox.set_margin_end(6);
        let image = thumb_image();
        hbox.add(&image);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.set_valign(gtk::Align::Center);
        column.add(&strong_label(&playlist.name));
        column.add(&dim(&playlist.subtitle()));
        hbox.add(&column);
        row.add(&hbox);
        list.add(&row);
        lazy.register(&row, &image, &playlist.cover_art);
        ids.push(playlist.id.clone());
    }
    {
        let ctx = Rc::clone(ctx);
        list.connect_row_activated(move |_, row| {
            if let Some(id) = ids.get(row.index().max(0) as usize) {
                ui::open_playlist(&ctx, id);
            }
        });
    }

    refresh_when_idle(&lazy);
    scrolled.upcast()
}

/// A titled page holding only a list of songs (queue, random tracks).
pub(super) fn song_page(
    ctx: &Rc<Ctx>,
    title: &str,
    subtitle: &str,
    songs: Vec<Song>,
    playing: Option<&Song>,
    is_queue: bool,
) -> gtk::Widget {
    let root = titled_root();
    root.add(&heading(title));
    root.add(&dim(subtitle));

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_margin_top(6);
    actions.set_margin_bottom(6);
    if !songs.is_empty() {
        let play = gtk::Button::with_label("Играть");
        {
            let ctx = Rc::clone(ctx);
            let songs = songs.clone();
            play.connect_clicked(move |_| {
                if is_queue {
                    ui::play_position(&ctx, 0);
                } else {
                    ui::play_now(&ctx, songs.clone(), 0);
                }
            });
        }
        actions.add(&play);

        let clear = gtk::Button::with_label("Очистить");
        {
            let ctx = Rc::clone(ctx);
            clear.connect_clicked(move |_| {
                ctx.state.queue.borrow_mut().clear();
                ui::open_queue(&ctx);
            });
        }
        actions.add(&clear);

        let shuffle = gtk::Button::with_label(if ctx.state.shuffle.get() {
            "Перемешано"
        } else {
            "Перемешать"
        });
        {
            let ctx = Rc::clone(ctx);
            shuffle.connect_clicked(move |_| {
                let on = !ctx.state.shuffle.get();
                ctx.state.shuffle.set(on);
                ctx.shuffle_button.set_active(on);
                if on {
                    ctx.state.queue.borrow_mut().reshuffle();
                } else {
                    let mut queue = ctx.state.queue.borrow_mut();
                    let current = queue.current();
                    queue.order = (0..queue.songs.len()).collect();
                    queue.cursor = current
                        .and_then(|song| queue.songs.iter().position(|item| item.id == song.id))
                        .unwrap_or(0);
                }
                ui::open_queue(&ctx);
            });
        }
        if is_queue {
            actions.add(&shuffle);
        }
    }
    root.add(&actions);

    let scrolled = scrolled(&root);
    let lazy = LazyCovers::new(ctx.covers.clone(), &scrolled, COVER_ROW);
    root.add(&song_list(ctx, &songs, playing, is_queue, &lazy));
    refresh_when_idle(&lazy);
    scrolled.upcast()
}

pub(super) fn starred_page(ctx: &Rc<Ctx>, starred: Starred) -> gtk::Widget {
    let root = titled_root();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add(&content);
    let scrolled = scrolled(&root);
    let lazy = LazyCovers::new(ctx.covers.clone(), &scrolled, ui::COVER_GRID);

    if !starred.artists.is_empty() {
        content.add(&section("Исполнители"));
        content.add(&artist_rows(ctx, &starred.artists, &lazy));
    }
    if !starred.albums.is_empty() {
        content.add(&section("Альбомы"));
        content.add(&album_flow(ctx, &starred.albums, &lazy));
    }
    if !starred.songs.is_empty() {
        content.add(&section("Треки"));
        content.add(&song_list(ctx, &starred.songs, None, false, &lazy));
    }
    if starred.artists.is_empty() && starred.albums.is_empty() && starred.songs.is_empty() {
        content.add(&dim("Пока ничего не отмечено."));
    }

    refresh_when_idle(&lazy);
    scrolled.upcast()
}

pub(super) fn search_page(ctx: &Rc<Ctx>, results: SearchResults) -> gtk::Widget {
    if results.is_empty() {
        let root = titled_root();
        root.add(&heading("Ничего не найдено"));
        root.add(&dim(
            "Попробуйте другой запрос — Navidrome ищет по названию, альбому и исполнителю.",
        ));
        let scrolled = scrolled(&root);
        return scrolled.upcast();
    }

    let root = titled_root();
    root.add(&heading("Результаты поиска"));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add(&content);
    let scrolled = scrolled(&root);
    let lazy = LazyCovers::new(ctx.covers.clone(), &scrolled, ui::COVER_GRID);

    if !results.artists.is_empty() {
        content.add(&section(&format!(
            "Исполнители · {}",
            results.artists.len()
        )));
        content.add(&artist_rows(ctx, &results.artists, &lazy));
    }
    if !results.albums.is_empty() {
        content.add(&section(&format!("Альбомы · {}", results.albums.len())));
        content.add(&album_flow(ctx, &results.albums, &lazy));
    }
    if !results.songs.is_empty() {
        content.add(&section(&format!("Треки · {}", results.songs.len())));
        content.add(&song_list(ctx, &results.songs, None, false, &lazy));
    }

    refresh_when_idle(&lazy);
    scrolled.upcast()
}

// ---------------------------------------------------------------------------
// Reusable pieces
// ---------------------------------------------------------------------------

fn thumb_image() -> gtk::Image {
    let image = gtk::Image::from_pixbuf(None);
    let size = COVER_ROW as i32;
    image.set_size_request(size, size);
    image.set_pixel_size(size);
    image
}

fn strong_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_markup(&format!("<b>{}</b>", escape(text)));
    label
}

/// Artist rows sharing an existing scroll area (used by search and starred).
fn artist_rows(ctx: &Rc<Ctx>, artists: &[Artist], lazy: &Rc<LazyCovers>) -> gtk::Widget {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    for artist in artists {
        let row = gtk::ListBoxRow::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_top(4);
        hbox.set_margin_bottom(4);
        hbox.set_margin_start(6);
        hbox.set_margin_end(6);
        let image = thumb_image();
        hbox.add(&image);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.set_valign(gtk::Align::Center);
        column.add(&strong_label(&artist.name));
        column.add(&dim(&format!("{} альбомов", artist.album_count)));
        hbox.add(&column);
        row.add(&hbox);
        list.add(&row);
        lazy.register(&row, &image, &artist.cover_art);

        let ctx = Rc::clone(ctx);
        let id = artist.id.clone();
        row.connect_activate(move |_| ui::open_artist(&ctx, &id));
    }
    list.upcast()
}

/// An album grid sharing an existing scroll area.
fn album_flow(ctx: &Rc<Ctx>, albums: &[Album], lazy: &Rc<LazyCovers>) -> gtk::Widget {
    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_homogeneous(true);
    flow.set_column_spacing(8);
    flow.set_row_spacing(10);
    flow.set_max_children_per_line(12);
    flow.set_min_children_per_line(2);
    for album in albums {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
        card.set_size_request(148, -1);
        let image = gtk::Image::from_pixbuf(None);
        image.set_size_request(148, 148);
        image.set_pixel_size(148);
        card.add(&image);
        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(20);
        name.set_markup(&format!("<b>{}</b>", escape(&album.name)));
        card.add(&name);
        card.add(&dim(&album.subtitle()));

        let event = gtk::EventBox::new();
        event.set_visible_window(false);
        event.add(&card);
        {
            let ctx = Rc::clone(ctx);
            let id = album.id.clone();
            event.connect_button_release_event(move |_, event| {
                if event.button() == 1 {
                    ui::open_album(&ctx, &id);
                }
                glib::Propagation::Proceed
            });
        }
        flow.add(&event);
        lazy.register(&event, &image, &album.cover_art);
    }
    flow.upcast()
}

/// A list of song rows.
fn song_list(
    ctx: &Rc<Ctx>,
    songs: &[Song],
    playing: Option<&Song>,
    is_queue: bool,
    lazy: &Rc<LazyCovers>,
) -> gtk::Widget {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    let playing_id = playing.map(|song| song.id.clone());

    for (index, song) in songs.iter().enumerate() {
        let row = gtk::ListBoxRow::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_top(3);
        hbox.set_margin_bottom(3);
        hbox.set_margin_start(6);
        hbox.set_margin_end(6);

        let image = thumb_image();
        hbox.add(&image);

        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.set_valign(gtk::Align::Center);
        column.set_hexpand(true);
        let marker = if playing_id.as_deref() == Some(song.id.as_str()) {
            "▶ "
        } else {
            ""
        };
        column.add(&strong_label(&format!("{marker}{}", song.title)));
        column.add(&dim(&song.subtitle()));
        hbox.add(&column);

        let duration = dim(&format_duration(song.duration));
        duration.set_valign(gtk::Align::Center);
        hbox.add(&duration);

        row.add(&hbox);
        list.add(&row);
        lazy.register(&row, &image, &song.cover_art);
        attach_song_menu(ctx, &row, song, index, is_queue);
    }

    {
        let ctx = Rc::clone(ctx);
        let songs = songs.to_vec();
        list.connect_row_activated(move |_, row| {
            let index = row.index().max(0) as usize;
            if is_queue {
                // In the queue, activating a row jumps to it rather than
                // replacing the queue with itself.
                ui::play_position(&ctx, index);
            } else {
                ui::play_now(&ctx, songs.clone(), index);
            }
        });
    }

    if let Some(playing_id) = playing_id {
        if let Some(index) = songs.iter().position(|song| song.id == playing_id) {
            if let Some(row) = list.row_at_index(index as i32) {
                list.select_row(Some(&row));
            }
        }
    }
    list.upcast()
}

/// Attach a right-click menu to a song row.
fn attach_song_menu(
    ctx: &Rc<Ctx>,
    row: &gtk::ListBoxRow,
    song: &Song,
    index: usize,
    is_queue: bool,
) {
    let menu = gtk::Menu::new();
    let play = gtk::MenuItem::with_label("Играть");
    {
        let ctx = Rc::clone(ctx);
        let song = song.clone();
        play.connect_activate(move |_| ui::play_now(&ctx, vec![song.clone()], 0));
    }
    menu.append(&play);

    let next = gtk::MenuItem::with_label("Играть следующим");
    {
        let ctx = Rc::clone(ctx);
        let song = song.clone();
        next.connect_activate(move |_| ui::enqueue(&ctx, vec![song.clone()], true));
    }
    menu.append(&next);

    let append = gtk::MenuItem::with_label("Добавить в очередь");
    {
        let ctx = Rc::clone(ctx);
        let song = song.clone();
        append.connect_activate(move |_| ui::enqueue(&ctx, vec![song.clone()], false));
    }
    menu.append(&append);

    let album = gtk::MenuItem::with_label("Перейти к альбому");
    {
        let ctx = Rc::clone(ctx);
        let album_id = song.album_id.clone();
        album.connect_activate(move |_| {
            if !album_id.is_empty() {
                ui::open_album(&ctx, &album_id);
            }
        });
    }
    menu.append(&album);

    let artist = gtk::MenuItem::with_label("Перейти к исполнителю");
    {
        let ctx = Rc::clone(ctx);
        let artist_id = song.artist_id.clone();
        artist.connect_activate(move |_| {
            if !artist_id.is_empty() {
                ui::open_artist(&ctx, &artist_id);
            }
        });
    }
    menu.append(&artist);

    let star = gtk::MenuItem::with_label(if song.starred.is_some() {
        "Убрать из избранного"
    } else {
        "В избранное"
    });
    {
        let ctx = Rc::clone(ctx);
        let id = song.id.clone();
        let on = song.starred.is_none();
        star.connect_activate(move |_| {
            ctx.send_untagged(Op::Star {
                kind: StarKind::Song,
                id: id.clone(),
                on,
            });
        });
    }
    menu.append(&star);

    if is_queue {
        let remove = gtk::MenuItem::with_label("Убрать из очереди");
        {
            let ctx = Rc::clone(ctx);
            remove.connect_activate(move |_| ui::remove_position(&ctx, index));
        }
        menu.append(&remove);
    }

    menu.show_all();
    {
        // The handler owns the menu, so the popup stays alive as long as the
        // row does.
        row.connect_button_press_event(move |_, event| {
            if event.button() == 3 {
                menu.popup_at_pointer(Some(event));
            }
            glib::Propagation::Proceed
        });
    }
}
