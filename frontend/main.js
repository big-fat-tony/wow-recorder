const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

async function refresh() {
  const s = await invoke("get_status");
  if (s.version) document.getElementById("version").textContent = `v${s.version}`;
  $("backend").textContent = `· backend: ${s.backend}`;
  $("watching").textContent = s.watching ? "yes" : "no";
  $("watching").className = "pill " + (s.watching ? "on" : "off");
  $("recording").textContent = s.recording ? "REC" : "idle";
  $("recording").className = "pill " + (s.recording ? "on" : "off");
  $("logdir").textContent = s.logDirectory || "(not set)";
}

async function loadConfig() {
  const c = await invoke("get_config");
  for (const k of ["logDirectory","outputDirectory","width","height","fps","bitrateKbps","stopDelaySecs"]) {
    const el = document.getElementById(k.replace(/[A-Z]/g, (m) => "-" + m.toLowerCase()));
    if (el) el.value = c[k];
  }
}

$("save").addEventListener("click", async () => {
  const c = await invoke("get_config");
  c.logDirectory = $("log-directory").value.trim();
  c.outputDirectory = $("output-directory").value.trim();
  c.width = +$("width").value; c.height = +$("height").value;
  c.fps = +$("fps").value; c.bitrateKbps = +$("bitrate-kbps").value;
  c.stopDelaySecs = +$("stop-delay-secs").value;
  await invoke("set_config", { config: c });
  refresh();
});
$("open-folder").addEventListener("click", () => invoke("open_recordings_folder"));

loadConfig();
refresh();
setInterval(refresh, 2000);

// --- Auto-update ---
listen("update-available", ({ payload }) => {
  if (!payload || !payload.version) return; // never show without a real version
  document.getElementById("update-message").textContent = `Version ${payload.version} is available.`;
  document.getElementById("update-banner").hidden = false;
});
listen("download-progress", ({ payload }) => {
  if (payload.total) {
    const pct = Math.round((payload.downloaded / payload.total) * 100);
    document.getElementById("update-message").textContent = `Downloading update… ${pct}%`;
  }
});
document.getElementById("update-now").addEventListener("click", () => {
  document.getElementById("update-now").disabled = true;
  invoke("install_update").catch((e) => {
    console.warn("update install failed:", e);
    document.getElementById("update-banner").hidden = true;
  });
});
document.getElementById("update-dismiss").addEventListener("click", () => {
  document.getElementById("update-banner").hidden = true;
});
