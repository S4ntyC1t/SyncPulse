# SyncPulse

> **Pre-release:** this project is under active development. The Linux release
> is provided for evaluation and has not yet been validated in clean Fedora,
> Bazzite, or Ubuntu virtual machines. Review the setup plan before approving
> any system package installation.

## Try the Linux release (x86-64)

On Linux with GTK 3 and WebKitGTK 4.1 runtime libraries installed, download,
verify, and launch the latest release with:

```sh
mkdir -p "$HOME/.local/opt/syncpulse" && cd "$HOME/.local/opt/syncpulse" && \
curl -fLO https://github.com/S4ntyC1t/SyncPulse/releases/download/v0.1.0-pre.2/syncpulse-linux-x86_64.tar.gz && \
curl -fLO https://github.com/S4ntyC1t/SyncPulse/releases/download/v0.1.0-pre.2/syncpulse-linux-x86_64.tar.gz.sha256 && \
sha256sum -c syncpulse-linux-x86_64.tar.gz.sha256 && \
tar -xzf syncpulse-linux-x86_64.tar.gz && ./syncpulse
```

The archive contains a release-mode Linux x86-64 executable, not a universal
static binary. It uses the host's GTK/WebKitGTK libraries and will not run on
macOS or Windows. No system package changes are made just by launching SyncPulse;
the first-run wizard can separately offer system dependencies, and authorization
must be approved in the desktop's Polkit dialog.

