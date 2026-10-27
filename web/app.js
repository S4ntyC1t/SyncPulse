const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const titles = {
  onboarding: "First setup",
  overview: "Overview",
  services: "Services",
  downloader: "Downloader",
  plugins: "Extensions",
  navidrome: "Navidrome",
  invidious: "Invidious",
  accounts: "Accounts",
  settings: "Settings",
};

let currentDashboard = null;
let activePage = "overview";
let busy = false;
let settingsDirty = false;
let rpcSettingsDirty = false;
let rpcSettingsLoaded = false;
let composeLoaded = false;
let composeDirty = false;
let invidiousSettingsDirty = false;
let initialSetupChecked = false;

const byId = (id) => document.getElementById(id);
const escapeText = (text) => String(text).replace(/[&<>"']/g, (char) => ({
  "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
}[char]));

function toast(message, isError = false) {
  const item = document.createElement("div");
  item.className = `toast${isError ? " error" : ""}`;
  item.textContent = message;
  byId("toast-region").append(item);
  window.setTimeout(() => item.remove(), 4200);
}

function setPage(name) {
  if (!titles[name]) return;
  activePage = name;
  document.querySelectorAll(".page").forEach((page) => {
    page.classList.toggle("active", page.id === `page-${name}`);
  });
  document.querySelectorAll(".nav-item").forEach((item) => {
    item.classList.toggle("active", item.dataset.page === name);
  });
  byId("page-title").textContent = titles[name];
  if (name === "settings" && currentDashboard) {
    populateSettings(currentDashboard);
    void loadSettingsDetails();
  }
  window.scrollTo({ top: 0, behavior: "smooth" });
}

function setBadge(id, online, pending) {
  const badge = byId(id);
  if (!badge) return;
  const state = pending ? "checking" : online ? "online" : "offline";
  const label = pending ? "Checking" : online ? "Online" : "Offline";
  badge.className = `status-badge ${pending ? "neutral" : online ? "online" : "offline"}`;
  badge.innerHTML = `<i></i>${label}`;
  badge.dataset.state = state;
}

function setText(id, value) {
  const element = byId(id);
  if (element) element.textContent = value;
}

function applyDashboard(data) {
  const wasInvidiousDeployment = currentDashboard?.invidiousComposePath;
  currentDashboard = data;
  if (wasInvidiousDeployment !== data.invidiousComposePath) composeLoaded = false;
  const nav = data.navidrome;
  const navState = nav.reachable;
  ["navidrome-badge", "navidrome-badge-page", "navidrome-badge-detail"]
    .forEach((id) => setBadge(id, navState, false));
  ["navidrome-url", "navidrome-url-page", "navidrome-url-detail"].forEach((id) => {
    const element = byId(id);
    if (element) {
      const navHost = data.navidromeAccessHost || null;
      const text = navHost
        ? `http://${navHost}:${data.config.navidrome_port}`
        : (data.config.network_access === "lan" ? "LAN address unavailable" : "Tailscale disconnected");
      if (element.tagName === "A") {
        if (navHost) element.href = text;
        else element.removeAttribute("href");
        if (navHost) element.dataset.action = "open-navidrome";
        else delete element.dataset.action;
      }
      element.textContent = text;
    }
  });
  const navStatus = `${nav.status}${nav.pid ? ` · PID ${nav.pid}` : ""} — ${nav.detail}`;
  setText("navidrome-note", navStatus);
  setText("navidrome-note-page", navStatus);
  const installStatus = nav.managed_binary ? "Installed" : "Not installed here";
  setText("navidrome-install", installStatus);
  setText("navidrome-install-page", installStatus);
  ["navidrome-primary", "navidrome-primary-page"].forEach((id) => {
    const button = byId(id);
    if (!button) return;
    if (nav.reachable && !nav.managed_process) {
      button.innerHTML = "Already responding <span>✓</span>";
      button.disabled = true;
      button.dataset.mode = "external";
    } else if (nav.managed_process) {
      button.innerHTML = "Stop service <span>■</span>";
      button.disabled = false;
      button.dataset.mode = "stop";
    } else if (nav.managed_binary) {
      button.innerHTML = "Start service <span>→</span>";
      button.disabled = false;
      button.dataset.mode = "start";
    } else {
      button.innerHTML = "Install service <span>↓</span>";
      button.disabled = Boolean(nav.reachable);
      button.dataset.mode = "install";
    }
  });

  ["invidious-badge", "invidious-badge-page", "invidious-badge-detail"]
    .forEach((id) => setBadge(id, data.invidiousReachable, false));
  ["invidious-url", "invidious-url-page", "invidious-url-detail"].forEach((id) => {
    const element = byId(id);
    if (!element) return;
      const invidiousHost = data.invidiousAccessHost || null;
    const text = invidiousHost
      ? `http://${invidiousHost}:${data.config.invidious_port}`
      : (data.config.network_access === "lan" ? "LAN address unavailable" : "Tailscale disconnected");
    if (element.tagName === "A") {
      if (invidiousHost) element.href = text;
      else element.removeAttribute("href");
      if (invidiousHost) element.dataset.action = "open-invidious";
      else delete element.dataset.action;
    }
    element.textContent = text;
  });
  const ivDeployed = Boolean(data.invidiousComposePath);
  const ivInstall = data.invidiousInstalled
    ? "Compose deployment found"
    : ivDeployed
      ? "Detected external deployment"
      : "Not installed";
  setText("invidious-install", ivInstall);
  setText("invidious-install-page", ivInstall);
  let ivButtonText;
  let ivButtonMode;
  let ivExternal = false;
  if (ivDeployed) {
    ivButtonMode = data.invidiousReachable ? "stop" : "start";
    ivButtonText = data.invidiousReachable ? "Stop service <span>■</span>" : "Start service <span>→</span>";
  } else if (data.invidiousReachable) {
    ivButtonMode = "external";
    ivButtonText = "Already responding <span>✓</span>";
    ivExternal = true;
  } else {
    ivButtonMode = "install";
    ivButtonText = "Set up service <span>↓</span>";
  }
  ["invidious-primary", "invidious-primary-page", "invidious-detail-primary"].forEach((id) => {
    const button = byId(id);
    if (!button) return;
    button.innerHTML = ivButtonText;
    button.dataset.mode = ivButtonMode;
    button.disabled = busy || ivExternal;
  });
  const navSetup = nav.managed_process
    ? "Installed, running, and health-checked."
    : nav.managed_binary
      ? "Binary installed. Start it from Services to run the health check."
      : nav.reachable
        ? "Existing Navidrome detected on the configured port."
        : "Not installed yet.";
  const invidiousSetup = ivDeployed && data.invidiousReachable
    ? "Deployment is running and its API identity is verified."
    : ivDeployed
      ? "Deployment found. Start it from Services to run the API health check."
      : data.invidiousReachable
        ? "Existing Invidious detected through its API."
        : `Not installed yet. ${data.config.container_engine === "podman" ? "Podman and a Compose provider" : "Docker Engine and Compose v2"} are required.`;
  setText("onboarding-navidrome-status", navSetup);
  setText("onboarding-invidious-status", invidiousSetup);
  document.querySelectorAll('#page-onboarding [data-action="navidrome-primary"]').forEach((button) => {
    const detectedExternal = nav.reachable && !nav.managed_process;
    button.dataset.action = detectedExternal ? "open-navidrome" : "navidrome-primary";
    button.dataset.mode = detectedExternal
      ? "external"
      : nav.managed_process ? "stop" : nav.managed_binary ? "start" : "install";
    button.textContent = detectedExternal
      ? "Open detected Navidrome"
      : nav.managed_process ? "Navidrome is ready ✓" : nav.managed_binary ? "Start and verify Navidrome →" : "Install Navidrome ↓";
    button.disabled = busy || (nav.reachable && !nav.managed_process) || nav.managed_process;
  });
  document.querySelectorAll('#page-onboarding [data-action="invidious-primary"]').forEach((button) => {
    const detectedExternal = !ivDeployed && data.invidiousReachable;
    button.dataset.action = detectedExternal ? "open-invidious" : "invidious-primary";
    button.dataset.mode = ivDeployed
      ? data.invidiousReachable ? "stop" : "start"
      : data.invidiousReachable ? "external" : "install";
    button.textContent = ivDeployed
      ? data.invidiousReachable ? "Invidious is ready ✓" : "Start and verify Invidious →"
      : data.invidiousReachable ? "Open detected Invidious" : "Set up Invidious ↓";
    button.disabled = busy || (ivDeployed && data.invidiousReachable);
  });
  setText("online-count", `${Number(nav.reachable) + Number(data.invidiousReachable)}`);
  setText(
    "ytdlp-state",
    data.ffmpegAvailable && data.ytDlpInstalled
      ? "yt-dlp and FFmpeg ready"
      : "yt-dlp and FFmpeg install on first download",
  );
  setText("managed-binary", data.config.bin_dir);
  setText("config-path", data.configPath);
  setText("log-path", data.logPath);
  const schedule = data.config.schedule_enabled
    ? `Daily · ${formatTime(data.config.schedule_start_hour, data.config.schedule_start_minute)} – ${formatTime(data.config.schedule_stop_hour, data.config.schedule_stop_minute)} (local time)`
    : "Schedule is off.";
  setText("schedule-summary", schedule);
  if (!settingsDirty && activePage === "settings") populateSettings(data);
  if (activePage === "settings") void loadSettingsDetails();
  updateNavidromeControls();
  if (!initialSetupChecked) {
    initialSetupChecked = true;
    const noServicesFound = !nav.managed_binary && !ivDeployed && !nav.reachable && !data.invidiousReachable;
    if (data.firstRun && noServicesFound) {
      initializeWelcomeWizard(data.config);
      byId("welcome-wizard").classList.remove("hidden");
    } else if (data.firstRun) {
      void invoke("complete_initial_setup").then(() => refreshDashboard()).catch((error) => toast(String(error), true));
    }
    if (!data.firstRun && noServicesFound) {
      setPage("onboarding");
    }
  }
}

