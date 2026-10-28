//! MPRIS2: let the desktop drive the native client.
//!
//! Every modern desktop keeps a list of the media players that live on the
//! session bus and offers them in the system tray ("стопить как аудио" in KDE:
//! the media widget of Plasma). To join that list the client has to own the
//! bus name `org.mpris.MediaPlayer2.syncpulse` and export two interfaces on
//! `/org/mpris/MediaPlayer2`:
//!
//! * `org.mpris.MediaPlayer2` — who we are and window control,
//! * `org.mpris.MediaPlayer2.Player` — the playback state and the transport.
//!
//! The desktop talks to the *service thread*, but the player belongs to the
//! GTK thread, so the two exchange messages: the service pushes commands into
//! a channel the UI drains, and the UI publishes the current state into a
//! shared snapshot the service reads and announces with `PropertiesChanged`.
//!
//! Nothing here is required for the client to work: when there is no session
//! bus (a bare TTY, a container) the service simply never comes up.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::fdo::Properties;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use super::api::Song;

/// The bus name the client owns.
const BUS_NAME: &str = "org.mpris.MediaPlayer2.syncpulse";
/// Where both interfaces live.
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
/// The interface that carries the playback state.
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
/// The desktop entry the shell uses to find our icon.
const DESKTOP_ENTRY: &str = "org.navidromehub.syncpulse";
/// How often the playback position is announced.
const POSITION_INTERVAL: Duration = Duration::from_millis(1000);

/// What the desktop asked the player to do.
#[derive(Clone, Debug)]
pub(crate) enum Command {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Relative jump, in microseconds.
    Seek(i64),
    /// Absolute position, in microseconds.
    SetPosition(i64),
    SetVolume(f64),
    SetShuffle(bool),
    SetLoopStatus(LoopStatus),
    /// Show the client window.
    Raise,
    /// Close the client.
    Quit,
}

/// The playback state as the desktop sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Status {
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Stopped => "Stopped",
            Status::Playing => "Playing",
            Status::Paused => "Paused",
        }
    }
}

/// How the queue repeats, in the vocabulary of MPRIS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum LoopStatus {
    #[default]
    None,
    Track,
    Playlist,
}

impl LoopStatus {
    fn as_str(self) -> &'static str {
        match self {
            LoopStatus::None => "None",
            LoopStatus::Track => "Track",
            LoopStatus::Playlist => "Playlist",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "None" => Some(LoopStatus::None),
            "Track" => Some(LoopStatus::Track),
            "Playlist" => Some(LoopStatus::Playlist),
            _ => None,
        }
    }
}

/// How the desktop sees the current track.
#[derive(Clone, Debug, Default)]
pub(crate) struct Track {
    /// The object path that identifies the track over D-Bus.
    pub id: String,
    /// The metadata dictionary the desktop reads.
    pub metadata: HashMap<String, OwnedValue>,
}

impl Track {
    /// Describe a song the way MPRIS expects it.
    pub(crate) fn of(song: &Song) -> Self {
        let id = format!("/org/mpris/MediaPlayer2/Track/{}", sanitize(&song.id));
        let mut metadata = HashMap::new();
        let mut insert = |key: &str, value: Value<'_>| {
            if let Ok(value) = OwnedValue::try_from(value) {
                metadata.insert(key.to_owned(), value);
            }
        };

        if let Ok(path) = ObjectPath::try_from(id.as_str()) {
            insert("mpris:trackid", Value::from(path));
        }
        // MPRIS measures the length in microseconds.
        insert(
            "mpris:length",
            Value::from(song.duration as i64 * 1_000_000),
        );
        insert("xesam:title", Value::from(song.title.clone()));
        if !song.album.is_empty() {
            insert("xesam:album", Value::from(song.album.clone()));
        }
        if !song.artist.is_empty() {
            insert("xesam:artist", Value::from(vec![song.artist.clone()]));
        }

        Self { id, metadata }
    }

    /// Show a cover file as the album art (or drop it again).
    pub(crate) fn with_art(self, url: Option<&str>) -> Self {
        let mut track = self;
        match url {
            Some(url) => {
                if let Ok(value) = OwnedValue::try_from(Value::from(url.to_owned())) {
                    track.metadata.insert("mpris:artUrl".to_owned(), value);
                }
            }
            None => {
                track.metadata.remove("mpris:artUrl");
            }
        }
        track
    }
}

