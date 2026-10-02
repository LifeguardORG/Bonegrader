// Bonegrader Admin frontend. Talks to Rust via the global Tauri bridge.
// Dynamic text is only ever set via textContent.

const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const el = (id) => document.getElementById(id);
const TEXT_FIELDS = [
  "instance", "channel", "pack", "baseUrl", "sshHost", "remoteBase", "ignore",
  "signKey", "minClientVersion", "latestClientVersion", "clientDownloadUrl",
  "serverName", "serverAddress", "seeds",
];
// Machine-specific values (paths, SSH target) have no defaults: the repo is
// public, and every admin's setup differs. Values typed once are remembered.
const DEFAULTS = {
  channel: "main",
  pack: "BonesAndBees",
  baseUrl: "https://bonegrader.rescue-compete.de/main",
  remoteBase: "/srv/bonegrader",
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

function load(key) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function store(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* not remembered */
  }
}

const lines = (id) => el(id).value.split("\n").map((l) => l.trim()).filter(Boolean);

function settings() {
  const s = {};
  for (const f of TEXT_FIELDS) s[f] = el(f).value.trim();
  s.ignore = lines("ignore");
  s.seeds = lines("seeds");
  s.gc = el("gc").checked;
  return s;
}

function loadSettings() {
  for (const f of TEXT_FIELDS) {
    el(f).value = load("admin." + f) ?? DEFAULTS[f] ?? "";
    el(f).addEventListener("input", () => {
      store("admin." + f, el(f).value);
      publishBtn.disabled = true; // settings changed: preview again first
    });
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

function note(text, cls) {
  const p = document.createElement("p");
  p.className = cls;
  p.textContent = text;
  return p;
}

function renderPreview(res) {
  const summary = el("summary");
  summary.replaceChildren(
    pill(`${res.files} Dateien`),
    pill(`${res.mods} Mods · ${res.resourcepacks} RP · ${res.shaderpacks} Shader`),
    pill(res.loader),
    pill(res.liveReachable ? "Server erreichbar" : "Server nicht erreichbar (Erstveröffentlichung)", res.liveReachable ? "" : "warn"),
    pill(res.signed ? "wird signiert" : "unsigniert", res.signed ? "" : "warn")
  );
  if (res.seeds > 0) summary.append(pill(`${res.seeds} Standard-Einstellungen`));
  if (res.ignored > 0) summary.append(pill(`${res.ignored} ignoriert`));
  if (res.noModId.length > 0) summary.append(pill(`${res.noModId.length} ohne modId`, "warn"));

  const body = el("diffBody");
  body.replaceChildren();
  const d = res.diff;
  if (d.firstBuild) {
    const g = group("Erstveröffentlichung", "add", "neu", d.added);
    if (g) body.append(g);
  } else if (d.added.length + d.removed.length + d.changed.length + d.updated.length === 0) {
    body.append(note("Keine Änderungen gegenüber dem Live-Stand — Veröffentlichen ist ein No-op.", "muted"));
  } else {
    const upd = d.updated.map((u) => `${u.modId}: ${u.oldFile} -> ${u.newFile}`);
    [
      group("Aktualisieren", "upd", "update", upd),
      group("Geändert", "upd", "geändert", d.changed),
      group("Neu", "add", "neu", d.added),
      group("Entfernen", "rem", "entfernen", d.removed),
    ].forEach((g) => g && body.append(g));
  }

  for (const w of res.warnings) body.append(note("⚠ " + w, "nomod"));
  if (res.noModId.length > 0) {
    body.append(note("Ohne modId (Dateiname-Identität): " + res.noModId.join(", "), "nomod"));
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

// `quiet`: refresh after publishing without replacing the publish message.
async function doPreview({ quiet = false } = {}) {
  if (!invoke) return setStatus("Läuft nur in der Admin-App (Tauri).", "err");
  if (!quiet) setStatus("Baue & vergleiche …", "busy");
  publishBtn.disabled = true;
  try {
    const res = await invoke("preview", { settings: settings() });
    renderPreview(res);
    publishBtn.disabled = false;
    if (!quiet) setStatus("Vorschau bereit — prüfen, dann veröffentlichen.", "ok");
  } catch (e) {
    setStatus("Fehler: " + e, "err");
  }
}

async function doPublish() {
  if (!invoke) return;
  const s = settings();
  if (!s.sshHost || !s.remoteBase) return setStatus("SSH-Ziel und Remote-Basis angeben.", "err");
  setStatus("Veröffentliche … (Fenster offen lassen)", "busy");
  publishBtn.disabled = true;
  previewBtn.disabled = true;
  showProgress(true);
  try {
    const res = await invoke("publish", { settings: s });
    let msg = `Veröffentlicht${res.signed ? " (signiert)" : ""}: ${res.files} Dateien, ${res.newBlobs} neue Blobs hochgeladen`;
    if (res.gcRemoved) msg += `, ${res.gcRemoved} lokal aufgeräumt`;
    showProgress(false);
    previewBtn.disabled = false;
    setStatus(msg + " Aktualisiere die Vorschau …", "ok");
    await doPreview({ quiet: true });
    setStatus(msg + ".", "ok");
  } catch (e) {
    setStatus("Fehler: " + e, "err");
    showProgress(false);
    previewBtn.disabled = false;
    publishBtn.disabled = false;
  }
}

async function init() {
  loadSettings();
  previewBtn.addEventListener("click", () => doPreview());
  publishBtn.addEventListener("click", doPublish);
  if (listen) await listen("publish-progress", (e) => updatePhase(e.payload));
  if (!invoke) setStatus("Vorschau-Modus (kein Tauri) — Aktionen inaktiv.", "");
}

init();