function initializeWelcomeWizard(config) {
  byId("wizard-music-dir").value = config.music_dir;
  byId("wizard-data-dir").value = config.data_dir;
  byId("wizard-bin-dir").value = config.bin_dir;
  byId("wizard-plugins-dir").value = config.plugins_dir;
  byId("wizard-navidrome-port").value = config.navidrome_port;
  byId("wizard-invidious-port").value = config.invidious_port;
  byId("wizard-network-access").value = config.network_access || "local";
  byId("wizard-auto-start").checked = config.auto_start;
  byId("wizard-tunnel").value = config.discord_artwork_tunnel || "none";
  byId("wizard-engine").value = currentDashboard?.systemSetup?.suggestedEngine || "podman";
  byId("wizard-install-rpc").checked = false;
  byId("wizard-artwork-url").value = "";
  byId("wizard-rpc-options").classList.add("hidden");
  byId("wizard-container-options").classList.add("hidden");
  byId("wizard-install-dependencies").checked = true;
  byId("wizard-install-tailscale").checked = false;
  updateWizardTunnelNote();
  updateWizardDependencies();
  updateWizardNetworkNote();
}

function showWizardConfig(advanced) {
  byId("wizard-welcome-step").classList.add("hidden");
  byId("wizard-config-step").classList.remove("hidden");
  byId("wizard-advanced-fields").classList.toggle("hidden", !advanced);
  setText("wizard-mode-label", advanced ? "ADVANCED SETUP" : "QUICK SETUP");
  byId("wizard-error").classList.add("hidden");
  byId("wizard-install-rpc").dispatchEvent(new Event("change"));
}

function updateWizardTunnelNote() {
  const notes = {
    none: "No tunnel will be configured. Discord can still show playback text; remote album art is not available without a public HTTPS address.",
    tailscale: "After setup, sign in to Tailscale. Artwork may use public Funnel only after a separate warning and confirmation; private phone access is a separate choice.",
    cloudflare: "After setup, install cloudflared and create a Cloudflare Tunnel to the artwork listener. SyncPulse does not yet create or start Cloudflare tunnels automatically.",
  };
  setText("wizard-tunnel-note", notes[byId("wizard-tunnel").value] || notes.none);
}