/// Turn a song id into something an object path accepts.
fn sanitize(id: &str) -> String {
    let mut clean = String::with_capacity(id.len());
    for character in id.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            clean.push(character);
        } else {
            clean.push('_');
        }
    }
    if clean.is_empty() {
        clean.push('0');
    }
    clean
}

/// The state the desktop reads.
#[derive(Clone, Debug, Default)]
pub(crate) struct Snapshot {
    pub status: Status,
    /// The current track, in the vocabulary of MPRIS (`mpris:trackid`,
    /// `xesam:title`, ...).
    pub metadata: HashMap<String, OwnedValue>,
    /// The object path of the current track, used to reject a `SetPosition`
    /// that belongs to another song.
    pub track_id: String,
    /// The playback position, in microseconds.
    pub position: i64,
    /// The volume, between 0 and 1.
    pub volume: f64,
    pub shuffle: bool,
    pub loop_status: LoopStatus,
    pub can_go_next: bool,
    pub can_go_previous: bool,
}

/// The state and the command channel both sides of the service share.
struct Shared {
    state: Mutex<Snapshot>,
    commands: Sender<Command>,
}

impl Shared {
    fn snapshot(&self) -> Snapshot {
        match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

/// The MPRIS service of the client.
///
/// The handle lives in the UI: it publishes the playback state and drains the
/// commands the desktop sends.
pub(crate) struct Mpris {
    shared: Arc<Shared>,
    /// Wakes the service thread after the state changed.
    notify: Sender<()>,
    /// The commands the desktop sent, drained by the UI thread.
    commands: Receiver<Command>,
}

impl Mpris {
    /// Start the service.
    ///
    /// The thread it runs in may fail to reach the bus (a headless session);
    /// the client then keeps working, just without the desktop integration.
    pub(crate) fn start() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (notify_tx, notify_rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(Snapshot::default()),
            commands: command_tx,
        });

        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("syncpulse-mpris".to_owned())
            .spawn(move || {
                if let Err(error) = serve(worker, notify_rx) {
                    tracing::warn!("mpris: {error}");
                }
            })
            .ok();

        Self {
            shared,
            notify: notify_tx,
            commands: command_rx,
        }
    }

    /// The commands the desktop sent since the last call.
    pub(crate) fn drain(&self) -> Vec<Command> {
        self.commands.try_iter().collect()
    }

    /// Publish a new state and let the service announce it.
    pub(crate) fn publish(&self, update: impl FnOnce(&mut Snapshot)) {
        {
            let mut state = match self.shared.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            update(&mut state);
        }
        let _ = self.notify.send(());
    }
}

/// Own the bus name, export the interfaces and announce the changes.
fn serve(shared: Arc<Shared>, notify: Receiver<()>) -> Result<(), String> {
    let connection = zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
    let server = connection.object_server();
    server
        .at(
            OBJECT_PATH,
            Root {
                shared: Arc::clone(&shared),
            },
        )
        .map_err(|error| error.to_string())?;
    server
        .at(
            OBJECT_PATH,
            Player {
                shared: Arc::clone(&shared),
            },
        )
        .map_err(|error| error.to_string())?;
    connection
        .request_name(BUS_NAME)
        .map_err(|error| error.to_string())?;
    tracing::info!("mpris: serving {BUS_NAME} on {OBJECT_PATH}");

    let emitter =
        SignalEmitter::new(connection.inner(), OBJECT_PATH).map_err(|error| error.to_string())?;
    let mut previous = shared.snapshot();
    let mut announced = Instant::now();

    while notify.recv().is_ok() {
        let current = shared.snapshot();
        let mut changed: HashMap<&str, Value> = HashMap::new();

        if current.status != previous.status {
            changed.insert("PlaybackStatus", Value::from(current.status.as_str()));
        }
        if current.metadata != previous.metadata {
            changed.insert("Metadata", Value::from(current.metadata.clone()));
        }
        if current.volume != previous.volume {
            changed.insert("Volume", Value::from(current.volume));
        }
        if current.shuffle != previous.shuffle {
            changed.insert("Shuffle", Value::from(current.shuffle));
        }
        if current.loop_status != previous.loop_status {
            changed.insert("LoopStatus", Value::from(current.loop_status.as_str()));
        }
        if current.can_go_next != previous.can_go_next {
            changed.insert("CanGoNext", Value::from(current.can_go_next));
        }
        if current.can_go_previous != previous.can_go_previous {
            changed.insert("CanGoPrevious", Value::from(current.can_go_previous));
        }

        // The position moves with every poll; announce it once a second, and
        // right away when the track jumps (a seek).
        let jumped = (current.position - previous.position).abs() > 2_000_000;
        if current.position != previous.position
            && (jumped || announced.elapsed() >= POSITION_INTERVAL)
        {
            changed.insert("Position", Value::from(current.position));
            announced = Instant::now();
        }

        if !changed.is_empty() {
            let result = zbus::block_on(Properties::properties_changed(
                &emitter,
                zbus::names::InterfaceName::from_static_str_unchecked(PLAYER_INTERFACE),
                changed,
                Cow::Borrowed(&[]),
            ));
            if let Err(error) = result {
                tracing::warn!("mpris: PropertiesChanged failed: {error}");
            }
        }

        previous = current;
    }

    Ok(())
}

