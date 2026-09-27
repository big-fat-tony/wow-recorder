const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

// --- Status ----------------------------------------------------------------
async function refresh() {
  let s;
  try {
    s = await invoke("get_status");
  } catch {
    return;
  }
  if (s.version) $("version").textContent = `v${s.version}`;
  $("backend").textContent = `backend: ${s.backend}`;

  $("watching").textContent = s.watching ? "active" : "off";
  $("watching").className = "pill " + (s.watching ? "on" : "off");
  $("logdir").textContent = s.logDirectory || "(not set)";
  $("logdir").title = s.logDirectory || "";
  $("outdir").textContent = s.outputDirectory || "(not set)";
  $("outdir").title = s.outputDirectory || "";

  const rec = s.recording;
  $("rec-dot").className = "rec-dot" + (rec ? " live" : "");
  $("status-badge").className = "status-badge" + (rec ? " rec" : "");
  $("status-badge").textContent = rec ? "🔴" : "🎬";
  $("status-title").textContent = rec ? "Recording" : "Idle";
  $("status-sub").textContent = rec
    ? "A Mythic+ run is being captured."
    : s.watching
    ? "Waiting for a Mythic+ to start…"
    : "Not watching — set a Logs directory below.";
  $("stop-recording").hidden = !rec;
}

// Abandoned a key and still standing in the dungeon? Nothing in the combat log
// says so, so let the run be ended by hand instead of waiting for the backstop.
$("stop-recording").addEventListener("click", async () => {
  const button = $("stop-recording");
  button.disabled = true;
  button.textContent = "Saving…";
  try {
    await invoke("stop_recording");
  } finally {
    setTimeout(() => {
      button.disabled = false;
      button.textContent = "Stop & save";
      refresh();
    }, 1500);
  }
});

// --- Settings --------------------------------------------------------------
const FIELDS = {
  "log-directory": "logDirectory",
  "output-directory": "outputDirectory",
  "output-mode": "outputMode",
  "audio-mode": "audioMode",
  fps: "fps",
  "stop-delay-secs": "stopDelaySecs",
  "abandon-timeout-secs": "abandonTimeoutSecs",
};

async function loadConfig() {
  const c = await invoke("get_config");
  for (const [id, key] of Object.entries(FIELDS)) $(id).value = c[key];
  $("quality").value = c.quality;
  $("mic-gain-db").value = c.micGainDb;
  $("desktop-gain-db").value = c.desktopGainDb;
  $("timer-overlay").checked = !!c.timerOverlay;
  $("cover-chat").checked = !!c.coverChat;
  const [cx, cy, cw, ch] = c.coverRectPct || [1, 75, 21, 21];
  $("cover-x").value = cx;
  $("cover-y").value = cy;
  $("cover-w").value = cw;
  $("cover-h").value = ch;
  updateCoverFields();
  updateQualityLabel();
  updateMicGainLabel();
  updateDesktopGainLabel();
}

function updateQualityLabel() {
  const q = +$("quality").value;
  const word = q >= 85 ? "Very high" : q >= 65 ? "High" : q >= 45 ? "Medium" : q >= 25 ? "Low" : "Very low";
  $("quality-val").textContent = `${word} (${q}%)`;
}
$("quality").addEventListener("input", updateQualityLabel);

function updateMicGainLabel() {
  const g = +$("mic-gain-db").value;
  $("mic-gain-val").textContent = g === 0 ? "off (as captured)" : `${g > 0 ? "+" : ""}${g} dB`;
}
$("mic-gain-db").addEventListener("input", updateMicGainLabel);

function updateDesktopGainLabel() {
  const g = +$("desktop-gain-db").value;
  $("desktop-gain-val").textContent = g === 0 ? "as captured (may clip)" : `${g > 0 ? "+" : ""}${g} dB`;
}
$("desktop-gain-db").addEventListener("input", updateDesktopGainLabel);

function updateCoverFields() {
  $("cover-rect").hidden = !$("cover-chat").checked;
}
$("cover-chat").addEventListener("change", updateCoverFields);