function updateWizardDependencies() {
  if (!currentDashboard) return;
  const system = currentDashboard.systemSetup;
  const engine = byId("wizard-engine").value;
  const invidiousSelected = byId("wizard-install-invidious").checked;
  byId("wizard-container-options").classList.toggle("hidden", !invidiousSelected);
  const ready = engine === "podman"
    ? system.podmanInstalled && system.podmanComposeAvailable
    : system.dockerInstalled && system.composeAvailable;
  const dependencyRow = byId("wizard-install-dependencies-row");
  const plan = byId("wizard-dependency-plan");
  if (ready) {
    dependencyRow.classList.add("hidden");
    plan.classList.add("hidden");
  } else {
    const packages = engine === "podman"
      ? ["podman", "podman-compose"]
      : system.packageManager === "pacman" ? ["docker", "docker-compose"]
        : system.packageManager === "apt-get" ? ["docker.io", "docker-compose-v2"]
          : ["moby-engine", "docker-compose"];
    const installable = system.supported && system.pkexecAvailable && engine === "podman";
    dependencyRow.classList.toggle("hidden", !installable);
    byId("wizard-install-dependencies").disabled = !installable;
    setText("wizard-dependency-plan-copy", installable
      ? `Detected ${system.distribution}. ${system.packageManager === "apt-get" ? "First refresh the apt package lists. Then " : ""}Install ${[...packages, ...(byId("wizard-install-tailscale").checked && !system.tailscaleInstalled && system.packageManager === "pacman" ? ["tailscale"] : [])].join(" and ")} with ${system.packageManager}${byId("wizard-install-tailscale").checked ? ", then enable and start tailscaled.service" : ""}. KDE/Polkit asks for authorization in system dialogs; SyncPulse never receives or stores the password. Podman uses rootless containers.`
    : `Required packages: ${packages.join(" and ")}. ${engine === "docker" ? "SyncPulse will not install/configure Docker automatically because daemon access can grant root-equivalent privileges. Choose Podman or configure Docker yourself." : "Automatic installation needs a supported package manager and pkexec."}`);
    plan.classList.remove("hidden");
  }
  const privateRemote = byId("wizard-network-access").value === "tailscale";
  const wantsArtworkTunnel = byId("wizard-tunnel").value === "tailscale";
  const canInstallTailscale = system.pkexecAvailable
    && ((system.supported && system.packageManager === "pacman") || system.tailscaleInstalled);
  const tailscaleRow = byId("wizard-install-tailscale-row");
  tailscaleRow.classList.toggle("hidden", !(privateRemote || wantsArtworkTunnel) || !canInstallTailscale);
  if (!(privateRemote || wantsArtworkTunnel) || !canInstallTailscale) {
    byId("wizard-install-tailscale").checked = false;
  }
  const tailscalePlan = byId("wizard-tailscale-plan");
  const showTailscalePlan = (privateRemote || wantsArtworkTunnel) && canInstallTailscale;
  tailscalePlan.classList.toggle("hidden", !showTailscalePlan);
  if (showTailscalePlan) {
    setText("wizard-tailscale-plan-copy", `System changes if selected: ${system.tailscaleInstalled ? "enable and start tailscaled.service" : "install the Tailscale package with pacman, then enable and start tailscaled.service"}. This only joins a private tailnet after you sign in; it does not enable Funnel. The package authorization dialog appears through KDE/Polkit.`);
  }
}

function updateWizardNetworkNote() {
  const system = currentDashboard?.systemSetup;
  const lanHost = system?.lanIp || "No LAN IPv4 address detected";
  const notes = {
    local: "Services stay on this computer. Your phone and other devices cannot connect.",
    lan: `Services listen on all network interfaces. Phone URL host: ${lanHost}. Devices on Wi-Fi can connect; use a trusted network and check firewall rules.`,
    tailscale: `Services bind only to the private Tailscale IP. ${system?.tailscaleIp ? `Phone URL host: ${system.tailscaleIp}.` : "Sign in and connect to Tailscale before enabling this mode."} Public Funnel remains off.`,
  };
  setText("wizard-network-note", notes[byId("wizard-network-access").value] || notes.local);
}

function updateSettingsNetworkNote(system = currentDashboard?.systemSetup) {
  if (!system) return;
  const mode = byId("setting-network-access").value;
  const text = mode === "lan"
    ? `LAN selected: managed services bind to all interfaces. Phone URL host: ${system.lanIp || "No LAN IPv4 address detected"}. This may be reachable by every device on the Wi-Fi network; firewall rules still apply.`
    : mode === "tailscale"
      ? `Tailscale selected: managed services bind only to the private Tailscale IP. ${system.tailscaleIp ? `Phone URL host: ${system.tailscaleIp}` : "Connect and sign in to Tailscale before starting services."} Public Funnel remains off.`
      : "Local selected: services are available only on this computer at 127.0.0.1.";
  setText("settings-network-note", text);
}

function dataServiceHost(config) {
  if (config.network_access === "tailscale") {
    return currentDashboard?.systemSetup?.tailscaleIp || "127.0.0.1";
  }
  // Loopback works for both local binding and LAN wildcard binding.
  return "127.0.0.1";
}

async function pickWizardDirectory(inputId) {
  try {
    const selected = await invoke("select_directory", {
      title: `Choose ${inputId === "wizard-music-dir" ? "your music library" : "a service folder"}`,
    });
    if (selected) byId(inputId).value = selected;
  } catch (error) {
    toast(String(error), true);
  }
}

