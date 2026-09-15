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
}

// --- Settings --------------------------------------------------------------
const FIELDS = {
  "log-directory": "logDirectory",
  "output-directory": "outputDirectory",
  "output-mode": "outputMode",
  fps: "fps",
  "stop-delay-secs": "stopDelaySecs",
};

async function loadConfig() {
  const c = await invoke("get_config");
  for (const [id, key] of Object.entries(FIELDS)) $(id).value = c[key];
  $("record-audio").checked = !!c.recordAudio;
  $("quality").value = c.quality;
  updateQualityLabel();
}

function updateQualityLabel() {
  const q = +$("quality").value;
  const word = q >= 85 ? "Very high" : q >= 65 ? "High" : q >= 45 ? "Medium" : q >= 25 ? "Low" : "Very low";
  $("quality-val").textContent = `${word} (${q}%)`;
}
$("quality").addEventListener("input", updateQualityLabel);

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
  c.recordAudio = $("record-audio").checked;
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

loadConfig();
refresh();
refreshGameRes();
setInterval(refresh, 2000);
setInterval(refreshGameRes, 3000);
