#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
binary=${1:-"$project_root/target/debug/navidrome-companion"}
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}

if [ ! -x "$binary" ]; then
  printf 'SyncPulse executable not found or not executable: %s\n' "$binary" >&2
  printf 'Build first with: cargo build\n' >&2
  exit 1
fi

binary_dir=$(CDPATH= cd -- "$(dirname -- "$binary")" && pwd)
binary="$binary_dir/$(basename -- "$binary")"
escaped_binary=$(printf '%s' "$binary" | sed 's/\\/\\\\/g; s/"/\\"/g')
applications="$data_home/applications"

install -Dm644 "$project_root/icons/syncpulse.png" \
  "$data_home/icons/hicolor/256x256/apps/org.navidromehub.syncpulse.png"
install -Dm644 "$project_root/icons/navidrome.png" \
  "$data_home/icons/hicolor/256x256/apps/syncpulse-navidrome-v2.png"
install -Dm644 "$project_root/icons/invidious.png" \
  "$data_home/icons/hicolor/256x256/apps/syncpulse-invidious-v2.png"
mkdir -p "$applications"

printf '[Desktop Entry]\nType=Application\nName=SyncPulse\nComment=Install and manage self-hosted open-source services\nExec="%s"\nIcon=org.navidromehub.syncpulse\nTerminal=false\nCategories=Utility;\nStartupWMClass=org.navidromehub.syncpulse\nStartupNotify=true\n' \
  "$escaped_binary" > "$applications/org.navidromehub.syncpulse.desktop"
printf '[Desktop Entry]\nType=Application\nName=Navidrome\nComment=Open the local Navidrome music server\nExec="%s" --syncpulse-service navidrome 4533\nIcon=syncpulse-navidrome-v2\nTerminal=false\nCategories=AudioVideo;Audio;Player;\nStartupWMClass=org.navidromehub.syncpulse.navidrome\nStartupNotify=true\n' \
  "$escaped_binary" > "$applications/org.navidromehub.syncpulse.navidrome.desktop"
printf '[Desktop Entry]\nType=Application\nName=Invidious\nComment=Open the local Invidious video service\nExec="%s" --syncpulse-service invidious 3000\nIcon=syncpulse-invidious-v2\nTerminal=false\nCategories=AudioVideo;Video;Player;\nStartupWMClass=org.navidromehub.syncpulse.invidious\nStartupNotify=true\n' \
  "$escaped_binary" > "$applications/org.navidromehub.syncpulse.invidious.desktop"

if command -v desktop-file-validate >/dev/null 2>&1; then
  desktop-file-validate \
    "$applications/org.navidromehub.syncpulse.desktop" \
    "$applications/org.navidromehub.syncpulse.navidrome.desktop" \
    "$applications/org.navidromehub.syncpulse.invidious.desktop"
fi
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$applications"
fi
if command -v kbuildsycoca6 >/dev/null 2>&1; then
  kbuildsycoca6 --noincremental
elif command -v kbuildsycoca5 >/dev/null 2>&1; then
  kbuildsycoca5 --noincremental
fi

printf 'Installed desktop IDs and icon names:\n'
printf '  org.navidromehub.syncpulse -> %s\n' "$data_home/icons/hicolor/256x256/apps/org.navidromehub.syncpulse.png"
printf '  org.navidromehub.syncpulse.navidrome -> %s\n' "$data_home/icons/hicolor/256x256/apps/syncpulse-navidrome-v2.png"
printf '  org.navidromehub.syncpulse.invidious -> %s\n' "$data_home/icons/hicolor/256x256/apps/syncpulse-invidious-v2.png"
printf 'KDE cache refreshed when its cache utility was available. Restart the app or re-pin it if its taskbar entry was created before registration.\n'