async function runWelcomeSetup() {
  if (busy || !currentDashboard) return;
  const wantsNavidrome = byId("wizard-install-navidrome").checked;
  const wantsInvidious = byId("wizard-install-invidious").checked;
  const wantsRpc = byId("wizard-install-rpc").checked;
  const wantsDependencies = wantsInvidious
    && byId("wizard-engine").value === "podman"
    && byId("wizard-install-dependencies").checked
    && !(currentDashboard.systemSetup.podmanInstalled && currentDashboard.systemSetup.podmanComposeAvailable);
  const wantsTailscaleInstall = byId("wizard-install-tailscale").checked;
  if (wantsTailscaleInstall && !currentDashboard.systemSetup.tailscaleInstalled
    && currentDashboard.systemSetup.packageManager !== "pacman") {
    setText("wizard-error", "This distribution does not have guided Tailscale package installation yet. Install Tailscale from its official distribution-specific instructions, then return here; SyncPulse will not guess repository setup commands.");
    byId("wizard-error").classList.remove("hidden");
    return;
  }
  if (wantsRpc && !wantsNavidrome && !currentDashboard.navidrome.reachable) {
    setText("wizard-error", "Discord Rich Presence requires a reachable Navidrome server. Select Navidrome or set up an existing server first.");
    byId("wizard-error").classList.remove("hidden");
    return;
  }
  if (!wantsNavidrome && !wantsInvidious && !wantsRpc && !wantsTailscaleInstall) {
    setText("wizard-error", "Choose at least one service or integration, or go back and skip setup.");
    byId("wizard-error").classList.remove("hidden");
    return;
  }
  const config = {
    ...currentDashboard.config,
    music_dir: byId("wizard-music-dir").value.trim(),
    bin_dir: byId("wizard-bin-dir").value.trim(),
    discord_artwork_tunnel: byId("wizard-tunnel").value,
    tailscale: byId("wizard-tunnel").value === "tailscale",
    network_access: byId("wizard-network-access").value,
    discord_rpc_enabled: false,
  };
  if (config.network_access !== "tailscale" && config.discord_artwork_tunnel !== "tailscale") {
    byId("wizard-install-tailscale").checked = false;
  }
  const tailscaleMustConnect = byId("wizard-network-access").value === "tailscale";
  if (byId("wizard-advanced-fields").classList.contains("hidden") === false) {
    config.navidrome_port = Number(byId("wizard-navidrome-port").value);
    config.invidious_port = Number(byId("wizard-invidious-port").value);
    config.data_dir = byId("wizard-data-dir").value.trim();
    config.bin_dir = byId("wizard-bin-dir").value.trim();
    config.plugins_dir = byId("wizard-plugins-dir").value.trim();
    config.auto_start = byId("wizard-auto-start").checked;
  }
  if (wantsInvidious) config.container_engine = byId("wizard-engine").value;
  if (tailscaleMustConnect && !currentDashboard.systemSetup.tailscaleConnected && !wantsTailscaleInstall) {
    setText("wizard-error", "Tailscale must be signed in and connected before services can bind to its private IP. Choose local/LAN now, then switch to Tailscale in Settings after signing in.");
    byId("wizard-error").classList.remove("hidden");
    return;
  }
  if (config.discord_artwork_tunnel === "tailscale"
    && !currentDashboard.systemSetup.tailscaleInstalled && !wantsTailscaleInstall) {
    setText("wizard-error", "Tailscale is missing. Select package installation or choose another artwork tunnel.");
    byId("wizard-error").classList.remove("hidden");
    return;
  }
  const engineReady = config.container_engine === "podman"
    ? currentDashboard.systemSetup.podmanInstalled && currentDashboard.systemSetup.podmanComposeAvailable
    : currentDashboard.systemSetup.dockerInstalled && currentDashboard.systemSetup.composeAvailable;
  if (wantsInvidious && !engineReady && !wantsDependencies) {
    setText("wizard-error", "The selected container engine is not ready. Select the dependency installation option or install and configure the engine yourself before continuing.");
    byId("wizard-error").classList.remove("hidden");
    return;
  }
  if (!config.music_dir || !config.bin_dir || (config.navidrome_port < 1 || config.navidrome_port > 65535)
    || (config.invidious_port < 1 || config.invidious_port > 65535)) {
    setText("wizard-error", "Enter music and binary folders, and valid service ports (1–65535).");
    byId("wizard-error").classList.remove("hidden");
    return;
  }

  busy = true;
  updateNavidromeControls();
  byId("wizard-config-step").classList.add("hidden");
  byId("wizard-progress-step").classList.remove("hidden");
  byId("wizard-error").classList.add("hidden");
  byId("wizard-progress-error").classList.add("hidden");
  byId("wizard-progress-close").classList.add("hidden");
  const tasks = [
    ...(wantsDependencies || wantsTailscaleInstall ? [async () => {
      setWizardProgress("Waiting for system authorization and installing selected packages...", 10);
      await invoke("install_system_dependencies", {
        engine: wantsDependencies ? "podman" : "none",
        includeTailscale: wantsTailscaleInstall,
      });
    }] : []),
    ...(wantsTailscaleInstall && config.network_access === "tailscale" ? [async () => {
      setWizardProgress("Waiting for your Tailscale sign-in...", 62);
      const deadline = Date.now() + 120000;
      while (Date.now() < deadline) {
        const refreshed = await invoke("dashboard");
        currentDashboard = refreshed;
        if (refreshed.systemSetup.tailscaleConnected) return;
        await new Promise((resolve) => window.setTimeout(resolve, 2500));
      }
      throw new Error("Tailscale was installed and tailscaled started, but this setup could not detect a completed sign-in. Sign in using Tailscale, then select Tailscale service access in Settings.");
    }] : []),
    async () => { setWizardProgress("Saving your choices...", 68); await invoke("save_settings", { config }); },
    ...(wantsRpc ? [async () => {
      setWizardProgress("Preparing Discord artwork settings…", 7);
      const rpcConfig = await invoke("get_discord_rpc_config");
      rpcConfig.navidromeUrl = `http://${dataServiceHost(config)}:${config.navidrome_port}`;
      rpcConfig.navidromePublicUrl = byId("wizard-artwork-url").value.trim();
      rpcConfig.coversListen = "127.0.0.1:4534";
      rpcConfig.coverMode = config.discord_artwork_tunnel !== "none" && rpcConfig.navidromePublicUrl
        ? "server"
        : "off";
      await invoke("save_discord_rpc_settings", { rpcConfig });
    }] : []),
    ...(wantsNavidrome ? [async () => { setWizardProgress("Installing Navidrome…", 8); await invoke("install_navidrome"); }] : []),
    ...(wantsInvidious ? [async () => { setWizardProgress("Setting up Invidious containers…", 8); await invoke("operate_invidious", { action: "install" }); }] : []),
    ...(wantsRpc ? [async () => { setWizardProgress("Installing Discord Rich Presence…", 8); await invoke("install_discord_rpc"); }] : []),
  ];
  try {
    for (const task of tasks) await task();
    setWizardProgress("Finishing first-run setup…", 98);
    await invoke("complete_initial_setup");
    setWizardProgress("Setup complete", 100);
    setText("wizard-progress-title", "You’re all set.");
    setText("wizard-progress-copy", "Selected items are ready. Discord Rich Presence still needs your Navidrome login in Settings before it can be enabled.");
    byId("wizard-progress-close").classList.remove("hidden");
    await refreshDashboard();
  } catch (error) {
    setText("wizard-progress-title", "Setup needs attention.");
    setText("wizard-progress-copy", "The setup stopped on an error. You can continue now and install or retry services from the Services and Extensions pages.");
    setText("wizard-progress-error", String(error));
    byId("wizard-progress-error").classList.remove("hidden");
    setText("wizard-progress-close", "Continue to SyncPulse →");
    byId("wizard-progress-close").classList.remove("hidden");
  } finally {
    busy = false;
    updateNavidromeControls();
  }
}

