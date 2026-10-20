//! Workarounds for embedding media-heavy web UIs in Tauri's system WebView.
//!
//! On Linux, Tauri uses WebKitGTK (not Chromium/Electron). With NVIDIA GPUs
//! under Wayland the explicit-sync handshake often breaks; MediaSession is also
//! bridged to MPRIS, so hardware media keys (e.g. Fn+F7) can pause and mute
//! HTML audio even when the same site behaves normally in a browser.
//!
//! Prefer `__NV_DISABLE_EXPLICIT_SYNC=1` and keep the DMA-BUF renderer enabled.
//! `WEBKIT_DISABLE_DMABUF_RENDERER=1` avoids some crashes but forces a slow
//! shared-memory path and makes Navidrome/Invidious feel laggy.

/// Apply process-wide Linux WebView environment fixes before any WebKit window
/// is created. Existing user overrides are left untouched.
pub fn apply_linux_webview_env() {
    #[cfg(target_os = "linux")]
    {
        if !nvidia_gpu_present() {
            return;
        }
        // Keeps the accelerated DMA-BUF path while avoiding the Wayland
        // explicit-sync protocol error common with NVIDIA + KWin.
        set_env_if_unset("__NV_DISABLE_EXPLICIT_SYNC", "1");
        // If a previous SyncPulse build (or the user) forced the slow DMA-BUF
        // workaround, clear it unless they explicitly opted in via
        // SYNCPULSE_WEBKIT_DISABLE_DMABUF=1.
        match std::env::var("SYNCPULSE_WEBKIT_DISABLE_DMABUF") {
            Ok(value) if is_truthy(&value) => {
                set_env_if_unset("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            }
            _ => {
                // SAFETY: called once from main before other threads start.
                unsafe {
                    std::env::remove_var("WEBKIT_DISABLE_DMABUF_RENDERER");
                }
            }
        }
    }
}

/// Initialization script for the Invidious WebKit service window.
///
/// This JavaScript stand-in complements the native MediaSession feature
/// disablement below. Do not intercept keyboard events: Fn/media shortcuts are
/// owned by firmware/the desktop environment, and the Fn modifier is not
/// reliably exposed to a WebView.
pub fn service_window_init_script() -> &'static str {
    r#"(function () {
  try {
    var session = {
      metadata: null,
      playbackState: "none",
      setActionHandler: function () {},
      setPositionState: function () {}
    };
    Object.defineProperty(navigator, "mediaSession", {
      configurable: true,
      enumerable: true,
      get: function () { return session; }
    });
  } catch (_) {}
})();"#
}

/// WebKitGTK may expose a stale zero volume during Navidrome's pause fade-out.
/// Preserve that restore value so the player does not persist zero as its new
/// volume, and retry setting it after playback resumes. Explicit mute/slider
/// input cancels the pending restore.
pub fn navidrome_paused_volume_workaround_script() -> &'static str {
    r#"(function () {
  if (window.__syncPulsePausedVolumeWorkaround) return;
  window.__syncPulsePausedVolumeWorkaround = true;
  var pending = new WeakMap();
  var descriptor = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, "volume");
  if (!descriptor || !descriptor.get || !descriptor.set || !descriptor.configurable) return;

  function desired(media) {
    return pending.get(media);
  }

  function restoreIfSilent(media) {
    var target = pending.get(media);
    if (target == null || media.paused || descriptor.get.call(media) > 0.01) return;
    descriptor.set.call(media, target);
    // WebKitGTK can apply volume after the setter returns. Check on later turns
    // rather than trusting the immediate getter value.
    [0, 50, 200, 500].forEach(function (delay) {
      setTimeout(function () {
        if (pending.get(media) !== target || media.paused) return;
        if (descriptor.get.call(media) > 0.01) {
          pending.delete(media);
          return;
        }
        descriptor.set.call(media, target);
      }, delay);
    });
  }

  Object.defineProperty(HTMLMediaElement.prototype, "volume", {
    configurable: descriptor.configurable,
    enumerable: descriptor.enumerable,
    get: function () {
      var actual = descriptor.get.call(this);
      var target = pending.get(this);
      // Navidrome reads audio.volume from volumechange events and persists it
      // to app state. Do not let a failed WebKit restore turn that state into
      // zero, which makes the next Play appear muted as well.
      return target != null && actual <= 0.01 ? target : actual;
    },
    set: function (value) {
      descriptor.set.call(this, value);
      var requested = Number(value);
      if (requested > 0 && this.paused) {
        pending.set(this, requested);
      }
    }
  });

  document.addEventListener("click", function (event) {
    var muteControl = event.target && event.target.closest &&
      event.target.closest(".play-sounds .sounds-icon");
    if (!muteControl) return;
    document.querySelectorAll("audio,video").forEach(function (media) {
      pending.delete(media);
    });
  }, true);

  ["pointerdown", "touchstart", "keydown"].forEach(function (eventName) {
    document.addEventListener(eventName, function (event) {
      var slider = event.target && event.target.closest &&
        event.target.closest(".play-sounds .sound-operation");
      if (!slider) return;
      document.querySelectorAll("audio,video").forEach(function (media) {
        pending.delete(media);
      });
    }, true);
  });

  ["play", "playing", "volumechange"].forEach(function (eventName) {
    document.addEventListener(eventName, function (event) {
      var media = event.target;
      if (!(media instanceof HTMLMediaElement) || desired(media) == null) return;
      if (descriptor.get.call(media) > 0.01) {
        pending.delete(media);
        return;
      }
      restoreIfSilent(media);
    }, true);
  });
})();"#
}

