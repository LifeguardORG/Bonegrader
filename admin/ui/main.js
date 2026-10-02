// Bonegrader Admin frontend. Talks to Rust via the global Tauri bridge.
// Dynamic text is only ever set via textContent (see `h`).

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
const sshTestBtn = el("sshTestBtn");
const historyBtn = el("historyBtn");
const resultCard = el("resultCard");
const statusLine = el("status");
const noticeSlot = el("notice");
const progressBox = el("progress");
const progressBar = el("bar");
const progressFill = el("barFill");
const progressText = el("progressText");

let lastPreview = null; // preview result the publish button refers to
let busy = false;

// --- helpers ---------------------------------------------------------------------

function put(parent, ...children) {
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    parent.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return parent;
}

/** Replace the children (null/false entries are skipped, see `put`). */
function fillIn(parent, ...children) {
  parent.replaceChildren();
  return put(parent, ...children);
}

function h(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v == null || v === false) continue;
    if (k === "class") node.className = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v === true ? "" : v);
  }
  return put(node, ...children);
}

// Feather Icons (MIT) / Lucide (ISC) shapes — see THIRD-PARTY-NOTICES.md.
const ICONS = {
  alert: [["circle", { cx: 12, cy: 12, r: 10 }], ["line", { x1: 12, y1: 8, x2: 12, y2: 12 }], ["line", { x1: 12, y1: 16, x2: 12.01, y2: 16 }]],
  warn: [
    ["path", { d: "M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" }],
    ["line", { x1: 12, y1: 9, x2: 12, y2: 13 }],
    ["line", { x1: 12, y1: 17, x2: 12.01, y2: 17 }],
  ],
  info: [["circle", { cx: 12, cy: 12, r: 10 }], ["line", { x1: 12, y1: 16, x2: 12, y2: 12 }], ["line", { x1: 12, y1: 8, x2: 12.01, y2: 8 }]],
  check: [["path", { d: "M22 11.08V12a10 10 0 1 1-5.93-9.14" }], ["polyline", { points: "22 4 12 14.01 9 11.01" }]],
};

function icon(name) {
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  const base = {
    viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": 2,
    "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true",
  };
  for (const [k, v] of Object.entries(base)) svg.setAttribute(k, v);
  for (const [tag, attrs] of ICONS[name]) {
    const node = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
    svg.append(node);
  }
  return svg;
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

const count = (n, one, many) => `${n} ${n === 1 ? one : many}`;
const lines = (id) => el(id).value.split("\n").map((l) => l.trim()).filter(Boolean);
const baseName = (p) => String(p).split("/").pop();

const DATE_OPTS = { day: "2-digit", month: "2-digit", year: "numeric", hour: "2-digit", minute: "2-digit" };

/** RFC 3339 or "20261002-164403" (UTC) -> local "02.10.2026, 18:44". */
function formatWhen(value) {
  if (!value) return "";
  const m = /^(\d{4})(\d{2})(\d{2})-(\d{2})(\d{2})(\d{2})$/.exec(value);
  const d = m ? new Date(Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +m[6])) : new Date(value);
  return isNaN(d) ? value : d.toLocaleString("de-DE", DATE_OPTS);
}

function setStatus(msg, kind = "") {
  statusLine.textContent = msg;
  statusLine.className = "status " + kind;
}

function settings() {
  const s = {};
  for (const f of TEXT_FIELDS) s[f] = el(f).value.trim();
  s.ignore = lines("ignore");
  s.seeds = lines("seeds");
  s.gc = el("gc").checked;
  return s;
}

// --- validation ---------------------------------------------------------------------

const PLAIN = /^[A-Za-z0-9@._~:/-]+$/; // what the server commands accept
const VERSION = /^\d+(\.\d+){0,3}([-+][0-9A-Za-z.-]+)?$/;
const isHttpsUrl = (v) => /^https:\/\/[^\s/]+/i.test(v) || /^http:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?(\/|$)/i.test(v);