function setWizardProgress(message, percent) {
  setText("wizard-progress-message", message);
  setText("wizard-progress-percent", `${Math.round(percent)}%`);
  byId("wizard-progress-bar").style.width = `${percent}%`;
}

function updateNavidromeControls() {
  if (!currentDashboard) return;
  const nav = currentDashboard.navidrome;
  document.querySelectorAll('[data-action="navidrome-primary"]').forEach((button) => {
    button.disabled = busy || (nav.reachable && !nav.managed_process);
  });
  document.querySelectorAll('[data-action="invidious-primary"]').forEach((button) => {
    if (!button.closest("#page-onboarding")) button.disabled = busy;
  });
  byId("download-button").disabled = busy;
  byId("install-discord").disabled = busy || currentDashboard.discordRpcInstalled;
}

byId("wizard-quick-choice").addEventListener("click", () => showWizardConfig(false));
byId("wizard-advanced-choice").addEventListener("click", () => showWizardConfig(true));
byId("wizard-back").addEventListener("click", () => {
  byId("wizard-config-step").classList.add("hidden");
  byId("wizard-welcome-step").classList.remove("hidden");
});
  byId("wizard-skip").addEventListener("click", async () => {
    try {
      await invoke("complete_initial_setup");
      byId("welcome-wizard").classList.add("hidden");
      setPage("overview");
      await refreshDashboard();
    } catch (error) { toast(String(error), true); }
  });
byId("wizard-install-rpc").addEventListener("change", (event) => {
  byId("wizard-rpc-options").classList.toggle("hidden", !event.target.checked);
});
byId("wizard-install-invidious").addEventListener("change", updateWizardDependencies);
byId("wizard-engine").addEventListener("change", updateWizardDependencies);
byId("wizard-install-dependencies").addEventListener("change", updateWizardDependencies);
byId("wizard-install-tailscale").addEventListener("change", updateWizardDependencies);
byId("wizard-network-access").addEventListener("change", () => {
  updateWizardNetworkNote();
  updateWizardDependencies();
});
byId("wizard-tunnel").addEventListener("change", () => {
  updateWizardTunnelNote();
  updateWizardDependencies();
});
byId("wizard-finish").addEventListener("click", runWelcomeSetup);
byId("wizard-progress-close").addEventListener("click", () => {
  byId("welcome-wizard").classList.add("hidden");
  setPage("overview");
  void refreshDashboard();
});
document.querySelectorAll("[data-pick-directory]").forEach((button) => {
  button.addEventListener("click", () => pickWizardDirectory(button.dataset.pickDirectory));
});

function formatTime(hour, minute) {
  return `${String(hour).padStart(2, "0")}:${String(minute).padStart(2, "0")}`;
}

function setTimeValue(id, hour, minute) {
  byId(id).value = formatTime(hour, minute);
}

function readTimeValue(id) {
  const [hour, minute] = byId(id).value.split(":").map(Number);
  return { hour: hour || 0, minute: minute || 0 };
}

function populateSettings(data) {
  const config = data.config;
  byId("setting-navidrome-port").value = config.navidrome_port;
  byId("setting-invidious-port").value = config.invidious_port;
  byId("setting-container-engine").value = config.container_engine || "docker";
  byId("setting-network-access").value = config.network_access || "local";
  updateSettingsNetworkNote(data.systemSetup);
  byId("rpc-navidrome-url").value = `http://${dataServiceHost(config)}:${config.navidrome_port}`;
  byId("setting-music").value = config.music_dir;
  byId("setting-data").value = config.data_dir;
  byId("setting-bin").value = config.bin_dir;
  byId("setting-plugins").value = config.plugins_dir;
  byId("setting-artwork-tunnel").value = config.discord_artwork_tunnel || "none";
  byId("setting-navidrome-user").value = config.navidrome_username ?? "";
  byId("setting-navidrome-password").value = config.navidrome_password ?? "";
  byId("setting-autostart").checked = config.auto_start;
  byId("setting-schedule").checked = config.schedule_enabled;
  byId("setting-discord").checked = config.discord_rpc_enabled;
  setTimeValue("setting-start-time", config.schedule_start_hour, config.schedule_start_minute);
  setTimeValue("setting-stop-time", config.schedule_stop_hour, config.schedule_stop_minute);
  byId("schedule-fields").classList.toggle("visible", config.schedule_enabled);
  settingsDirty = false;
}

async function loadSettingsDetails() {
  if (!currentDashboard || activePage !== "settings") return;
  const rpcConflict = currentDashboard.discordRpcConflict;
  setText(
    "rpc-conflict-message",
    rpcConflict
      ? `${rpcConflict} Close that process once; after that SyncPulse will own and supervise its RPC.`
      : "",
  );
  byId("rpc-conflict-message").classList.toggle("error", Boolean(rpcConflict));
  if (!rpcSettingsLoaded) {
    try {
      const settings = await invoke("get_discord_rpc_config");
      if (!rpcSettingsDirty) {
        byId("rpc-navidrome-url").value = settings.navidromeUrl;
        byId("rpc-navidrome-user").value = settings.navidromeUser;
        byId("rpc-navidrome-password").value = settings.navidromePassword;
        byId("rpc-cover-mode").value = settings.coverMode;
        byId("rpc-public-url").value = settings.navidromePublicUrl;
        byId("rpc-client-id").value = settings.discordClientId;
        byId("rpc-poll-interval").value = settings.pollInterval;
        byId("rpc-covers-listen").value = settings.coversListen;
        updateCoverHelp();
      }
      rpcSettingsLoaded = true;
    } catch (error) {
      setText("rpc-settings-message", String(error));
    }
  }
  if (!composeLoaded && currentDashboard.invidiousComposePath) {
    try {
      const file = await invoke("get_invidious_compose");
      setText("invidious-compose-path", file.path);
      if (!composeDirty && !invidiousSettingsDirty) {
        byId("invidious-compose-editor").value = file.contents;
        byId("invidious-domain").value = file.appSettings.domain;
        byId("invidious-external-port").value = file.appSettings.externalPort ?? "";
        byId("invidious-https-only").checked = file.appSettings.httpsOnly;
        byId("invidious-registration-enabled").checked = file.appSettings.registrationEnabled;
        byId("invidious-statistics-enabled").checked = file.appSettings.statisticsEnabled;
      }
      const appSettingsPanel = byId("invidious-app-settings");
      appSettingsPanel.classList.toggle("unsupported", !file.appConfigSupported);
      appSettingsPanel.querySelectorAll("input").forEach((input) => {
        input.disabled = !file.appConfigSupported;
      });
      setText(
        "invidious-config-support-note",
        file.appConfigSupported
          ? "Saving updates only these known YAML keys and preserves the rest of the file."
          : "No literal INVIDIOUS_CONFIG block was found; edit this deployment's app configuration directly in Compose.",
      );
      byId("invidious-compose-editor").disabled = false;
      composeLoaded = true;
    } catch (error) {
      setText("invidious-compose-message", String(error));
    }
  } else if (!currentDashboard.invidiousComposePath) {
    setText("invidious-compose-path", "Deploy Invidious to create or detect this file.");
    byId("invidious-compose-editor").disabled = true;
    byId("invidious-app-settings").classList.add("unsupported");
    byId("invidious-app-settings").querySelectorAll("input").forEach((input) => {
      input.disabled = true;
    });
  }
  byId("restart-rpc").disabled = !currentDashboard.discordRpcInstalled
    || !byId("setting-discord").checked;
}