async function refreshGameRes() {
  const r = await invoke("get_wow_resolution");
  $("game-res").textContent = r ? `${r.width} × ${r.height}` : "WoW not running";
}

async function saveConfig() {
  const c = await invoke("get_config");
  c.logDirectory = $("log-directory").value.trim();
  c.outputDirectory = $("output-directory").value.trim();
  c.outputMode = $("output-mode").value;
  c.fps = +$("fps").value;
  c.quality = +$("quality").value;
  c.stopDelaySecs = +$("stop-delay-secs").value;
  c.abandonTimeoutSecs = +$("abandon-timeout-secs").value;
  c.audioMode = $("audio-mode").value;
  c.micGainDb = +$("mic-gain-db").value;
  c.desktopGainDb = +$("desktop-gain-db").value;
  c.timerOverlay = $("timer-overlay").checked;
  c.coverChat = $("cover-chat").checked;
  c.coverRectPct = [+$("cover-x").value, +$("cover-y").value, +$("cover-w").value, +$("cover-h").value];
  await invoke("set_config", { config: c });
  const saved = $("saved");
  saved.classList.add("show");
  setTimeout(() => saved.classList.remove("show"), 1600);
  refresh();
}

$("save").addEventListener("click", saveConfig);
$("open-folder").addEventListener("click", () => invoke("open_recordings_folder"));

$("log-browse").addEventListener("click", async () => {
  const dir = await invoke("pick_directory", { start: $("log-directory").value || null });
  if (dir) $("log-directory").value = dir;
});
$("out-browse").addEventListener("click", async () => {
  const dir = await invoke("pick_directory", { start: $("output-directory").value || null });
  if (dir) $("output-directory").value = dir;
});
$("log-detect").addEventListener("click", async () => {
  const dir = await invoke("detect_log_directory");
  if (dir) $("log-directory").value = dir;
});

// --- Auto-update -----------------------------------------------------------
listen("update-available", ({ payload }) => {
  if (!payload || !payload.version) return; // never show without a real version
  $("update-message").textContent = `Version ${payload.version} is available.`;
  $("update-banner").hidden = false;
});
listen("download-progress", ({ payload }) => {
  if (payload.total) {
    const pct = Math.round((payload.downloaded / payload.total) * 100);
    $("update-message").textContent = `Downloading update… ${pct}%`;
  }
});
$("update-now").addEventListener("click", () => {
  $("update-now").disabled = true;
  invoke("install_update").catch((e) => {
    console.warn("update install failed:", e);
    $("update-banner").hidden = true;
  });
});
$("update-dismiss").addEventListener("click", () => {
  $("update-banner").hidden = true;
});

async function loadAutostart() {
  try { $("autostart").checked = await invoke("get_autostart"); } catch {}
}
$("autostart").addEventListener("change", async () => {
  const el = $("autostart");
  try {
    await invoke("set_autostart", { enabled: el.checked });
  } catch (e) {
    el.checked = !el.checked; // revert on failure
    console.warn("autostart toggle failed:", e);
  }
});

loadConfig();
loadAutostart();
refresh();
refreshGameRes();
setInterval(refresh, 2000);
setInterval(refreshGameRes, 3000);

// --- Quitting ---------------------------------------------------------------
// Quitting mid-run waits for the video file to be closed. Say what is being
// waited on rather than freezing, and keep an escape hatch for a machine that
// needs its resources back right now.
listen("shutdown-progress", ({ payload }) => {
  if (!payload || payload.stage === null || payload.stage === undefined) {
    $("shutdown-stage").textContent = "Done — quitting…";
    return;
  }
  $("shutdown-overlay").hidden = false;
  $("shutdown-stage").textContent = `${payload.stage}…`;
});

$("force-quit").addEventListener("click", () => {
  $("force-quit-warning").hidden = false;
  $("force-quit").hidden = true;
});
$("force-quit-confirm").addEventListener("click", () => invoke("force_quit"));
