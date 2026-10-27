//! Tracks short-lived helper processes so their children do not linger as
//! zombie (`<defunct>`) entries once they exit.
//!
//! `std::process::Command::spawn` results are normally reaped only when the
//! caller explicitly waits on the child. Helpers such as `xdg-open` or the
//! standalone service windows are spawned "fire and forget", so once they exit
//! nothing collects their status. Registering them here lets a small care
//! thread reap each child exactly once via [`Child::try_wait`], without a
//! global `waitpid(-1)` reaper that would race with code that waits on its own
//! children (for example the Discord RPC supervisor).
//!
//! A child can also be registered under a `&'static str` tag, which lets the
//! caller ask whether the corresponding helper is still running (used to avoid
//! opening a second standalone service window for the same service).

use std::process::Child;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

struct Tracked {
    tag: Option<&'static str>,
    child: Child,
}

fn tracked_children() -> &'static Mutex<Vec<Tracked>> {
    static TRACKED_CHILDREN: OnceLock<Mutex<Vec<Tracked>>> = OnceLock::new();
    TRACKED_CHILDREN.get_or_init(|| Mutex::new(Vec::new()))
}

/// Registers a spawned child for deferred reaping. Exited children recorded
/// earlier are removed lazily on each registration.
pub fn track(child: Child) {
    push(None, child);
}

/// Registers a spawned child under a tag so [`tag_running`] can report whether
/// it is still alive.
pub fn track_tagged(tag: &'static str, child: Child) {
    push(Some(tag), child);
}

fn push(tag: Option<&'static str>, child: Child) {
    if let Ok(mut children) = tracked_children().lock() {
        retain_running(&mut children);
        children.push(Tracked { tag, child });
    }
}

/// Returns `true` while a child registered under `tag` is still running.
pub fn tag_running(tag: &str) -> bool {
    let Ok(mut children) = tracked_children().lock() else {
        return false;
    };
    retain_running(&mut children);
    children.iter().any(|tracked| tracked.tag == Some(tag))
}

/// Starts the singleton reaper thread. Safe to call more than once.
pub fn start_reaper() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("child-reaper".to_owned())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            if let Ok(mut children) = tracked_children().lock() {
                retain_running(&mut children);
            }
        });
}

fn retain_running(children: &mut Vec<Tracked>) {
    children.retain_mut(|tracked| tracked.child.try_wait().ok().flatten().is_none());
}