function updateCoverHelp() {
  const mode = byId("rpc-cover-mode").value;
  const descriptions = {
    off: "Album art is off by default. Discord cannot fetch images from localhost.",
    navidrome: "Use this only if Navidrome itself is already reachable from the public internet. Set its public HTTPS origin below.",
    server: "SyncPulse runs the upstream serve-covers process on loopback. Publish only that local port through a public HTTPS tunnel or reverse proxy.",
  };
  setText("rpc-cover-help", descriptions[mode]);
  document.querySelector(".funnel-row").classList.toggle("hidden", mode !== "server");
  const httpsPort = Number(byId("rpc-funnel-port").value) || 8443;
  const listen = byId("rpc-covers-listen").value.trim() || "127.0.0.1:4534";
  setText("rpc-funnel-command", `tailscale funnel --bg --https=${httpsPort} http://${listen}`);
  byId("funnel-one-click").classList.toggle(
    "hidden",
    mode !== "server" || byId("setting-artwork-tunnel").value !== "tailscale",
  );
  if (mode === "server" && !byId("rpc-public-url").value.trim()) {
    byId("rpc-public-url").placeholder = `https://<your-node>.<your-tailnet>.ts.net:${httpsPort}`;
  }
}

async function refreshDashboard() {
  // Skip polling while the window is hidden/minimized: there is nothing to
  // update and the backend probes (HTTP checks, process and system scans)
  // would keep the CPU and allocator warm for no reason.
  if (document.hidden) return;
  try {
    const [data, metrics] = await Promise.all([invoke("dashboard"), invoke("system_metrics")]);
    applyDashboard(data);
    applyMetrics(metrics);
    byId("local-time").textContent = new Intl.DateTimeFormat(undefined, {
      weekday: "short", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
    }).format(new Date());
    setText("navidrome-last-check", `Checked ${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}`);
  } catch (error) {
    toast(String(error), true);
  }
}

function applyMetrics(metrics) {
  const cpu = Math.max(0, Math.min(100, metrics.cpuPercent));
  const memory = metrics.totalMemoryMb ? metrics.usedMemoryMb / metrics.totalMemoryMb * 100 : 0;
  const memoryPercent = Math.max(0, Math.min(100, memory));
  const cpuText = `${cpu.toFixed(1)}%`;
  const memoryText = `${metrics.usedMemoryMb.toLocaleString()} MB`;
  setText("cpu-usage", cpuText);
  setText("cpu-inline", cpuText);
  setText("memory-usage", memoryText);
  setText("memory-inline", `${metrics.usedMemoryMb.toLocaleString()} / ${metrics.totalMemoryMb.toLocaleString()} MB`);
  byId("cpu-bar").style.width = `${cpu}%`;
  byId("memory-bar").style.width = `${memoryPercent}%`;
}

async function withBusy(label, operation) {
  if (busy) return;
  busy = true;
  updateNavidromeControls();
  const progress = byId("global-progress");
  progress.classList.remove("hidden");
  setText("global-progress-message", label);
  setText("global-progress-percent", "Working");
  try {
    const result = await operation();
    toast(typeof result === "string" ? result : `${label} complete`);
    return result;
  } catch (error) {
    toast(String(error), true);
    throw error;
  } finally {
    busy = false;
    window.setTimeout(() => progress.classList.add("hidden"), 700);
    updateNavidromeControls();
    await refreshDashboard();
  }
}

async function runNavidromeAction(button) {
  const mode = button.dataset.mode;
  if (mode === "external") return;
  await withBusy(
    mode === "install" ? "Installing Navidrome…" : mode === "start" ? "Starting Navidrome…" : "Stopping Navidrome…",
    async () => {
      if (mode === "install") return invoke("install_navidrome");
      if (mode === "start") return invoke("start_navidrome");
      return invoke("stop_navidrome");
    },
  );
}

async function runInvidiousAction(button) {
  const action = button.dataset.mode;
  if (action === "external") return;
  const labels = { install: "Setting up Invidious…", start: "Starting Invidious…", stop: "Stopping Invidious…" };
  await withBusy(labels[action], () => invoke("operate_invidious", { action }));
}

async function downloadAudio() {
  const url = byId("download-url").value.trim();
  if (!url) {
    byId("download-url").focus();
    toast("Paste a track or playlist URL first.", true);
    return;
  }
  byId("download-error").classList.add("hidden");
  byId("download-result").classList.add("hidden");
  try {
    const result = await withBusy("Downloading audio…", () => invoke("download_audio", {
      url,
      format: byId("download-format").value,
      quality: byId("download-quality").value,
    }));
    if (result) {
      byId("download-result").innerHTML = `<strong>Saved to your music library</strong>${result.files.map((file) => `<span>${escapeText(file)}</span>`).join("")}`;
      byId("download-result").classList.remove("hidden");
    }
  } catch (error) {
    setText("download-error", String(error));
    byId("download-error").classList.remove("hidden");
  }
}