/// The `org.mpris.MediaPlayer2` interface: who we are.
struct Root {
    shared: Arc<Shared>,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    /// The name the desktop shows.
    #[zbus(property)]
    fn identity(&self) -> String {
        "SyncPulse".to_owned()
    }

    /// The desktop entry the shell uses to find our icon.
    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        DESKTOP_ENTRY.to_owned()
    }

    /// The icon the desktop shows next to the player.
    #[zbus(property)]
    fn icon(&self) -> String {
        DESKTOP_ENTRY.to_owned()
    }

    /// The URI schemes the desktop may hand over.
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        vec!["http".to_owned(), "https".to_owned()]
    }

    /// mpv plays whatever Navidrome serves, so no fixed list is advertised.
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }

    /// The playlist is not exposed over D-Bus.
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }

    /// Bring the client window to the front.
    fn raise(&self) {
        self.shared.send(Command::Raise);
    }

    /// Close the client.
    fn quit(&self) {
        self.shared.send(Command::Quit);
    }
}

/// The `org.mpris.MediaPlayer2.Player` interface: what is playing.
struct Player {
    shared: Arc<Shared>,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    /// "Playing", "Paused" or "Stopped".
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.shared.snapshot().status.as_str().to_owned()
    }

    /// The current track.
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        self.shared.snapshot().metadata
    }

    /// The playback position, in microseconds.
    #[zbus(property)]
    fn position(&self) -> i64 {
        self.shared.snapshot().position
    }

    /// The volume, between 0 and 1.
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.shared.snapshot().volume
    }

    /// The desktop may change the volume.
    #[zbus(property)]
    fn set_volume(&mut self, volume: f64) {
        self.shared.send(Command::SetVolume(volume));
    }

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.shared.snapshot().shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&mut self, shuffle: bool) {
        self.shared.send(Command::SetShuffle(shuffle));
    }

    #[zbus(property)]
    fn loop_status(&self) -> String {
        self.shared.snapshot().loop_status.as_str().to_owned()
    }

    #[zbus(property)]
    fn set_loop_status(&mut self, status: String) {
        if let Some(status) = LoopStatus::parse(&status) {
            self.shared.send(Command::SetLoopStatus(status));
        }
    }

    /// mpv plays at the normal speed.
    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.shared.snapshot().can_go_next
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.shared.snapshot().can_go_previous
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }

    /// Resume the playback.
    fn play(&self) {
        self.shared.send(Command::Play);
    }

    /// Pause the playback.
    fn pause(&self) {
        self.shared.send(Command::Pause);
    }

    /// Toggle between playing and paused.
    fn play_pause(&self) {
        self.shared.send(Command::PlayPause);
    }

    /// Stop the playback.
    fn stop(&self) {
        self.shared.send(Command::Stop);
    }

    /// Play the next track of the queue.
    fn next(&self) {
        self.shared.send(Command::Next);
    }

    /// Play the previous track of the queue.
    fn previous(&self) {
        self.shared.send(Command::Previous);
    }

    /// Jump forward (or backward, with a negative offset), in microseconds.
    fn seek(&self, offset: i64) {
        self.shared.send(Command::Seek(offset));
    }

    /// Jump to an absolute position, in microseconds.
    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
        // The desktop names the track it believes is playing; a mismatch means
        // the request is stale and seeking would jump in the wrong song.
        let current = self.shared.snapshot();
        if !current.track_id.is_empty() && current.track_id != track_id.as_str() {
            tracing::debug!("mpris: ignoring SetPosition for {track_id}");
            return;
        }
        self.shared.send(Command::SetPosition(position));
    }

    /// Playing an arbitrary URI is not supported.
    fn open_uri(&self, uri: String) {
        tracing::debug!("mpris: OpenUri is not supported ({uri})");
    }
}
