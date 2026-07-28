// Bonegrader Admin frontend. Talks to Rust via the global Tauri bridge.

const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const el = (id) => document.getElementById(id);
const FIELDS = ["instance", "channel", "pack", "baseUrl", "sshHost", "remoteBase", "ignore"];
const DEFAULTS = {
  instance: "/home/jonas/Documents/curseforge/minecraft/Instances/BonesAndBees",
  channel: "main",
  pack: "BonesAndBees",
  baseUrl: "https://bonegrader.rescue-compete.de/main",
  sshHost: "root@62.171.170.74",
  remoteBase: "/srv/bonegrader",
  ignore: "",
};

const previewBtn = el("previewBtn");
const publishBtn = el("publishBtn");
const resultCard = el("resultCard");
const statusLine = el("status");
const progressBox = el("progress");
const progressBar = el("bar");
const progressText = el("progressText");

function setStatus(msg, kind = "") {
  statusLine.textContent = msg;
  statusLine.className = "status " + kind;
}

function settings() {
  const s = {};
  for (const f of FIELDS) s[f] = el(f).value.trim();
  return s;
}

function ignoreLines() {
  return el("ignore").value.split("\n").map((l) => l.trim()).filter(Boolean);
}

function loadSettings() {
  for (const f of FIELDS) {
    el(f).value = localStorage.getItem("admin." + f) ?? DEFAULTS[f];
    el(f).addEventListener("input", () => localStorage.setItem("admin." + f, el(f).value));
  }
}

function pill(text, cls = "") {
  const s = document.createElement("span");
  s.className = "pill " + cls;
  s.textContent = text;
  return s;
}

function group(title, tagClass, tagText, items) {
  if (!items.length) return null;
  const g = document.createElement("div");
  g.className = "diff-group";
  const h = document.createElement("h3");
  h.textContent = `${title} (${items.length})`;
  g.appendChild(h);
  for (const text of items) {
    const row = document.createElement("div");
    row.className = "diff-row";
    const tag = document.createElement("span");
    tag.className = "tag " + tagClass;
    tag.textContent = tagText;
    const code = document.createElement("code");
    code.textContent = text;
    row.append(tag, code);
    g.appendChild(row);
  }
  return g;
}

function renderPreview(res) {
  const summary = el("summary");
  summary.innerHTML = "";
  summary.append(
    pill(`${res.files} Dateien`),
    pill(`${res.mods} Mods · ${res.resourcepacks} RP · ${res.shaderpacks} Shader`),
    pill(res.loader),
    pill(res.liveReachable ? "Server erreichbar" : "Server nicht erreichbar (Erstveröffentlichung)", res.liveReachable ? "" : "warn"),
  );
  if (res.ignored > 0) summary.append(pill(`${res.ignored} ignoriert`));
  if (res.noModId.length > 0) summary.append(pill(`${res.noModId.length} ohne modId`, "warn"));

  const body = el("diffBody");
  body.innerHTML = "";
  const d = res.diff;
  if (d.firstBuild) {
    body.append(group("Erstveröffentlichung", "add", "neu", d.added) || document.createElement("div"));
  } else if (d.added.length + d.removed.length + d.changed.length + d.updated.length === 0) {
    const p = document.createElement("p");
    p.className = "muted";
    p.textContent = "Keine Änderungen gegenüber dem Live-Stand — Veröffentlichen ist ein No-op.";
    body.appendChild(p);
  } else {
    const upd = d.updated.map((u) => `${u.modId}: ${u.oldFile} -> ${u.newFile}`);
    [
      group("Aktualisieren", "upd", "update", upd),
      group("Geändert", "upd", "geändert", d.changed),
      group("Neu", "add", "neu", d.added),
      group("Entfernen", "rem", "entfernen", d.removed),
    ].forEach((g) => g && body.appendChild(g));
  }

  if (res.noModId.length > 0) {
    const note = document.createElement("p");
    note.className = "nomod";
    note.textContent = "Ohne modId (Dateiname-Identität): " + res.noModId.join(", ");
    body.appendChild(note);
  }
  resultCard.classList.remove("hidden");
}

function showProgress(visible) {
  progressBox.classList.toggle("hidden", !visible);
  if (visible) {
    progressBar.style.width = "0%";
    progressText.textContent = "";
  }
}

function updatePhase(phase) {
  const map = { build: [33, "Baue Manifest & Store …"], upload: [70, "Lade zum Server …"], done: [100, "Fertig."] };
  const [pct, text] = map[phase] || [0, ""];
  progressBar.style.width = pct + "%";
  progressText.textContent = text;
}

async function doPreview() {
  if (!invoke) return setStatus("Läuft nur in der Admin-App (Tauri).", "err");
  const s = settings();
  setStatus("Baue & vergleiche …", "busy");
  try {
    const res = await invoke("preview", {
      instance: s.instance, channel: s.channel, baseUrl: s.baseUrl, pack: s.pack, ignore: ignoreLines(),
    });
    renderPreview(res);
    publishBtn.disabled = false;
    setStatus("Vorschau bereit — prüfen, dann veröffentlichen.", "ok");
  } catch (e) {
    setStatus("Fehler: " + e, "err");
  }
}

async function doPublish() {
  if (!invoke) return;
  const s = settings();
  setStatus("Veröffentliche … (Fenster offen lassen)", "busy");
  publishBtn.disabled = true;
  previewBtn.disabled = true;
  showProgress(true);
  try {
    const res = await invoke("publish", {
      instance: s.instance, channel: s.channel, baseUrl: s.baseUrl,
      sshHost: s.sshHost, remoteBase: s.remoteBase, pack: s.pack,
      ignore: ignoreLines(), gc: el("gc").checked,
    });
    let msg = `Veröffentlicht: ${res.files} Dateien, ${res.newBlobs} neue Blobs hochgeladen`;
    if (res.gcRemoved) msg += `, ${res.gcRemoved} lokal aufgeräumt`;
    setStatus(msg + ".", "ok");
    showProgress(false);
    previewBtn.disabled = false;
    await doPreview();
  } catch (e) {
    setStatus("Fehler: " + e, "err");
    showProgress(false);
    previewBtn.disabled = false;
    publishBtn.disabled = false;
  }
}

async function init() {
  loadSettings();
  previewBtn.addEventListener("click", doPreview);
  publishBtn.addEventListener("click", doPublish);
  if (listen) await listen("publish-progress", (e) => updatePhase(e.payload));
  if (!invoke) setStatus("Vorschau-Modus (kein Tauri) — Aktionen inaktiv.", "");
}

init();