async function saveSettings() {
  if (!currentDashboard) return false;
  const start = readTimeValue("setting-start-time");
  const stop = readTimeValue("setting-stop-time");
  const config = {
    ...currentDashboard.config,
    navidrome_port: Number(byId("setting-navidrome-port").value),
    invidious_port: Number(byId("setting-invidious-port").value),
    container_engine: byId("setting-container-engine").value,
    network_access: byId("setting-network-access").value,
    music_dir: byId("setting-music").value.trim(),
    data_dir: byId("setting-data").value.trim(),
    bin_dir: byId("setting-bin").value.trim(),
    plugins_dir: byId("setting-plugins").value.trim(),
    navidrome_username: byId("setting-navidrome-user").value.trim() || null,
    navidrome_password: byId("setting-navidrome-password").value || null,
    auto_start: byId("setting-autostart").checked,
    schedule_enabled: byId("setting-schedule").checked,
    schedule_start_hour: start.hour,
    schedule_start_minute: start.minute,
    schedule_stop_hour: stop.hour,
    schedule_stop_minute: stop.minute,
    discord_rpc_enabled: byId("setting-discord").checked,
    discord_artwork_tunnel: byId("setting-artwork-tunnel").value,
    tailscale: byId("setting-artwork-tunnel").value === "tailscale",
  };
  try {
    const saveMessage = await invoke("save_settings", { config });
    settingsDirty = false;
    setText("settings-message", saveMessage);
    toast(saveMessage);
    await refreshDashboard();
    return true;
  } catch (error) {
    setText("settings-message", String(error));
    toast(String(error), true);
    return false;
  }
}

function readRpcSettings() {
  return {
    navidromeUrl: byId("rpc-navidrome-url").value.trim(),
    navidromeUser: byId("rpc-navidrome-user").value.trim(),
    navidromePassword: byId("rpc-navidrome-password").value,
    coverMode: byId("rpc-cover-mode").value,
    navidromePublicUrl: byId("rpc-public-url").value.trim(),
    discordClientId: byId("rpc-client-id").value.trim(),
    pollInterval: Number(byId("rpc-poll-interval").value),
    coversListen: byId("rpc-covers-listen").value.trim() || "127.0.0.1:4534",
  };
}

async function saveRpcSettings() {
  const rpcConfig = readRpcSettings();
  if (rpcConfig.coverMode !== "off" && !rpcConfig.navidromePublicUrl) {
    const message = "Enter a public HTTPS artwork URL, or set Album Art Source to Off.";
    setText("rpc-settings-message", message);
    byId("rpc-public-url").focus();
    toast(message, true);
    throw new Error(message);
  }
  if (rpcConfig.coverMode !== "off" && /^http:\/\//i.test(rpcConfig.navidromePublicUrl)) {
    const message = "Use the public HTTPS address for your artwork tunnel (for example, your Funnel .ts.net URL), not the local http://127.0.0.1:4534 address.";
    setText("rpc-settings-message", message);
    byId("rpc-public-url").focus();
    toast(message, true);
    throw new Error(message);
  }
  if (rpcConfig.coverMode === "server"
    && !/^(?:127(?:\.\d{1,3}){3}|localhost|\[[0-9a-fA-F:]+\]):\d{1,5}$/.test(rpcConfig.coversListen)) {
    const message = "Cover server listen address must be a loopback address with a port, for example 127.0.0.1:4534. Do not enter a URL or omit the port.";
    setText("rpc-settings-message", message);
    byId("rpc-covers-listen").focus();
    toast(message, true);
    throw new Error(message);
  }
  try {
    await invoke("save_discord_rpc_settings", { rpcConfig });
    rpcSettingsDirty = false;
    rpcSettingsLoaded = true;
    setText("rpc-settings-message", "Saved locally with private file permissions.");
    toast("Discord integration settings saved.");
  } catch (error) {
    setText("rpc-settings-message", String(error));
    toast(String(error), true);
    throw error;
  }
}

async function saveInvidiousCompose() {
  try {
    const contents = await invoke("save_invidious_compose", {
      contents: byId("invidious-compose-editor").value,
      appSettings: {
        domain: byId("invidious-domain").value.trim(),
        externalPort: byId("invidious-external-port").value
          ? Number(byId("invidious-external-port").value)
          : null,
        httpsOnly: byId("invidious-https-only").checked,
        registrationEnabled: byId("invidious-registration-enabled").checked,
        statisticsEnabled: byId("invidious-statistics-enabled").checked,
      },
    });
    byId("invidious-compose-editor").value = contents;
    composeDirty = false;
    invidiousSettingsDirty = false;
    setText("invidious-compose-message", "Compose file saved. Validate it before starting the service.");
    toast("Invidious Compose file saved.");
  } catch (error) {
    setText("invidious-compose-message", String(error));
    toast(String(error), true);
    throw error;
  }
}

async function validateInvidiousCompose() {
  try {
    if (settingsDirty && !(await saveSettings())) return;
    if (composeDirty || invidiousSettingsDirty) await saveInvidiousCompose();
    await invoke("validate_invidious_compose");
    setText("invidious-compose-message", `Valid with ${currentDashboard.config.container_engine} Compose.`);
    toast("Invidious Compose configuration is valid.");
  } catch (error) {
    setText("invidious-compose-message", String(error));
    toast(String(error), true);
  }
}

document.addEventListener("click", async (event) => {
  const pageButton = event.target.closest("[data-page]");
  if (pageButton) {
    event.preventDefault();
    setPage(pageButton.dataset.page);
    return;
  }
  const actionButton = event.target.closest("[data-action]");
  if (!actionButton || actionButton.disabled) return;
  event.preventDefault();
  try {
    switch (actionButton.dataset.action) {
      case "open-navidrome": {
        const message = await invoke("open_navidrome");
        if (message) toast(message);
        break;
      }
      case "open-invidious": {
        const message = await invoke("open_invidious");
        if (message) toast(message);
        break;
      }
      case "open-navidrome-client": {
        const message = await invoke("open_navidrome_client");
        if (message) toast(message);
        break;
      }
      case "open-navidrome-browser": await invoke("open_navidrome_browser"); break;
      case "open-invidious-browser": await invoke("open_invidious_browser"); break;
      case "navidrome-primary": await runNavidromeAction(actionButton); break;
      case "invidious-primary": await runInvidiousAction(actionButton); break;
      default: break;
    }
  } catch (error) {
    toast(String(error), true);
  }
});