To build from source, see [Build and test](#build-and-test).

SyncPulse is a local desktop manager for installing and operating open-source
services. It currently manages Navidrome and a Docker Compose or Podman Compose
deployment of Invidious; network, archive, and process operations run outside the UI thread.
The desktop interface is written in HTML, CSS, and JavaScript and runs in
Tauri's native system WebView (WebKitGTK on Linux, WebView2 on Windows). It does
not bundle Electron or Chromium.

## Current features

- Install the latest Navidrome release for Linux or Windows on x86-64 or ARM64.
  SyncPulse runs the binary's version check, starts the service on a fresh
  install, and waits for Navidrome's `/ping` endpoint before reporting success.
- Generate a Navidrome configuration using the configured music/data folders
  and local port; start and stop only the Navidrome binary installed in the
  configured binary folder.
- Check Navidrome through its `/ping` endpoint and Invidious through its
  documented `/api/v1/stats` identity. Navidrome's status distinguishes a
  responding external instance from a SyncPulse-managed binary/process; an
  installation elsewhere is not reported as missing without that qualification.
- Deploy Invidious with the official container images, PostgreSQL, and the
  required companion service using Docker Compose or Podman Compose. Deployment
  secrets are generated locally and the web port is bound to localhost.
- Edit and validate the actual Invidious Compose YAML in Settings. The file
  includes the app's `INVIDIOUS_CONFIG` YAML block and generated secrets; it is
  stored with user-only permissions. The selected engine's Compose provider
  validates the file before you start it. Common domain, reverse-proxy,
  HTTPS-only, registration, and public-statistics options are also available as
  described form controls that update only their corresponding YAML keys.
- Optionally run Navidrome during configured daily local-time hours. The app
  must remain open for the schedule to be enforced; equal start and stop times
  keep the service stopped.
- Download tracks or playlists with yt-dlp into the Navidrome music folder,
  embedding available audio metadata and artwork, tagging the YouTube channel as
  artist, keeping real playlists grouped by playlist name, assigning standalone
  tracks their own album title, avoiding filename overwrites, and requesting a
  library rescan. SyncPulse installs its own yt-dlp and FFmpeg on
  first use, so no system installation is required.
- Configure the SyncPulse-adapted Discord Rich Presence integration using the
  upstream-supported environment variables. Navidrome credentials are stored
  in a local user-only settings file and are passed to the child process as
  environment variables, not placed in URLs or the obsolete unsupported TOML
  format. Arbitrary GitHub repositories are not treated as compatible
  extensions or executed.
- Open reachable Navidrome and Invidious in separate native system-WebView
  windows, or choose to open them in the system browser. Each service window
  runs as a separate lightweight process with its own Linux/Wayland application
  ID, so KDE can match it to its own desktop entry and icon.
- Use separate SyncPulse, Navidrome, and Invidious icons. Linux packages include
  matching desktop entries and hicolor icons for the launcher and taskbar.
- On a fresh profile with no existing services, a Welcome to SyncPulse wizard
  offers quick or advanced setup, optional Navidrome/Invidious/Discord RPC
  installation, folder selection, and optional Tailscale/Cloudflare artwork
  tunnel guidance. Tunnel setup is always explicit: SyncPulse does not expose
  local ports or create public tunnels automatically. Existing configuration
  files are migrated as already set up, so updates do not interrupt existing
   users. The First setup page remains available afterward.

On supported CachyOS/Arch, Debian/Ubuntu, and Fedora systems, the setup wizard
checks for package managers and Polkit and can show a concrete rootless Podman
package plan before requesting authorization in the desktop's system dialog.
SyncPulse never receives or stores the system password and does not add users to
the Docker group. Tailscale installation is a separate opt-in and installs the
client when missing, then enables and starts its daemon; signing in remains an
explicit user step. Starting the daemon does not enable Funnel.

Phone access is separately configured as **This computer only** (default),
**Home Wi-Fi / LAN**, or **Private Tailscale network**. LAN mode binds
SyncPulse-managed Navidrome and Invidious to all interfaces; use only on a
trusted network and account for firewall rules. Tailscale mode binds them only
to the machine's private Tailscale IP and requires the device to be signed in.
Neither mode enables Tailscale Funnel, Cloudflare Tunnel, or public access.
Changing the mode updates managed config/Compose files; SyncPulse refuses to
change a running managed service and never stops it implicitly. Stop the service
explicitly, save the new mode, and start it again to apply the binding.
User-managed deployments are not modified.

Invidious installation requires either Docker Engine with Compose v2, or
Podman with a working Compose provider. Select the engine in Settings before
installing; changing engines does not migrate containers, volumes, or database
data between runtimes. It
downloads the official Invidious source archive for the database initialization
files and pulls the upstream containers; the initial deployment can require
substantial disk space and bandwidth. Stopping Invidious keeps its database
volume. SyncPulse does not currently remove deployments or perform explicit
version pinning/upgrades.

## Discord Rich Presence and cover art

The adapted upstream integration requires a Navidrome username and password.
Set them in Settings, save the integration settings, enable Discord Rich
Presence, and use **Apply & restart integration**. The credentials are written
to `navidrome-discord-rpc.toml` with mode `0600` on Linux. The upstream program
receives its supported `NAVIDROME_*`, `COVER_MODE`, `POLL_INTERVAL`, and
optional `DISCORD_CLIENT_ID` variables at launch.

While SyncPulse is open, its background supervisor starts the integration
independently of Discord or Navidrome windows, restarts it if it exits, and
restarts the bridge when Discord's IPC socket is recreated. Closing Discord or
Navidrome does not stop the SyncPulse-owned RPC process; it exits when SyncPulse
exits. Do not run another copy of the RPC executable or enable its separate
systemd service at the same time: SyncPulse will report a conflict rather than
kill a process it does not own.

Album art defaults to **Off**. Discord fetches images from its own servers and
cannot reach localhost:

- **Navidrome** mode is only for a Navidrome server that is already publicly
  reachable. Set its public HTTPS origin.
- **Private cover server** mode starts upstream `serve-covers`, which binds
  only to loopback and publishes artwork rather than Navidrome's login/API.
  Configure a public HTTPS origin, then expose that local port using Tailscale
  Funnel or a reverse proxy. In Settings, the **Enable Funnel & fill URL**
  button can configure Tailscale Funnel after an explicit confirmation.
  It publishes the entire selected HTTPS port, so review existing Funnel/Serve
  routes before proceeding. Cloudflare Tunnel still requires configuring the
  tunnel and public hostname in Cloudflare.

For Tailscale Funnel, Settings offers a one-click setup and shows the equivalent
command. The button explains the exposure and asks for confirmation first. Funnel
publishes the selected port publicly, not just a URL path, and may replace an
existing route on that port. Check your Tailscale access controls before
proceeding. The public
origin entered in SyncPulse must match the HTTPS address Discord can reach.
The **Public HTTPS origin for Discord** field takes only the public base address
(scheme, hostname, and optional port), with no path. For an existing public
Navidrome deployment, use its public HTTPS address. For the private cover server,
use the hostname created/configured for your tunnel: with Funnel, the machine's
`*.ts.net` hostname and selected HTTPS port; with Cloudflare Tunnel, the public
hostname assigned to the tunnel. The tunnel must route to the loopback cover
listener shown in Settings (default `127.0.0.1:4534`). Discord cannot fetch
`localhost` or a Tailscale-only private address.

Navidrome and Invidious maintain their own service accounts. A shared SyncPulse
login cannot make them share an identity without configuring a compatible SSO
provider in both services; SyncPulse does not claim to provide that SSO today.
The Accounts page opens each service's own account settings in a separate
native WebView window by default, with an option to use the system browser.

On KDE and other Linux desktops, SyncPulse registers the SyncPulse, Navidrome,
and Invidious desktop entries and hicolor icons on every launch, so each service
window is matched to its own application ID and shown as a separate taskbar
entry with its own icon, title, and Alt+Tab entry. AppImage launches register
the original AppImage path rather than its temporary mount path. The helper script remains available and
validates the entries where `desktop-file-validate` is present:

```sh
sh packaging/linux/install-desktop-integration.sh
```

## Build and test

```sh
cargo run
cargo test
```

On Linux, building requires GTK 3 and WebKitGTK 4.1 development packages.
The `web/` directory contains the complete frontend; Rust commands provide
the native service-management bridge.

SyncPulse runs natively on Wayland through GTK/WebKitGTK. On NVIDIA GPUs it
sets `__NV_DISABLE_EXPLICIT_SYNC=1` (unless already set) so the accelerated
DMA-BUF renderer can stay enabled. Do **not** set
`WEBKIT_DISABLE_DMABUF_RENDERER=1` for performance: it avoids some crashes but
forces a slow shared-memory path that makes Navidrome/Invidious feel laggy.
Only enable it when WebKit otherwise crashes, via
`SYNCPULSE_WEBKIT_DISABLE_DMABUF=1`.

**Open window** for Navidrome/Invidious always uses SyncPulse's own native
system WebView (WebKitGTK on Linux, WebView2 on Windows), so no bundled or
system Chromium/Electron is required. **Open in browser** always uses the normal
system browser.

WebKitGTK service windows prevent their embedded page from publishing a
native MediaSession integration to MPRIS. They do not intercept keyboard
events: Fn/media shortcuts are handled by the keyboard firmware or desktop
environment, and the Fn modifier is not reliably exposed to WebKit.

If WebKitGTK exits with `Error 71 (Protocol error) dispatching to Wayland
display` even with `__NV_DISABLE_EXPLICIT_SYNC=1`, try X11 compatibility mode:

```sh
GDK_BACKEND=x11 cargo run
```

Forcing `LIBGL_ALWAYS_SOFTWARE=1` can make WebKit-heavy pages lag further; use
it only as a graphics-driver troubleshooting fallback.

Configuration is stored in the platform-specific application config directory.
The Settings page lets you change the service ports, Docker/Podman engine,
installation folders, startup behavior, Navidrome schedule, Invidious Compose
file, and Discord integration. The responsive dashboard shows local service
health and system metrics, with dedicated pages for downloads, extensions,
and service configuration.

SyncPulse writes diagnostic logs to `syncpulse.log` in the same application
configuration directory (the Settings page shows the full path). Download logs
include the destination path, saved file sizes, yt-dlp warnings/errors, and
whether Navidrome accepted an immediate library rescan. When reporting a
missing download, retry one track and share the relevant log lines; do not share
your Navidrome password or other secrets.