/// Disable WebKit's native Media Session / MPRIS bridge for Invidious.
///
/// Navidrome keeps WebKit's normal MediaSession behavior so its player pause
/// and volume controls work like they do in a regular browser. Resolve the
/// WebKit 2.42 feature API dynamically because Tauri also
/// supports older WebKitGTK releases where those symbols are not available.
#[cfg(target_os = "linux")]
pub fn disable_service_window_media_session(webview: &webkit2gtk::WebView) -> Result<(), String> {
    use webkit2gtk::glib::translate::ToGlibPtr;
    use webkit2gtk::WebViewExt;

    type GetAllFeatures = unsafe extern "C" fn() -> *mut libc::c_void;
    type FindFeature =
        unsafe extern "C" fn(*mut libc::c_void, *const libc::c_char) -> *mut libc::c_void;
    type SetFeatureEnabled =
        unsafe extern "C" fn(*mut libc::c_void, *mut libc::c_void, libc::c_int);
    type Unref = unsafe extern "C" fn(*mut libc::c_void);

    unsafe fn symbol<T: Copy>(name: &'static [u8]) -> Option<T> {
        let pointer = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr().cast());
        if pointer.is_null() {
            None
        } else {
            // Function pointers returned by dlsym have the requested ABI and
            // are copied into their corresponding function-pointer type.
            Some(std::mem::transmute_copy(&pointer))
        }
    }

    let get_features: GetAllFeatures = unsafe { symbol(b"webkit_settings_get_all_features\0") }
        .ok_or_else(|| {
            "This WebKitGTK version does not expose the feature API (requires 2.42+)".to_owned()
        })?;
    let find_feature: FindFeature = unsafe { symbol(b"webkit_feature_list_find\0") }
        .ok_or_else(|| "WebKitGTK feature lookup is unavailable".to_owned())?;
    let set_feature_enabled: SetFeatureEnabled =
        unsafe { symbol(b"webkit_settings_set_feature_enabled\0") }
            .ok_or_else(|| "WebKitGTK feature controls are unavailable".to_owned())?;
    let unref_feature_list: Unref = unsafe { symbol(b"webkit_feature_list_unref\0") }
        .ok_or_else(|| "WebKitGTK feature list cleanup is unavailable".to_owned())?;

    let settings = webview
        .settings()
        .ok_or_else(|| "Unable to get WebKit settings for the service window".to_owned())?;
    let settings_ptr: *mut webkit2gtk::ffi::WebKitSettings = settings.to_glib_none().0;
    let features = unsafe { get_features() };
    if features.is_null() {
        return Err("WebKitGTK returned an empty feature list".to_owned());
    }

    let mut disabled = Vec::new();
    for name in ["MediaSession", "RemoteMediaSessionManager"] {
        let name_c = std::ffi::CString::new(name).expect("feature names contain no NUL bytes");
        let feature = unsafe { find_feature(features, name_c.as_ptr()) };
        if !feature.is_null() {
            unsafe { set_feature_enabled(settings_ptr.cast(), feature, 0) };
            disabled.push(name);
        }
    }
    unsafe { unref_feature_list(features) };

    if disabled.is_empty() {
        return Err("WebKitGTK did not provide the MediaSession features".to_owned());
    }
    tracing::info!(
        "Disabled WebKit service-window features: {}",
        disabled.join(", ")
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_env_if_unset(key: &str, value: &str) {
    if std::env::var_os(key).is_none() {
        // SAFETY: called once from main before other threads touch the env.
        unsafe { std::env::set_var(key, value) };
    }
}

#[cfg(target_os = "linux")]
fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(target_os = "linux")]
fn nvidia_gpu_present() -> bool {
    std::path::Path::new("/proc/driver/nvidia/version").is_file()
        || std::path::Path::new("/dev/nvidia0").exists()
}

#[cfg(test)]
mod tests {
    use super::{navidrome_paused_volume_workaround_script, service_window_init_script};

    #[test]
    fn service_init_script_disables_media_session_without_intercepting_keys() {
        let script = service_window_init_script();
        assert!(script.contains("mediaSession"));
        assert!(!script.contains("addEventListener(\"keydown\""));
        assert!(!script.contains("addEventListener(\"keyup\""));
        assert!(!script.contains("preventDefault"));
        assert!(!script.contains("F7"));
    }

    #[test]
    fn navidrome_volume_workaround_preserves_restore_and_respects_manual_mute() {
        let script = navidrome_paused_volume_workaround_script();
        assert!(script.contains("pending.set(this, requested)"));
        assert!(script.contains("restoreIfSilent"));
        assert!(script.contains(".play-sounds .sounds-icon"));
        assert!(script.contains("pending.delete(media)"));
    }
}