byId("refresh-button").addEventListener("click", refreshDashboard);
byId("download-button").addEventListener("click", downloadAudio);
byId("download-quality").addEventListener("change", () => {
  if (byId("download-format").value !== "mp3" && byId("download-quality").value === "320") {
    byId("download-quality").value = "best";
  }
});
byId("download-format").addEventListener("change", () => {
  byId("download-quality").disabled = byId("download-format").value !== "mp3";
  if (byId("download-quality").disabled) byId("download-quality").value = "best";
});
byId("setting-schedule").addEventListener("change", (event) => {
  byId("schedule-fields").classList.toggle("visible", event.target.checked);
});
document.querySelectorAll("#page-settings input").forEach((input) => {
  if (input.id.startsWith("setting-")) {
    input.addEventListener("input", () => { settingsDirty = true; });
    input.addEventListener("change", () => { settingsDirty = true; });
  }
});
byId("setting-artwork-tunnel").addEventListener("change", () => {
  settingsDirty = true;
  updateCoverHelp();
});
byId("setting-container-engine").addEventListener("change", () => { settingsDirty = true; });
byId("setting-network-access").addEventListener("change", () => {
  settingsDirty = true;
  updateSettingsNetworkNote();
});
byId("save-settings").addEventListener("click", saveSettings);
byId("rescan-navidrome").addEventListener("click", async () => {
  try {
    if (settingsDirty && !(await saveSettings())) return;
    const message = await invoke("rescan_navidrome");
    setText("rescan-message", message);
    toast("Navidrome library rescan requested.");
  } catch (error) {
    setText("rescan-message", String(error));
    toast(String(error), true);
  }
});
document.querySelectorAll("#rpc-navidrome-url, #rpc-navidrome-user, #rpc-navidrome-password, #rpc-cover-mode, #rpc-public-url, #rpc-client-id, #rpc-poll-interval, #rpc-covers-listen")
  .forEach((input) => {
    input.addEventListener("input", () => { rpcSettingsDirty = true; });
    input.addEventListener("change", () => {
      rpcSettingsDirty = true;
      updateCoverHelp();
    });
  });
byId("rpc-funnel-port").addEventListener("input", updateCoverHelp);
byId("rpc-covers-listen").addEventListener("input", updateCoverHelp);
byId("enable-funnel").addEventListener("click", async () => {
  const port = Number(byId("rpc-funnel-port").value);
  const coversListen = byId("rpc-covers-listen").value.trim() || "127.0.0.1:4534";
  const confirmed = window.confirm(
    `Enable Tailscale Funnel on public HTTPS port ${port}?\n\n`
      + `This will start SyncPulse's image-only cover server at ${coversListen} `
      + `and make it reachable from the public internet through your Tailscale hostname. `
      + `The entire image-only listener on this port will be public (not just a specific image URL); anyone may access it. `
      + `This may replace an existing Funnel route configured on the selected HTTPS port. `
      + `Tailscale must be installed, signed in, and Funnel enabled for your tailnet. `
      + `The Funnel configuration can persist across restarts until you turn it off.\n\nContinue?`,
  );
  if (!confirmed) return;
  const button = byId("enable-funnel");
  button.disabled = true;
  button.textContent = "Configuring Funnel...";
  setText("rpc-settings-message", "");
  try {
    const result = await invoke("enable_artwork_funnel", {
      httpsPort: port,
      coversListen,
    });
    byId("rpc-public-url").value = result.publicUrl;
    rpcSettingsDirty = true;
    await saveRpcSettings();
    setText("rpc-settings-message", `Funnel is active at ${result.publicUrl}; public artwork URL saved.`);
  } catch (error) {
    setText("rpc-settings-message", String(error));
    toast(String(error), true);
  } finally {
    button.disabled = false;
    button.textContent = "Enable Funnel & fill URL";
  }
});
byId("save-rpc-settings").addEventListener("click", async () => {
  try { await saveRpcSettings(); } catch (_) { /* Error is already shown to the user. */ }
});
byId("restart-rpc").addEventListener("click", async () => {
  try {
    await saveRpcSettings();
    byId("setting-discord").checked = true;
    settingsDirty = true;
    if (!(await saveSettings())) return;
    await withBusy("Restarting Discord Rich Presence…", () => invoke("restart_discord_rpc"));
  } catch (_) { /* Error is already shown to the user. */ }
});
byId("invidious-compose-editor").addEventListener("input", () => { composeDirty = true; });
byId("invidious-app-settings").querySelectorAll("input").forEach((input) => {
  input.addEventListener("input", () => { invidiousSettingsDirty = true; });
  input.addEventListener("change", () => { invidiousSettingsDirty = true; });
});
byId("load-invidious-compose").addEventListener("click", async () => {
  composeDirty = false;
  invidiousSettingsDirty = false;
  composeLoaded = false;
  await loadSettingsDetails();
});
byId("save-invidious-compose").addEventListener("click", async () => {
  try { await saveInvidiousCompose(); } catch (_) { /* Error is already shown to the user. */ }
});
byId("validate-invidious-compose").addEventListener("click", validateInvidiousCompose);
byId("copy-funnel-command").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(byId("rpc-funnel-command").textContent);
    toast("Tailscale Funnel command copied.");
  } catch (error) {
    toast(`Unable to copy command: ${String(error)}`, true);
  }
});
byId("install-discord").addEventListener("click", async () => {
  try {
    await withBusy("Installing Discord Rich Presence…", () => invoke("install_discord_rpc"));
    rpcSettingsLoaded = false;
  }
  catch (_) { /* Error is already shown as a toast. */ }
});
listen("operation-progress", (event) => {
  const progress = event.payload;
  setText("global-progress-message", progress.message);
  setText("global-progress-percent", `${Math.round(progress.percent)}%`);
  byId("global-progress-bar").style.width = `${Math.max(0, Math.min(100, progress.percent))}%`;
  byId("download-progress").classList.remove("hidden");
  setText("progress-message", progress.message);
  setText("progress-percent", `${Math.round(progress.percent)}%`);
  byId("progress-bar").style.width = `${Math.max(0, Math.min(100, progress.percent))}%`;
  if (!byId("welcome-wizard").classList.contains("hidden")
    && !byId("wizard-progress-step").classList.contains("hidden")) {
    setText("wizard-progress-message", progress.message);
    setText("wizard-progress-percent", `${Math.round(progress.percent)}%`);
    byId("wizard-progress-bar").style.width = `${Math.max(0, Math.min(100, progress.percent))}%`;
  }
});

refreshDashboard();
window.setInterval(refreshDashboard, 15000);
// Refresh immediately when the window becomes visible again, since polling is
// paused while it is hidden.
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) void refreshDashboard();
});