function cmpVersion(a, b) {
  const pa = a.split(/[-+]/)[0].split(".").map(Number);
  const pb = b.split(/[-+]/)[0].split(".").map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d) return d;
  }
  return 0;
}

/** Field id -> [level, message]; level "err" blocks preview/publish. */
function validate(s) {
  const v = {};
  if (!s.instance) v.instance = ["err", "Pfad zur Instanz angeben (der Ordner mit mods/)."];
  if (!/^[A-Za-z0-9._-]+$/.test(s.channel) || s.channel === "." || s.channel === "..") {
    v.channel = ["err", "Nur Buchstaben, Ziffern, Punkt, Binde- und Unterstrich (z. B. main, beta)."];
  }
  if (!s.pack) v.pack = ["err", "Pack-Namen angeben – Spieler sehen ihn in der App."];
  if (!isHttpsUrl(s.baseUrl)) v.baseUrl = ["err", "Muss mit https:// beginnen (nur für lokale Tests: http://localhost)."];
  else if (s.channel && !s.baseUrl.replace(/\/+$/, "").endsWith("/" + s.channel)) {
    v.baseUrl = ["warn", `Die URL endet nicht auf /${s.channel} – stimmt der Channel?`];
  }
  if (s.sshHost && (!PLAIN.test(s.sshHost) || s.sshHost.startsWith("-") || s.sshHost.includes("/"))) {
    v.sshHost = ["err", "Form: nutzer@server (ohne Leerzeichen und Sonderzeichen)."];
  }
  if (s.remoteBase && (!PLAIN.test(s.remoteBase) || !/^[/~]/.test(s.remoteBase))) {
    v.remoteBase = ["err", "Absoluter Pfad, z. B. /srv/bonegrader (ohne Leer- und Sonderzeichen)."];
  }
  for (const f of ["minClientVersion", "latestClientVersion"]) {
    if (s[f] && !VERSION.test(s[f])) v[f] = ["err", "Versionsnummer wie 1.2.0"];
  }
  if (!v.minClientVersion && !v.latestClientVersion && s.minClientVersion && s.latestClientVersion &&
      cmpVersion(s.minClientVersion, s.latestClientVersion) > 0) {
    v.minClientVersion = ["warn", "Die Mindest-Version ist höher als die aktuelle Version."];
  }
  if (s.clientDownloadUrl && !isHttpsUrl(s.clientDownloadUrl)) v.clientDownloadUrl = ["err", "Muss mit https:// beginnen."];
  if ((s.minClientVersion || s.latestClientVersion) && !s.clientDownloadUrl) {
    v.clientDownloadUrl = ["warn", "Ohne Download-Seite sehen Spieler keinen Link zur neuen Version."];
  }
  if (s.serverAddress && !/^[A-Za-z0-9.-]+(:\d{1,5})?$/.test(s.serverAddress)) {
    v.serverAddress = ["err", "Adresse wie play.example.com oder play.example.com:25565"];
  }
  const badSeed = s.seeds.find((p) => p.includes("..") || p.startsWith("/") ||
    !(p === "options.txt" || /^(config|defaultconfigs|kubejs)\//.test(p) || ["config", "defaultconfigs", "kubejs"].includes(p)));
  if (badSeed) v.seeds = ["err", `Nicht erlaubt: ${badSeed} – nur options.txt, config/, defaultconfigs/, kubejs/.`];
  return v;
}

function showValidation() {
  const s = settings();
  const v = validate(s);
  for (const f of TEXT_FIELDS) {
    const msg = el("msg-" + f);
    const [level, text] = v[f] || ["", ""];
    if (msg) {
      msg.textContent = text;
      msg.className = "field-msg " + level;
    }
    el(f).classList.toggle("invalid", level === "err");
    el(f).setAttribute("aria-invalid", level === "err" ? "true" : "false");
  }
  // Problems in the collapsed section must not hide.
  if (["signKey", "minClientVersion", "latestClientVersion", "clientDownloadUrl", "serverAddress", "seeds"].some((f) => v[f]?.[0] === "err")) {
    el("more").open = true;
  }
  const blocking = Object.values(v).some(([level]) => level === "err");
  const sshMissing = !s.sshHost || !s.remoteBase || v.sshHost || v.remoteBase;
  previewBtn.disabled = busy || blocking;
  publishBtn.disabled = busy || blocking || sshMissing || !lastPreview || lastPreview.blocked;
  sshTestBtn.disabled = busy || !!sshMissing;
  historyBtn.disabled = busy || !!sshMissing || !!v.channel;
  renderContext(s);
  return !blocking;
}

function renderContext(s) {
  const items = [];
  if (s.channel) items.push(h("li", { class: s.channel === "main" ? "main" : "strong" }, `Channel ${s.channel}`));
  if (s.sshHost && s.remoteBase) items.push(h("li", {}, `${s.sshHost}:${s.remoteBase.replace(/\/+$/, "")}/${s.channel}`));
  el("context").replaceChildren(...items);
}

// --- notices ------------------------------------------------------------------------

/** Plain-language hint for the usual SSH/rsync/build failures. */
function hintFor(detail) {
  const rules = [
    [/Permission denied \(publickey/i, "Der Server akzeptiert deinen SSH-Schlüssel nicht. Stimmt das SSH-Ziel, und ist dein öffentlicher Schlüssel beim deploy-Nutzer hinterlegt?"],
    [/Host key verification failed/i, "Der Server ist noch nicht bekannt: einmal im Terminal „ssh <SSH-Ziel>“ ausführen und den Host-Schlüssel bestätigen."],
    [/Could not resolve hostname|Name or service not known|nodename nor servname/i, "Hostname nicht gefunden – SSH-Ziel prüfen."],
    [/Connection refused|timed out|No route to host|Network is unreachable/i, "Server nicht erreichbar – läuft er, und ist der SSH-Port offen?"],
    [/rsync.*(not found|starten)|command not found/i, "rsync fehlt – hier oder auf dem Server installieren (z. B. apt install rsync)."],
    [/Signatur-Schlüssel .* lesen|kein gültiger Schlüssel/i, "Signatur-Schlüssel nicht gefunden oder ungültig – Pfad unter „Erweitert“ prüfen."],
    [/Instanz-Ordner nicht gefunden/i, "Instanz-Ordner prüfen – er muss den mods/-Ordner enthalten."],
    [/Ordner auf dem Server anlegen|Permission denied/i, "Keine Schreibrechte auf dem Server – gehört die Remote-Basis dem deploy-Nutzer?"],
  ];
  return rules.find(([re]) => re.test(detail))?.[1] || "Details ansehen; mit „Details kopieren“ lässt sich der Fehler weitergeben.";
}

function clearNotice() {
  noticeSlot.replaceChildren();
}

async function copyText(text, btn) {
  let ok = false;
  try {
    await navigator.clipboard.writeText(text);
    ok = true;
  } catch {
    const ta = h("textarea", { "aria-hidden": "true" });
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.append(ta);
    ta.select();
    try {
      ok = document.execCommand("copy");
    } catch {
      ok = false;
    }
    ta.remove();
  }
  btn.textContent = ok ? "Kopiert ✓" : "Bitte markieren und kopieren";
}

function showNotice({ tone = "err", title, text, detail, actions = [] }) {
  const parts = [
    icon({ info: "info", ok: "check", warn: "warn" }[tone] || "alert"),
    h("div", { class: "notice-title" }, title),
    text ? h("p", { class: "notice-text" }, text) : null,
  ];
  if (actions.length) {
    parts.push(h("div", { class: "notice-actions" },
      actions.map(([label, onclick, primary]) => h("button", { type: "button", class: primary ? "small" : "ghost small", onclick }, label))));
  }
  if (detail) {
    const copy = h("button", { type: "button", class: "ghost small" }, "Details kopieren");
    copy.addEventListener("click", () => copyText(detail, copy));
    parts.push(h("details", {}, h("summary", {}, "Details"), h("pre", {}, detail), copy));
  }
  noticeSlot.replaceChildren(h("div", { class: "notice " + tone, role: tone === "err" ? "alert" : "status" }, parts));
  noticeSlot.scrollIntoView({ block: "nearest" });
}

function showError(title, e, retry) {
  const detail = typeof e === "object" && e?.detail ? e.detail : String(e?.message ?? e);
  showNotice({ title, text: hintFor(detail), detail, actions: retry ? [["Erneut versuchen", retry, true]] : [] });
  setStatus("");
}

// --- preview ------------------------------------------------------------------------

function diffRow(tagClass, tagText, { title, version, from, file }) {
  let ver = null;
  if (from && version && from !== version) ver = h("span", { class: "ver" }, from, " → ", h("b", {}, version));
  else if (version) ver = h("span", { class: "ver" }, version);
  return h("div", { class: "diff-row" },
    h("span", { class: "tag " + tagClass }, tagText),
    h("div", { class: "what" }, h("div", {}, h("span", { class: "title" }, title), ver),
      file && file !== title ? h("div", { class: "file" }, file) : null));
}

function pathRow(tagClass, tagText, path, labels) {
  const l = labels[path] || {};
  return diffRow(tagClass, tagText, { title: l.name || baseName(path), version: l.version, file: path });
}

function group(title, rows) {
  if (!rows.length) return null;
  return h("details", { class: "diff-group", open: rows.length <= 10 },
    h("summary", {}, `${title} `, h("span", { class: "count" }, `(${rows.length})`)), rows);
}

const SIGNING = {
  trusted: ["ok", "Wird signiert", "Der Schlüssel passt zu dem, dem die Spieler-App vertraut."],
  signedUnchecked: ["info", "Wird signiert",
    "Die Spieler-App prüft Signaturen aber erst, wenn der öffentliche Schlüssel in keys/manifest-signing.pub steht und eine neue App-Version verteilt ist."],
  unsigned: ["warn", "Achtung: unsigniert",
    "Wer den Webserver übernimmt, könnte den Spielern beliebige Mods unterschieben. Trag unter „Erweitert“ einen Signatur-Schlüssel ein (deploy/README.md, Abschnitt 7)."],
  unsignedRejected: ["err", "Veröffentlichen gesperrt: Signatur fehlt",
    "Die Spieler-App verlangt eine Signatur – unsigniert würde jedes Update abgelehnt. Signatur-Schlüssel unter „Erweitert“ eintragen."],
  untrustedKey: ["err", "Veröffentlichen gesperrt: falscher Schlüssel",
    "Dieser Schlüssel steht nicht in keys/manifest-signing.pub – die Spieler-App würde den Stand ablehnen."],
};

function changeSummary(d, files) {
  if (d.firstBuild) return `Erstveröffentlichung: ${count(files, "Datei", "Dateien")}`;
  const parts = [];
  if (d.updated.length) parts.push(count(d.updated.length, "Update", "Updates"));
  if (d.changed.length) parts.push(`${d.changed.length} geändert`);
  if (d.added.length) parts.push(`${d.added.length} neu`);
  if (d.removed.length) parts.push(`${d.removed.length} entfernt`);
  return parts.length ? parts.join(" · ") : "Keine Dateiänderungen";
}

function renderPreview(res) {
  const [tone, title, text] = SIGNING[res.signing];
  res.blocked = tone === "err";
  fillIn(
    el("summary"),
    h("span", { class: "pill" }, changeSummary(res.diff, res.files)),
    h("span", { class: "pill" },
      `${count(res.mods, "Mod", "Mods")} · ${count(res.resourcepacks, "Ressourcenpaket", "Ressourcenpakete")} · ${res.shaderpacks} Shader`),
    h("span", { class: "pill" }, res.loader),
    res.liveReachable
      ? h("span", { class: "pill" }, `Live: Stand vom ${formatWhen(res.liveGeneratedAt)}`)
      : h("span", { class: "pill warn" }, "Live-Stand nicht erreichbar (Erstveröffentlichung?)"),
    res.seeds > 0 ? h("span", { class: "pill" }, count(res.seeds, "Standard-Einstellung", "Standard-Einstellungen")) : null,
    res.ignored > 0 ? h("span", { class: "pill" }, `${res.ignored} ignoriert`) : null,
    res.noModId.length > 0 ? h("span", { class: "pill warn" }, `${res.noModId.length} ohne modId`) : null
  );
  el("signBox").replaceChildren(h("div", { class: "box " + tone, role: tone === "err" ? "alert" : null }, h("h3", {}, title), h("p", {}, text)));

  const body = el("diffBody");
  body.replaceChildren();
  const d = res.diff;
  const labels = d.labels || {};
  if (d.firstBuild) {
    put(body, group("Erstveröffentlichung", d.added.map((p) => pathRow("add", "neu", p, labels))));
  } else if (d.added.length + d.removed.length + d.changed.length + d.updated.length === 0) {
    body.append(h("p", { class: "muted" }, "Keine Dateiänderungen gegenüber dem Live-Stand. Veröffentlichen übernimmt nur geänderte Einstellungen (Versionen, Server, Signatur)."));
  } else {
    put(body,
      group("Aktualisiert", d.updated.map((u) => diffRow("upd", "update", {
        title: u.name || u.modId, from: u.oldVersion, version: u.newVersion, file: `${u.oldFile} → ${u.newFile}`,
      }))),
      group("Geändert (gleicher Name, neuer Inhalt)", d.changed.map((p) => pathRow("upd", "geändert", p, labels))),
      group("Neu", d.added.map((p) => pathRow("add", "neu", p, labels))),
      group("Entfernt", d.removed.map((p) => pathRow("rem", "entfernt", p, labels))));
  }
  for (const w of res.warnings) body.append(h("p", { class: "nomod" }, "⚠ " + w));
  if (res.noModId.length > 0) {
    body.append(h("p", { class: "nomod" }, "Ohne modId (Erkennung über den Dateinamen): " + res.noModId.join(", ")));
  }
  resultCard.classList.remove("hidden");
}

function showProgress(visible) {
  progressBox.classList.toggle("hidden", !visible);
  if (visible) updatePhase("start");
}

function updatePhase(phase) {
  const map = { start: [5, "Starte …"], build: [33, "Baue Manifest und Dateispeicher …"], upload: [70, "Lade zum Server …"], done: [100, "Fertig."] };
  const [pct, text] = map[phase] || [0, ""];
  progressFill.style.width = pct + "%";
  progressBar.setAttribute("aria-valuenow", String(pct));
  progressText.textContent = text;
}

function setBusy(on) {
  busy = on;
  showValidation();
}

// `quiet`: refresh after publishing without replacing the publish message.
async function doPreview({ quiet = false } = {}) {
  if (!invoke) return setStatus("Läuft nur in der Admin-App (Tauri).", "err");
  if (!showValidation()) return;
  if (!quiet) {
    clearNotice();
    setStatus("Baue und vergleiche mit dem Live-Stand …", "busy");
  }
  lastPreview = null;
  setBusy(true);
  try {
    const res = await invoke("preview", { settings: settings() });
    renderPreview(res);
    lastPreview = res;
    if (!quiet) {
      setStatus("Vorschau bereit – prüfen, dann veröffentlichen.", "ok");
      el("h-preview").focus();
    }
  } catch (e) {
    showError("Vorschau fehlgeschlagen", e, () => doPreview());
  } finally {
    setBusy(false);
  }
}

// --- publish ------------------------------------------------------------------------

function askPublish() {
  if (!lastPreview || lastPreview.blocked) return;
  const s = settings();
  const res = lastPreview;
  const main = s.channel === "main";
  el("confirmTitle").textContent = `Auf „${s.channel}“ veröffentlichen?`;
  el("confirmText").textContent = main
    ? "Alle Spieler bekommen diesen Stand beim nächsten Start von Bonegrader."
    : `Alle, die den Channel „${s.channel}“ nutzen, bekommen diesen Stand beim nächsten Start.`;
  const signing = SIGNING[res.signing];
  fillIn(
    el("confirmList"),
    h("li", {}, `Änderungen: ${changeSummary(res.diff, res.files)}`),
    h("li", {}, `Pack: ${res.pack} · ${res.loader} · ${count(res.files, "Datei", "Dateien")}`),
    h("li", {}, `Ziel: ${res.target || `${s.sshHost}:${s.remoteBase}/${s.channel}`}`),
    h("li", { class: signing[0] === "warn" ? "warn" : null }, signing[0] === "warn" ? "⚠ unsigniert" : "✓ signiert"),
    s.minClientVersion ? h("li", {}, `Spieler-App mindestens ${s.minClientVersion}`) : null,
    h("li", { class: "muted" }, "Der bisherige Stand wird im Verlauf gesichert und lässt sich zurückholen.")
  );
  el("confirmYes").textContent = main ? "Für alle veröffentlichen" : "Jetzt veröffentlichen";
  el("confirmDialog").classList.remove("hidden");
  el("confirmNo").focus();
}

function closeConfirm() {
  el("confirmDialog").classList.add("hidden");
  publishBtn.focus();
}

async function doPublish() {
  el("confirmDialog").classList.add("hidden");
  if (!invoke || !lastPreview) return;
  const s = settings();
  clearNotice();
  setStatus("Veröffentliche … (Fenster offen lassen)", "busy");
  setBusy(true);
  showProgress(true);
  try {
    const res = await invoke("publish", { settings: s });
    showProgress(false);
    let msg = `${count(res.files, "Datei", "Dateien")}, ${res.signed ? "signiert" : "unsigniert"}`;
    if (res.gcRemoved) msg += `, ${res.gcRemoved} alte Dateien lokal aufgeräumt`;
    showNotice({
      tone: "ok",
      title: `Auf „${s.channel}“ veröffentlicht`,
      text: msg + "." + (res.previous ? ` Der vorherige Stand liegt als ${res.previous} im Verlauf.` : ""),
    });
    setStatus("");
    setBusy(false);
    await doPreview({ quiet: true });
    if (el("historyList").childElementCount) await loadHistory();
  } catch (e) {
    showProgress(false);
    showError("Veröffentlichen fehlgeschlagen", e, () => doPublish());
    setBusy(false);
  }
}

// --- SSH test -------------------------------------------------------------------------

async function sshTest() {
  const out = el("sshResult");
  out.textContent = "Verbinde …";
  out.className = "inline-result";
  setBusy(true);
  try {
    const c = await invoke("ssh_test", { settings: settings() });
    const s = settings();
    const problems = [];
    if (!c.writable) problems.push(c.exists ? `keine Schreibrechte in ${s.remoteBase}` : `${s.remoteBase} fehlt und kann nicht angelegt werden`);
    if (!c.remoteRsync) problems.push("rsync fehlt auf dem Server");
    if (!c.localRsync) problems.push("rsync fehlt auf diesem Rechner");
    if (problems.length) {
      out.textContent = "⚠ Verbunden, aber: " + problems.join(", ") + ".";
      out.className = "inline-result warn";
    } else {
      out.textContent = `✓ Verbindung ok – ${s.remoteBase} ${c.exists ? "beschreibbar" : "wird beim ersten Veröffentlichen angelegt"}, rsync vorhanden.`;
      out.className = "inline-result ok";
    }
  } catch (e) {
    out.textContent = "✗ Keine Verbindung";
    out.className = "inline-result err";
    showError("Verbindungstest fehlgeschlagen", e, sshTest);
  } finally {
    setBusy(false);
  }
}

// --- history & rollback ------------------------------------------------------------------

async function loadHistory() {
  const list = el("historyList");
  setBusy(true);
  try {
    const entries = await invoke("history", { settings: settings() });
    renderHistory(entries);
    historyBtn.textContent = "Neu laden";
  } catch (e) {
    list.replaceChildren();
    showError("Verlauf konnte nicht geladen werden", e, loadHistory);
  } finally {
    setBusy(false);
  }
}

function renderHistory(entries) {
  const list = el("historyList");
  list.replaceChildren();
  if (!entries.length) {
    list.append(h("p", { class: "muted" }, "Noch nichts veröffentlicht."));
    return;
  }
  for (const e of entries) {
    const when = e.generatedAt ? `Stand vom ${formatWhen(e.generatedAt)}` : "Stand unbekannt";
    const meta = [
      e.live ? "jetzt live" : e.replacedAt ? `live bis ${formatWhen(e.replacedAt)}` : null,
      count(e.files, "Datei", "Dateien"),
      e.signed ? "signiert" : "unsigniert",
    ].filter(Boolean).join(" · ");
    const row = h("div", { class: "history-row" + (e.live ? " live" : "") },
      h("div", { class: "what" }, h("div", { class: "title" }, e.live ? `Live – ${when}` : when), h("div", { class: "meta" }, meta)));
    if (!e.live) {
      const btn = h("button", { type: "button", class: "ghost small" }, "Zurückrollen");
      let armed = null;
      btn.addEventListener("click", () => {
        if (!armed) {
          btn.textContent = "Wirklich zurückrollen?";
          btn.classList.add("danger");
          armed = setTimeout(() => {
            armed = null;
            btn.textContent = "Zurückrollen";
            btn.classList.remove("danger");
          }, 5000);
          return;
        }
        clearTimeout(armed);
        doRollback(e);
      });
      row.append(btn);
    }
    list.append(row);
  }
}

async function doRollback(entry) {
  const s = settings();
  clearNotice();
  setStatus(`Rolle „${s.channel}“ zurück …`, "busy");
  setBusy(true);
  try {
    const saved = await invoke("rollback_to", { settings: s, name: entry.name });
    showNotice({
      tone: "ok",
      title: `„${s.channel}“ zeigt wieder den Stand vom ${formatWhen(entry.generatedAt) || entry.name}`,
      text: `Spieler bekommen ihn beim nächsten Start. Der ersetzte Stand liegt als ${saved} im Verlauf.`,
    });
    setStatus("");
    setBusy(false);
    await loadHistory();
    if (lastPreview) await doPreview({ quiet: true });
  } catch (e) {
    showError("Rollback fehlgeschlagen", e, () => doRollback(entry));
    setBusy(false);
  }
}

// --- startup ------------------------------------------------------------------------

function loadSettings() {
  for (const f of TEXT_FIELDS) {
    el(f).value = load("admin." + f) ?? DEFAULTS[f] ?? "";
    el(f).addEventListener("input", () => {
      store("admin." + f, el(f).value);
      lastPreview = null; // settings changed: preview again first
      showValidation();
    });
  }
}

async function init() {
  loadSettings();
  previewBtn.addEventListener("click", () => doPreview());
  publishBtn.addEventListener("click", askPublish);
  sshTestBtn.addEventListener("click", sshTest);
  historyBtn.addEventListener("click", loadHistory);
  el("confirmNo").addEventListener("click", closeConfirm);
  el("confirmYes").addEventListener("click", doPublish);
  el("confirmDialog").addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeConfirm();
  });
  showValidation();
  if (listen) await listen("publish-progress", (e) => updatePhase(e.payload));
  if (!invoke) setStatus("Vorschau-Modus (kein Tauri) — Aktionen inaktiv.", "");
}

init();
