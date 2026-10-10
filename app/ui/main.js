// Bonegrader frontend. Talks to the Rust core via the global Tauri bridge
// (enabled by `withGlobalTauri: true`). No bundler, no npm imports.
//
// All dynamic text is inserted with textContent (see `h`) — never as HTML —
// so names from launcher files or the server manifest cannot inject markup.

const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const DEFAULT_URL = "https://bonegrader.rescue-compete.de/main";
const STORE_URL = "bonegrader.baseUrl";
// JSON {path, profileKey, name, launcher}; older versions stored a bare path.
const STORE_INSTANCE = "bonegrader.lastInstance";

const el = (id) => document.getElementById(id);
const baseUrlInput = el("baseUrl");
const serverInfoBox = el("serverInfo");
const serverHost = el("serverHost");
const serverEdit = el("serverEdit");
const contextList = el("context");
const instancesBox = el("instances");
const checkBtn = el("checkBtn");
const planBody = el("planBody");
const applyBtn = el("applyBtn");
const undoBtn = el("undoBtn");
const backBtn = el("backBtn");
const recheckBtn = el("recheckBtn");
const cancelBtn = el("cancelBtn");
const resultBox = el("result");
const statusLine = el("status");
const banner = el("banner");
const noticeSlot = el("notice");
const progressBox = el("progress");
const progressBar = el("bar");
const progressFill = el("barFill");
const progressTitle = el("progressTitle");
const progressPct = el("progressPct");
const progressText = el("progressText");
const progressFile = el("progressFile");
const closeDialog = el("closeDialog");
const next1 = el("next1");
const stepDots = [...el("stepper").querySelectorAll(".step-dot")];
const panels = [...document.querySelectorAll(".panel")];

let serverInfo = null; // manifest_info for the current URL
let instances = []; // detected + manually added
let selected = null; // { path, name, launcher, profileKey, ... }
let lastPlan = null; // plan view of the last check
let confirmedInstance = false; // player confirmed a suspicious instance
let busy = false;
let currentStep = 1; // step shown on the right
let maxStep = 1; // furthest step reached so far (controls what's clickable)
let work = null; // "apply" | "undo" | "install" while the backend must not be interrupted
let phase = null; // progress phase of the running update
let closeAfterWork = false; // the window was closed while working: quit when done
let compatState = null; // the channel's hint about this Bonegrader version
let appUpdate = null; // newer Bonegrader the updater found: {version, current, notes}
let appProgress = null; // self-update progress: {phase, downloaded, total}

const removeExtras = new Set();
const keepCollisions = new Set();

// --- helpers -------------------------------------------------------------------

/** Append children; strings become text nodes, never HTML. Skips null/false
 *  (plain Element.append would insert the text "null"). */
function put(parent, ...children) {
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    parent.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return parent;
}

/** Create an element with attributes/listeners and children (see `put`). */
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
  server: [
    ["rect", { x: 3, y: 4, width: 18, height: 7, rx: 1.6 }],
    ["rect", { x: 3, y: 13, width: 18, height: 7, rx: 1.6 }],
    ["line", { x1: 7, y1: 7.5, x2: 7.01, y2: 7.5 }],
    ["line", { x1: 7, y1: 16.5, x2: 7.01, y2: 16.5 }],
  ],
  download: [
    ["path", { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" }],
    ["polyline", { points: "7 10 12 15 17 10" }],
    ["line", { x1: 12, y1: 15, x2: 12, y2: 3 }],
  ],
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
    /* storage unavailable: just don't remember */
  }
}

const baseName = (p) => String(p).split(/[/\\]/).pop() || String(p);
/** "…/minecraft/Instances/BonesAndBees" for long paths. */
function shortPath(p) {
  const parts = String(p).split(/[/\\]/).filter(Boolean);
  return parts.length > 3 ? "…/" + parts.slice(-3).join("/") : String(p);
}

/** "1 Datei" / "3 Dateien". */
const count = (n, one, many) => `${n} ${n === 1 ? one : many}`;

const num = (n, digits = 0) => n.toLocaleString("de-DE", { minimumFractionDigits: digits, maximumFractionDigits: digits });

function humanBytes(n) {
  if (n < 1024) return num(n) + " B";
  if (n < 1024 * 1024) return num(n / 1024) + " KB";
  if (n < 1024 * 1024 * 1024) return num(n / 1024 / 1024, 1) + " MB";
  return num(n / 1024 / 1024 / 1024, 2) + " GB";
}

function formatEta(sec) {
  if (sec < 10) return "ein paar Sekunden";
  if (sec < 60) return `${Math.ceil(sec / 5) * 5} Sek.`;
  if (sec < 3600) return `${Math.ceil(sec / 60)} Min.`;
  return `${num(sec / 3600, 1)} Std.`;
}

const LOADER_NAMES = { neoforge: "NeoForge", forge: "Forge", fabric: "Fabric", quilt: "Quilt" };
const loaderName = (t) => LOADER_NAMES[t] || t || "?";
const LAUNCHER_NAMES = { CurseForge: "CurseForge", Prism: "Prism", Vanilla: "Offizieller Launcher", Manual: "Ordner" };
const launcherName = (l) => LAUNCHER_NAMES[l] || l || "Ordner";

const DATE_OPTS = { day: "2-digit", month: "2-digit", year: "numeric", hour: "2-digit", minute: "2-digit" };
const SHORT_DATE_OPTS = { day: "2-digit", month: "2-digit", hour: "2-digit", minute: "2-digit" };

/** Backup stamp "20260801-120000" (UTC) -> Date. */
function stampDate(ts) {
  const m = /^(\d{4})(\d{2})(\d{2})-(\d{2})(\d{2})(\d{2})?/.exec(ts || "");
  return m ? new Date(Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +(m[6] || 0))) : null;
}

/** RFC 3339 or backup stamp -> "02.10.2026, 18:40" in local time. */
function formatWhen(value, opts = DATE_OPTS) {
  const d = stampDate(value) || (value ? new Date(value) : null);
  return d && !isNaN(d) ? d.toLocaleString("de-DE", opts) : value || "";
}

function setStatus(msg, kind = "") {
  statusLine.textContent = msg;
  statusLine.className = "status " + kind;
}

// --- notices: errors explained in plain words --------------------------------------

// Title and what to do, per error kind (see crates/client/src/errors.rs).
const ERRORS = {
  offline: ["Keine Verbindung zum Update-Server",
    "Prüfe deine Internetverbindung. Funktioniert sie, ist der Server vielleicht kurz nicht erreichbar – versuche es gleich noch einmal."],
  notFound: ["Diesen Server-Stand gibt es nicht", "Stimmt die Server-Adresse? Wenn ja, bitte dem Admin Bescheid geben."],
  server: ["Der Update-Server meldet einen Fehler", "Versuche es in ein paar Minuten noch einmal. Hält es an, bitte dem Admin Bescheid geben."],
  badUrl: ["Ungültige Server-Adresse", "Die Adresse muss mit https:// beginnen."],
  invalid: ["Unter dieser Adresse liegt kein Bonegrader-Server", "Prüfe die Server-Adresse – sie endet meist auf /main."],
  signature: ["Der Server-Stand ist nicht vertrauenswürdig",
    "Die Signatur fehlt oder passt nicht, deshalb installiert Bonegrader nichts davon. Bitte dem Admin Bescheid geben."],
  tooOld: ["Diese Bonegrader-Version ist zu alt", "Lade die neue Version herunter und installiere sie – danach klappt das Update."],
  locked: ["Minecraft läuft noch", "Eine Mod-Datei ist gerade in Benutzung. Schließe Minecraft und den Launcher, dann erneut versuchen."],
  diskFull: ["Der Speicher ist voll", "Mach etwas Platz auf der Festplatte frei und versuche es erneut."],
  permission: ["Keine Schreibrechte",
    "Bonegrader darf in diesem Instanz-Ordner nichts ändern. Prüfe die Ordner-Rechte oder wähle eine andere Instanz."],
  corrupt: ["Eine Datei kam beschädigt an",
    "Das passiert bei wackeligen Verbindungen. Versuche es erneut – bereits geladene Dateien werden wiederverwendet."],
  instance: ["Instanz-Ordner nicht gefunden", "Der Ordner wurde verschoben oder gelöscht. Bitte wähle die Instanz neu."],
  cancelled: ["Abgebrochen", "Es wurde nichts verändert. Bereits geladene Dateien werden beim nächsten Mal wiederverwendet."],
  other: ["Das hat nicht geklappt", "Versuche es erneut. Hält das Problem an, kopiere die Details und schick sie dem Admin."],
};

/** Commands reject with `{kind, message, detail}`; anything else is "other". */
function asReport(e) {
  if (e && typeof e === "object" && typeof e.kind === "string") return e;
  const text = String(e?.message ?? e);
  return { kind: "other", message: text, detail: text };
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
    icon(tone === "info" ? "info" : tone === "warn" ? "warn" : "alert"),
    h("div", { class: "notice-title" }, title),
    text ? h("p", { class: "notice-text" }, text) : null,
  ];
  const buttons = actions.map(([label, onclick, primary]) =>
    h("button", { type: "button", class: primary ? "small" : "ghost small", onclick }, label)
  );
  if (buttons.length) parts.push(h("div", { class: "notice-actions" }, buttons));
  if (detail) {
    const copy = h("button", { type: "button", class: "ghost small" }, "Details kopieren");
    copy.addEventListener("click", () => copyText(detail, copy));
    parts.push(h("details", {}, h("summary", {}, "Details"), h("pre", {}, detail), copy));
  }
  const node = h("div", { class: "notice " + tone, role: tone === "info" ? "status" : "alert" }, parts);
  noticeSlot.replaceChildren(node);
  return node;
}

/** Explain a failed command. `retry` re-runs it; `changed` says whether the
 *  instance may have been modified (false = nothing was touched). */
function showError(e, { retry, changed = null } = {}) {
  const r = asReport(e);
  const [title, hint] = ERRORS[r.kind] || ERRORS.other;
  let text = hint;
  if (r.kind !== "cancelled" && changed === false) text += " Es wurde nichts verändert.";
  if (/Rücknahme unvollständig/.test(r.detail)) {
    text += " Achtung: Nicht alles konnte zurückgenommen werden – siehe Details.";
  }
  const actions = [];
  if (retry) actions.push(["Erneut versuchen", retry, true]);
  if (["offline", "notFound", "invalid", "badUrl", "server"].includes(r.kind) && currentStep !== 1) {
    actions.push(["Server ändern", () => { goStep(1); serverEdit.open = true; baseUrlInput.focus(); }]);
  }
  if (["offline", "notFound", "invalid", "badUrl"].includes(r.kind) && currentStep === 1) serverEdit.open = true;
  if ((r.kind === "instance" || r.kind === "permission") && currentStep !== 2) {
    actions.push(["Instanz wählen", () => goStep(2)]);
  }
  const url = (lastPlan?.compat || serverInfo?.compat)?.downloadUrl;
  if (r.kind === "tooOld" && appUpdate) actions.push(["Bonegrader jetzt aktualisieren", installAppUpdate, !retry]);
  else if (r.kind === "tooOld" && url) actions.push(["Zum Download", () => openUrl(url)]);
  showNotice({
    tone: r.kind === "cancelled" ? "info" : "err",
    title,
    text,
    detail: r.kind === "cancelled" ? null : r.detail,
    actions,
  });
  setStatus("");
}

function openUrl(url) {
  invoke("open_url", { url }).catch((e) => showError(e));
}

// --- buttons & wizard -----------------------------------------------------------------

function refreshButtons() {
  const hasUrl = !!baseUrlInput.value.trim();
  next1.disabled = !hasUrl || busy;
  checkBtn.disabled = !(selected && hasUrl) || busy;
  const blocked = lastPlan?.compat?.state === "updateRequired";
  const unconfirmed = lastPlan?.assessment?.suspicious && !confirmedInstance;
  const nothing = !lastPlan || (lastPlan.noop && removeExtras.size === 0);
  const running = work === "apply";
  applyBtn.disabled = busy || blocked || unconfirmed || nothing;
  applyBtn.classList.toggle("hidden", running || (!!lastPlan && nothing));
  recheckBtn.classList.toggle("hidden", running || !lastPlan || !nothing);
  recheckBtn.disabled = busy;
  backBtn.classList.toggle("hidden", running);
  cancelBtn.classList.toggle("hidden", !running);
  undoBtn.disabled = busy;
  if (running) undoBtn.classList.add("hidden");
}

function renderStepper() {
  stepDots.forEach((dot) => {
    const n = Number(dot.dataset.step);
    const reachable = n <= maxStep && !work;
    dot.classList.toggle("active", n === currentStep);
    dot.classList.toggle("done", reachable && n !== currentStep);
    dot.disabled = !reachable && n !== currentStep;
    if (n === currentStep) dot.setAttribute("aria-current", "step");
    else dot.removeAttribute("aria-current");
  });
}

// Show step `n`. `grow` unlocks it (and marks it reachable) when advancing;
// `focus` moves keyboard focus to the step's heading (announced by screen readers).
function goStep(n, { grow = false, focus = true } = {}) {
  if (n < 1 || n > panels.length) return;
  if (grow) maxStep = Math.max(maxStep, n);
  if (n > maxStep) return;
  currentStep = n;
  panels.forEach((p) => p.classList.toggle("active", Number(p.dataset.step) === n));
  renderStepper();
  if (focus) panels[n - 1].querySelector("h2")?.focus();
}

/** A changed URL or instance invalidates the reviewed plan. */
function invalidatePlan(toStep) {
  lastPlan = null;
  maxStep = Math.min(maxStep, toStep);
  renderStepper();
  refreshButtons();
}

// --- context line & update hints ------------------------------------------------------

function renderContext() {
  const items = [];
  if (serverInfo) {
    const l = serverInfo.loader;
    items.push([serverInfo.packName, "strong"]);
    items.push([`${loaderName(l.type)} ${l.loaderVersion}`]);
    items.push([`MC ${l.mcVersion}`]);
    if (serverInfo.signature === "verified") items.push(["✓ signiert", "ok"]);
  }
  if (selected) items.push([`Instanz: ${selected.name}`]);
  contextList.replaceChildren(...items.map(([t, c]) => h("li", { class: c || null, title: t }, t)));
  contextList.classList.toggle("hidden", items.length === 0);
}

function showCompat(compat) {
  compatState = compat;
  renderBanner();
}

/** Where to download Bonegrader by hand: the channel's hint, else its site. */
function downloadPage() {
  if (compatState?.downloadUrl) return compatState.downloadUrl;
  try {
    return new URL(baseUrlInput.value.trim()).origin + "/";
  } catch {
    return null;
  }
}

// One banner for "a newer Bonegrader exists": with the updater it installs
// itself, otherwise the channel's hint links to the download page.
function renderBanner() {
  banner.replaceChildren();
  banner.className = "banner hidden";
  const required = compatState?.state === "updateRequired";
  if (appProgress) {
    const p = appProgress;
    const pct = p.total ? ` ${Math.min(100, Math.round((p.downloaded / p.total) * 100))} %` : "";
    put(banner, h("span", { class: "banner-text" },
      p.phase === "download" ? `Lade Bonegrader ${appUpdate?.version ?? ""} …${pct}` : "Installiere – Bonegrader startet gleich neu …"));
    banner.className = "banner info";
    return;
  }
  if (appUpdate) {
    put(
      banner,
      h("span", { class: "banner-text" },
        required
          ? `Diese Bonegrader-Version ist zu alt für den Server – Bonegrader ${appUpdate.version} behebt das.`
          : `Bonegrader ${appUpdate.version} ist verfügbar.`),
      h("button", { type: "button", class: "small", onclick: installAppUpdate }, "Jetzt aktualisieren"),
      appUpdate.notes
        ? h("details", { class: "banner-notes" }, h("summary", {}, "Was ist neu?"), h("p", {}, appUpdate.notes))
        : null
    );
    banner.className = "banner " + (required ? "err" : "info");
    return;
  }
  if (!compatState || compatState.state === "current") return;
  put(
    banner,
    h("span", { class: "banner-text" },
      required
        ? `Diese Bonegrader-Version ist zu alt für den Server – bitte auf ${compatState.min} oder neuer aktualisieren.`
        : `Bonegrader ${compatState.latest} ist verfügbar.`),
    compatState.downloadUrl
      ? h("button", { type: "button", class: "link", onclick: () => openUrl(compatState.downloadUrl) }, "Zum Download")
      : null
  );
  banner.className = "banner " + (required ? "err" : "info");
}

// --- Bonegrader updating itself -----------------------------------------------------

/** Quietly ask whether a newer Bonegrader exists (errors just mean "not now"). */
async function checkAppUpdate() {
  try {
    appUpdate = await invoke("check_app_update");
  } catch {
    appUpdate = null;
  }
  renderBanner();
}

async function installAppUpdate() {
  if (work || busy) {
    setStatus("Erst das laufende Update abwarten, dann Bonegrader aktualisieren.", "busy");
    return;
  }
  clearNotice();
  work = "appupdate";
  busy = true;
  appProgress = { phase: "download", downloaded: 0, total: null };
  refreshButtons();
  renderStepper();
  renderBanner();
  try {
    // macOS/Linux restart right after this; on Windows the installer takes over.
    await invoke("install_app_update");
    appProgress = { phase: "install", downloaded: 0, total: null };
    renderBanner();
  } catch (e) {
    work = null;
    busy = false;
    appProgress = null;
    refreshButtons();
    renderStepper();
    renderBanner();
    const page = downloadPage();
    showNotice({
      tone: "err",
      title: "Das Bonegrader-Update hat nicht geklappt",
      text: "Versuch es noch einmal – oder lade die neue Version von der Download-Seite und installiere sie darüber.",
      detail: asReport(e).detail,
      actions: [["Erneut versuchen", installAppUpdate, true], page ? ["Zur Download-Seite", () => openUrl(page)] : null].filter(Boolean),
    });
    afterWork();
  }
}

// --- step 1: server ----------------------------------------------------------------

function renderServer() {
  serverInfoBox.replaceChildren();
  serverInfoBox.classList.toggle("hidden", !serverInfo);
  el("intro").classList.toggle("hidden", !!serverInfo);
  const url = baseUrlInput.value.trim();
  serverHost.textContent = url ? "Update-Server: " + url.replace(/^https?:\/\//, "") : "Keine Server-Adresse eingetragen.";
  if (serverInfo) {
    const l = serverInfo.loader;
    put(
      serverInfoBox,
      icon("server"),
      h("div", { class: "name" }, serverInfo.packName),
      h("div", { class: "meta" },
        `${loaderName(l.type)} ${l.loaderVersion} · Minecraft ${l.mcVersion} · ${count(serverInfo.files, "Datei", "Dateien")}`),
      serverInfo.generatedAt ? h("div", { class: "meta" }, `Stand vom ${formatWhen(serverInfo.generatedAt)}`) : null,
      serverInfo.signature === "verified" ? h("div", { class: "signed" }, "✓ Signatur des Servers geprüft") : null
    );
  }
  renderContext();
}

async function checkServer() {
  const baseUrl = baseUrlInput.value.trim();
  if (!baseUrl || !invoke) return;
  clearNotice();
  busy = true;
  refreshButtons();
  setStatus("Verbinde mit dem Server …", "busy");
  try {
    serverInfo = await invoke("manifest_info", { baseUrl });
    store(STORE_URL, baseUrl);
    renderServer();
    showCompat(serverInfo.compat);
    setStatus("");
    serverEdit.open = false;
    goStep(2, { grow: true });
    renderInstances();
  } catch (e) {
    serverInfo = null;
    renderServer();
    showError(e, { retry: checkServer });
  } finally {
    busy = false;
    refreshButtons();
  }
}

// --- step 2: instance ----------------------------------------------------------------

/** How well an instance fits the pack (higher = better). */
function matchScore(inst) {
  if (!serverInfo) return 0;
  const l = serverInfo.loader;
  let s = 0;
  if (inst.name && inst.name.toLowerCase().includes(serverInfo.packName.toLowerCase())) s += 4;
  if (inst.mcVersion && inst.mcVersion === l.mcVersion) s += 2;
  if (inst.loaderType && inst.loaderType === l.type) s += 1;
  if (inst.loaderVersion && inst.loaderVersion === l.loaderVersion) s += 1;
  return s;
}

function mismatch(inst) {
  const want = serverInfo?.loader?.mcVersion;
  if (want && inst.mcVersion && inst.mcVersion !== want) {
    return `Minecraft ${inst.mcVersion} – der Server braucht ${want}`;
  }
  return null;
}

function describe(inst) {
  const loader = [loaderName(inst.loaderType), inst.loaderVersion].filter((x) => x && x !== "?").join(" ");
  const mc = inst.mcVersion ? "MC " + inst.mcVersion : "";
  if (inst.loaderType) return [loader, mc].filter(Boolean).join(" · ");
  if (mc) return mc + " · kein Mod-Loader";
  return shortPath(inst.path);
}

const sameInstance = (a, b) => a && b && a.path === b.path && (a.profileKey || null) === (b.profileKey || null);

function renderInstances() {
  const list = [...instances].sort((a, b) => matchScore(b) - matchScore(a) || a.name.localeCompare(b.name));
  instancesBox.replaceChildren();
  if (!list.length) {
    instancesBox.append(h("p", { class: "muted" }, "Keine Instanz gefunden – lege eine eigene an oder wähle den Ordner."));
  }
  for (const inst of list) {
    const warn = mismatch(inst);
    const active = sameInstance(inst, selected);
    const node = h(
      "div",
      {
        class: "instance" + (active ? " active" : ""),
        role: "option",
        "aria-selected": active ? "true" : "false",
        tabindex: "0",
        title: inst.path,
      },
      h(
        "div",
        { class: "instance-text" },
        h("div", { class: "name" }, active ? h("span", { class: "check", "aria-hidden": "true" }, "✓ ") : null, inst.name),
        h("div", { class: "meta" }, describe(inst)),
        warn ? h("div", { class: "warn-line small" }, "⚠ " + warn) : null
      ),
      h(
        "div",
        { class: "badges" },
        matchScore(inst) >= 4 ? h("span", { class: "badge good" }, "passt") : null,
        h("span", { class: "badge" }, launcherName(inst.launcher))
      )
    );
    node.addEventListener("click", () => selectInstance(inst));
    node.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        selectInstance(inst);
      }
    });
    instancesBox.append(node);
  }
  // Nothing fits the pack: a separate instance is the safe choice.
  if (serverInfo && !list.some((i) => matchScore(i) >= 4)) {
    instancesBox.append(
      h(
        "div",
        { class: "recommend" },
        h("p", {},
          `Kein passendes Profil für „${serverInfo.packName}“ gefunden? „Eigene Instanz anlegen“ richtet einen eigenen ` +
            "Spielordner im offiziellen Launcher ein – deine anderen Welten und Mods bleiben unberührt."),
        h("button", { type: "button", class: "small", onclick: createInstance }, "Eigene Instanz anlegen")
      )
    );
  }
  // Pre-select the instance used last time, else only a clear match — never
  // just "the first one" (that might be a different modpack).
  if (!selected && list.length) {
    const last = rememberedInstance();
    const pick =
      (last && list.find((i) => i.path === last.path && (!last.profileKey || i.profileKey === last.profileKey))) ||
      (matchScore(list[0]) >= 4 ? list[0] : null);
    if (pick) selectInstance(pick, { keepFocus: true });
  }
}

function selectInstance(inst, { keepFocus = false } = {}) {
  if (!sameInstance(inst, selected)) invalidatePlan(2);
  selected = inst;
  confirmedInstance = false;
  if (!instances.some((i) => sameInstance(i, inst))) instances.push(inst);
  const hadFocus = instancesBox.contains(document.activeElement);
  renderInstances();
  renderContext();
  refreshButtons();
  if (hadFocus && !keepFocus) instancesBox.querySelector(".instance.active")?.focus();
}

function rememberedInstance() {
  const raw = load(STORE_INSTANCE);
  if (!raw) return null;
  if (!raw.startsWith("{")) return { path: raw, name: baseName(raw), launcher: "Manual" };
  try {
    const v = JSON.parse(raw);
    return typeof v?.path === "string" ? v : null;
  } catch {
    return null;
  }
}

function rememberInstance(inst) {
  const { path, profileKey, name, launcher } = inst;
  store(STORE_INSTANCE, JSON.stringify({ path, profileKey: profileKey || null, name, launcher }));
}

// Re-detect instances, keeping the selection bound to its fresh data (so
// loader/version reflect reality after an install).
async function refreshInstances() {
  let list;
  try {
    list = await invoke("detect_instances");
  } catch {
    return;
  }
  const extra = instances.filter((i) => !list.some((d) => d.path === i.path));
  instances = [...list, ...extra];
  if (selected) {
    const fresh = list.find((i) => i.path === selected.path && (!selected.profileKey || i.profileKey === selected.profileKey));
    if (fresh) selected = fresh;
  }
  renderInstances();
  renderContext();
}

async function pickFolder() {
  try {
    const path = await invoke("pick_folder");
    if (path) selectInstance({ name: baseName(path), path, launcher: "Manual" });
  } catch (e) {
    showError(e);
  }
}

async function createInstance() {
  const packName = serverInfo?.packName || "BonesAndBees";
  clearNotice();
  try {
    const inst = await invoke("create_instance", { packName });
    selectInstance(inst);
    setStatus(`Eigener Ordner für „${packName}“ angelegt – jetzt „Prüfen“, danach kannst du NeoForge dafür einrichten.`, "ok");
  } catch (e) {
    showError(e, { retry: createInstance });
  }
}

// --- step 3: plan ----------------------------------------------------------------

const prettyFile = (name) => String(name).replace(/\.(jar|zip)$/i, "");
const CATEGORY_NOTES = { resourcepack: "Ressourcenpaket", shaderpack: "Shader" };

/** One line of the change list. */
function row(tagClass, tagText, { title, version, from, file, note, control }) {
  let ver = null;
  if (from && version && from !== version) ver = h("span", { class: "ver" }, from, " → ", h("b", {}, version));
  else if (version) ver = h("span", { class: "ver" }, version);
  const showFile = file && baseName(file) !== title;
  return h(
    "div",
    { class: "plan-row" },
    h("span", { class: "tag " + tagClass }, tagText),
    h(
      "div",
      { class: "what" },
      h("div", {}, h("span", { class: "title" }, title), ver),
      showFile || note ? h("div", { class: "file" }, [note, showFile ? baseName(file) : null].filter(Boolean).join(" · ")) : null
    ),
    control
  );
}

function entryRow(tagClass, tagText, entry, from) {
  return row(tagClass, tagText, {
    title: entry.modName || prettyFile(entry.fileName),
    version: entry.modVersion,
    from,
    file: entry.fileName,
    note: CATEGORY_NOTES[entry.category],
  });
}

function localRow(tagClass, tagText, path, local, extra = {}) {
  const meta = local[path] || {};
  return row(tagClass, tagText, { title: meta.name || prettyFile(baseName(path)), version: meta.version, file: path, ...extra });
}

function toggle(labelText, onChange) {
  const cb = h("input", { type: "checkbox" });
  cb.addEventListener("change", () => {
    onChange(cb.checked);
    refreshButtons();
  });
  return h("label", {}, cb, labelText);
}

/** A collapsible group; long lists start closed (the summary has the count). */
function group(title, rows, { note, open, tool } = {}) {
  if (!rows.length) return null;
  const isOpen = open ?? rows.length <= 8;
  return h(
    "details",
    { class: "plan-group", open: isOpen },
    h("summary", {}, title, h("span", { class: "count" }, `(${rows.length})`)),
    note ? h("p", { class: "group-note" }, note) : null,
    tool,
    rows
  );
}

function renderPlan(view) {
  removeExtras.clear();
  keepCollisions.clear();
  planBody.replaceChildren();
  const plan = view.plan;
  const a = view.assessment;
  const local = view.local || {};

  if (a.suspicious) {
    const msg = a.previousPack
      ? `Diese Instanz wurde bisher mit „${a.previousPack}“ aktualisiert, nicht mit „${view.packName}“.`
      : `Nur ${a.packMods} von ${a.localMods} Mods in dieser Instanz ${a.packMods === 1 ? "gehört" : "gehören"} zu „${view.packName}“. ` +
        "Ein Update würde gemeinsame Mods durch die Versionen des Packs ersetzen.";
    const cb = h("input", { type: "checkbox" });
    cb.addEventListener("change", () => {
      confirmedInstance = cb.checked;
      refreshButtons();
    });
    planBody.append(
      h("div", { class: "box err" }, h("h3", {}, "Ist das die richtige Instanz?"), h("p", {}, msg),
        h("label", { class: "confirm" }, cb, "Ja, das ist die richtige Instanz"))
    );
  }

  const installs = plan.downloads.filter((d) => !d.replaces);
  const updates = plan.downloads.filter((d) => d.replaces);
  const removed = plan.removals.length + plan.duplicates.length;
  const stand = view.generatedAt ? ` · Server-Stand vom ${formatWhen(view.generatedAt)}` : "";

  if (view.noop) {
    planBody.append(
      h("div", { class: "hero" }, icon("check"),
        h("div", {}, h("div", { class: "hero-title" }, `${view.packName} ist aktuell`),
          h("div", { class: "hero-text" }, `Instanz „${selected?.name ?? "?"}“${stand}`)))
    );
  } else {
    const changes = plan.downloads.length + removed + plan.collisions.length + view.seeds.length + (view.server ? 1 : 0);
    const size = view.downloadBytes > 0 ? ` · ${humanBytes(view.downloadBytes)} zu laden` : "";
    planBody.append(
      h("div", { class: "hero pending" }, icon("download"),
        h("div", {}, h("div", { class: "hero-title" }, `${count(changes, "Änderung", "Änderungen")} für ${view.packName}`),
          h("div", { class: "hero-text" }, `Bitte kurz ansehen, dann „Aktualisieren“${size}${stand}`))),
      h("div", { class: "summary-row" },
        installs.length ? h("span", { class: "chip add" }, `${installs.length} neu`) : null,
        updates.length ? h("span", { class: "chip upd" }, count(updates.length, "Update", "Updates")) : null,
        removed ? h("span", { class: "chip rem" }, `${removed} entfernt`) : null,
        plan.collisions.length ? h("span", { class: "chip rem" }, count(plan.collisions.length, "Konflikt", "Konflikte")) : null,
        plan.userExtras.length ? h("span", { class: "chip" }, `${plan.userExtras.length} eigene`) : null)
    );
  }

  // Rows of the downloads, so a kept collision can strike its pack copy through.
  const rowFor = new Map();
  const dlRow = (d, tagClass, tagText) => {
    const r = entryRow(tagClass, tagText, d.entry, d.replaces ? local[d.replaces]?.version : null);
    rowFor.set(d.entry.path, r);
    return r;
  };

  const groups = [];
  groups.push(group(
    "Konflikte",
    plan.collisions.map((c) => {
      const pack = plan.downloads.find((d) => d.entry.path === c.manifestPath)?.entry;
      const mine = local[c.localPath] || {};
      const packVersion = pack?.modVersion ?? local[c.manifestPath]?.version;
      const packVer = packVersion ? ` (Pack: ${packVersion})` : "";
      return localRow("clash", "Konflikt", c.localPath, local, {
        note: `deine Datei${packVer}`,
        control: toggle("Meine behalten", (keep) => {
          keep ? keepCollisions.add(c.localPath) : keepCollisions.delete(c.localPath);
          rowFor.get(c.manifestPath)?.classList.toggle("excluded", keep);
        }),
        version: mine.version,
      });
    }),
    {
      open: true,
      note:
        "Diese Mods hast du selbst installiert, das Pack bringt sie auch mit – beide zusammen lassen das Spiel abstürzen. " +
        "Normalerweise kommt deine Datei ins Backup und die Pack-Version wird installiert. „Meine behalten“ lässt deine " +
        "Datei stehen; dann fehlt die Pack-Version, und der Server kann dich abweisen.",
    }
  ));
  groups.push(group("Neu", installs.map((d) => dlRow(d, "add", "neu"))));
  groups.push(group("Aktualisiert", updates.map((d) => dlRow(d, "upd", "update"))));
  groups.push(group("Entfernt", plan.removals.map((p) => localRow("rem", "entfernt", p, local)), {
    note: "Gehören nicht mehr zum Pack – sie landen im Backup.",
  }));
  groups.push(group("Doppelt", plan.duplicates.map((p) => localRow("rem", "doppelt", p, local)), {
    note: "Identische Kopien (z. B. „jei (1).jar“) – zwei gleiche Mods würden das Spiel abstürzen lassen.",
  }));
  groups.push(group("Standard-Einstellungen", view.seeds.map((p) => row("add", "neu", { title: p })), {
    note: "Werden nur angelegt, weil sie bei dir fehlen – deine eigenen Einstellungen bleiben.",
  }));
  if (view.server) {
    groups.push(group("Serverliste", [row("add", "neu", { title: view.server.name, note: view.server.address })], {
      note: "Der Server wird einmal in deine Mehrspieler-Liste eingetragen.",
    }));
  }
  if (plan.userExtras.length) {
    const boxes = [];
    const rows = plan.userExtras.map((p) => {
      const t = toggle("entfernen", (rm) => (rm ? removeExtras.add(p) : removeExtras.delete(p)));
      boxes.push(t.querySelector("input"));
      return localRow("own", "eigen", p, local, { control: t });
    });
    const all = h("input", { type: "checkbox" });
    all.addEventListener("change", () => {
      for (const b of boxes) {
        b.checked = all.checked;
        b.dispatchEvent(new Event("change"));
      }
    });
    groups.push(group("Deine eigenen Mods", rows, {
      note: "Gehören nicht zum Pack und bleiben erhalten. Nur entfernen, wenn sie Probleme machen.",
      tool: rows.length > 1 ? h("label", { class: "confirm group-note" }, all, "Alle eigenen Mods entfernen") : null,
    }));
  }
  put(planBody, groups);

  renderUndo(view.restorable);
}

function renderUndo(restorable) {
  undoBtn.classList.toggle("hidden", !restorable);
  disarmUndo(restorable);
}

// Compare the selected instance's loader against the pack's required loader.
// Runs for every launcher type: the vanilla + NeoForge path can be installed or
// repaired from here; other launchers only get a short warning so the player
// can fix the version in their own launcher.
async function checkLoader(view) {
  let st;
  try {
    st = await invoke("loader_status", {
      required: view.loader,
      launcher: selected.launcher || "Manual",
      loaderType: selected.loaderType || null,
      loaderVersion: selected.loaderVersion || null,
    });
  } catch {
    return;
  }
  if (st.state === "ok") return;

  const req = loaderName(st.requiredType) + " " + st.requiredVersion;
  const have =
    st.installedType || st.installedVersion
      ? loaderName(st.installedType) + " " + (st.installedVersion || "?")
      : null;
  const box = h("div", { class: "box warn" });

  if (st.canInstall) {
    const name = selected.name || "BonesAndBees";
    put(
      box,
      h("h3", {}, st.state === "mismatch" ? `Falsche Version: ${have}` : `${req} fehlt noch`),
      h("p", {}, st.state === "mismatch"
        ? `Der Server braucht ${req} (MC ${st.mcVersion}). Bonegrader kann das für dich korrigieren.`
        : `Ohne Mod-Loader startet Minecraft ohne Mods. Bonegrader kann ${req} für dich einrichten.`),
      st.javaAvailable ? null : h("p", { class: "warn-line" }, "Kein Java gefunden – starte Minecraft einmal oder installiere Java, dann erneut prüfen."),
      h("details", {}, h("summary", {}, "Was passiert dabei?"),
        h("p", {}, `Bonegrader lädt den offiziellen NeoForge-Installer und richtet das Profil „${name}“ ein. ` +
          "Danach im offiziellen Launcher genau dieses Profil auswählen – sonst startet weiter Vanilla."))
    );
    const btn = h("button", { type: "button", class: "small" }, st.state === "mismatch" ? "NeoForge korrigieren" : "NeoForge einrichten");
    btn.disabled = !st.javaAvailable;
    btn.addEventListener("click", async () => {
      clearNotice();
      setStatus(`Richte NeoForge ${st.requiredVersion} ein … (kann eine Minute dauern)`, "busy");
      btn.disabled = true;
      work = "install";
      renderStepper();
      try {
        await invoke("install_loader", {
          baseUrl: baseUrlInput.value.trim(),
          gameDir: selected.path,
          packName: name,
          profileKey: selected.profileKey || null,
        });
        setStatus(`NeoForge ${st.requiredVersion} ist eingerichtet – im offiziellen Launcher das Profil „${name}“ starten.`, "ok");
        box.remove();
        await refreshInstances();
      } catch (e) {
        showError(e);
        btn.disabled = false;
      } finally {
        work = null;
        renderStepper();
        afterWork();
      }
    });
    put(box, h("div", { class: "box-actions" }, btn));
  } else {
    put(
      box,
      h("h3", {}, st.state === "unknown" ? "Loader-Version prüfen" : `Falsche Loader-Version: ${have}`),
      h("p", {}, have
        ? `Der Server braucht ${req} (MC ${st.mcVersion}) – bitte im Launcher genau diese Version einstellen.`
        : `Der Server braucht ${req} (MC ${st.mcVersion}). Bitte im Launcher prüfen, ob die Instanz genau diese Version nutzt.`),
      h("details", {}, h("summary", {}, "Warum?"),
        h("p", {}, "Mit einer anderen Loader-Version lässt dich der Server nicht beitreten, auch wenn alle Mods stimmen."))
    );
  }
  planBody.prepend(box);
}

// --- progress ------------------------------------------------------------------------

let samples = []; // [time ms, doneBytes] of the last seconds, for speed and ETA

function showProgress(visible) {
  progressBox.classList.toggle("hidden", !visible);
  planBody.classList.toggle("hidden", visible);
  if (visible) {
    samples = [];
    progressFill.style.width = "0%";
    progressBar.setAttribute("aria-valuenow", "0");
    progressTitle.textContent = "Bereite das Update vor …";
    progressPct.textContent = "";
    progressText.textContent = "";
    progressFile.textContent = "";
    cancelBtn.disabled = false;
    cancelBtn.textContent = "Abbrechen";
  }
}

function updateProgress(p) {
  if (!p || work !== "apply") return;
  phase = p.phase;
  let pct = 0;
  if (p.totalBytes > 0) pct = (p.doneBytes / p.totalBytes) * 100;
  else if (p.totalFiles > 0) pct = (p.doneFiles / p.totalFiles) * 100;
  if (p.phase === "apply" || p.phase === "done") pct = 100;
  progressFill.style.width = pct.toFixed(1) + "%";
  progressBar.setAttribute("aria-valuenow", pct.toFixed(0));
  progressPct.textContent = `${pct.toFixed(0)} %`;

  if (p.phase === "download") {
    const now = performance.now();
    samples.push([now, p.doneBytes]);
    while (samples.length > 2 && now - samples[0][0] > 8000) samples.shift();
    const [t0, b0] = samples[0];
    const secs = (now - t0) / 1000;
    const rate = secs >= 1.5 ? (p.doneBytes - b0) / secs : 0;
    const parts = [
      `${p.doneFiles} von ${count(p.totalFiles, "Datei", "Dateien")}`,
      `${humanBytes(p.doneBytes)} von ${humanBytes(p.totalBytes)}`,
    ];
    if (rate > 0) {
      parts.push(`${humanBytes(rate)}/s`);
      parts.push(`noch ca. ${formatEta((p.totalBytes - p.doneBytes) / rate)}`);
    }
    progressTitle.textContent = cancelBtn.disabled ? "Breche ab …" : "Lade Dateien …";
    progressText.textContent = parts.join(" · ");
    progressFile.textContent = p.current ? baseName(p.current) : "";
    progressBar.setAttribute("aria-valuetext", `${pct.toFixed(0)} Prozent, ${parts.slice(0, 2).join(", ")}`);
  } else if (p.phase === "apply") {
    progressTitle.textContent = "Wende Änderungen an …";
    progressText.textContent = "Gleich fertig – bitte Bonegrader nicht schließen.";
    progressFile.textContent = "";
    cancelBtn.classList.add("hidden");
    progressBar.setAttribute("aria-valuetext", "Wende Änderungen an");
  } else if (p.phase === "done") {
    progressTitle.textContent = "Fertig.";
    progressText.textContent = "";
  }
}

async function cancelUpdate() {
  cancelBtn.disabled = true;
  cancelBtn.textContent = "Breche ab …";
  progressTitle.textContent = "Breche ab …";
  try {
    await invoke("cancel_update");
  } catch {
    /* the update ends on its own */
  }
}

// --- result ------------------------------------------------------------------------

function startHint() {
  const n = selected?.name || "deine Instanz";
  switch (selected?.launcher) {
    case "CurseForge":
      return `Jetzt in der CurseForge-App die Instanz „${n}“ starten – viel Spaß!`;
    case "Prism":
      return `Jetzt im Prism Launcher die Instanz „${n}“ starten – viel Spaß!`;
    case "Vanilla":
      return `Jetzt im offiziellen Minecraft-Launcher das Profil „${n}“ auswählen und spielen.`;
    default:
      return "Jetzt Minecraft mit dieser Instanz starten – viel Spaß!";
  }
}

function showResult(lines) {
  resultBox.replaceChildren(...lines.filter(Boolean));
  resultBox.classList.remove("hidden");
}

function hideResult() {
  resultBox.replaceChildren();
  resultBox.classList.add("hidden");
}

function folderButton(label, path) {
  return h("button", { type: "button", class: "ghost small", onclick: () => invoke("open_folder", { path }).catch((e) => showError(e)) }, label);
}

// --- check, apply, undo ----------------------------------------------------------------

async function doCheck({ keepResult = false, quiet = false } = {}) {
  if (!invoke || !selected) return;
  goStep(3, { grow: true, focus: !quiet });
  if (!quiet) clearNotice();
  planBody.replaceChildren(
    h("div", { class: "hero pending" }, icon("download"),
      h("div", {}, h("div", { class: "hero-title" }, "Prüfe …"),
        h("div", { class: "hero-text" }, "Beim ersten Mal dauert das bei großen Instanzen etwas.")))
  );
  if (!keepResult) hideResult();
  undoBtn.classList.add("hidden");
  lastPlan = null;
  busy = true;
  refreshButtons();
  setStatus("");
  const baseUrl = baseUrlInput.value.trim();
  try {
    const view = await invoke("plan_update", { instance: selected.path, baseUrl });
    lastPlan = view;
    confirmedInstance = false;
    rememberInstance(selected);
    showCompat(view.compat);
    renderPlan(view);
    await checkLoader(view);
  } catch (e) {
    const r = asReport(e);
    planBody.replaceChildren();
    if (r.kind === "instance") {
      maxStep = 2;
      goStep(2);
    }
    showError(r, { retry: () => doCheck() });
  } finally {
    busy = false;
    refreshButtons();
  }
}

async function doApply() {
  if (!invoke || !lastPlan) return;
  clearNotice();
  hideResult();
  busy = true;
  work = "apply";
  phase = null;
  refreshButtons();
  renderStepper();
  setStatus("");
  showProgress(true);
  let res;
  try {
    res = await invoke("apply_update", {
      instance: selected.path,
      baseUrl: baseUrlInput.value.trim(),
      manifestId: lastPlan.manifestId,
      removeExtras: [...removeExtras],
      keepCollisions: [...keepCollisions],
    });
  } catch (e) {
    const failedIn = phase;
    work = null;
    busy = false;
    showProgress(false);
    renderStepper();
    const r = asReport(e);
    if (r.kind === "stale") {
      // The server published something new while the plan was open.
      await doCheck({ quiet: true });
      showNotice({
        tone: "info",
        title: "Der Server-Stand hat sich gerade geändert",
        text: "Die neuen Änderungen stehen unten – bitte noch einmal ansehen und dann aktualisieren.",
      });
    } else {
      const changed = failedIn === "apply" ? !/alle Änderungen wurden zurückgenommen/.test(r.detail) : false;
      showError(r, { retry: doApply, changed });
      refreshButtons();
    }
    afterWork();
    return;
  }
  work = null;
  busy = false;
  if (closeAfterWork) return afterWork(); // closed during the update: done, quit now
  showProgress(false);
  renderStepper();
  const parts = [count(res.installed, "Datei installiert", "Dateien installiert")];
  if (res.replaced) parts.push(count(res.replaced, "alte Version", "alte Versionen") + " ersetzt");
  if (res.deleted) parts.push(`${res.deleted} entfernt`);
  if (res.seeded) parts.push(count(res.seeded, "Einstellungsdatei", "Einstellungsdateien") + " angelegt");
  showResult([
    h("div", { class: "result-title" }, "✓ Update abgeschlossen"),
    h("p", {}, parts.join(", ") + (res.reused ? ` (${res.downloaded} geladen, ${res.reused} wiederverwendet).` : ".")),
    res.serverAdded ? h("p", {}, "Der Server steht jetzt in deiner Mehrspieler-Liste.") : null,
    ...res.warnings.map((w) => h("p", { class: "warn-line" }, "⚠ " + w)),
    h("p", { class: "start-hint" }, startHint()),
    res.backupDir
      ? h("div", { class: "result-actions" }, folderButton("Backup-Ordner öffnen", res.backupDir), folderButton("Instanz-Ordner öffnen", selected.path))
      : null,
  ]);
  await doCheck({ keepResult: true, quiet: true });
  afterWork();
}

let undoArmed = null;
function undoLabel(restorable) {
  const when = restorable ? formatWhen(restorable.timestamp, SHORT_DATE_OPTS) : "";
  return when ? `Update vom ${when} rückgängig` : "Letztes Update rückgängig";
}

function disarmUndo(restorable = lastPlan?.restorable) {
  clearTimeout(undoArmed);
  undoArmed = null;
  undoBtn.textContent = undoLabel(restorable);
  undoBtn.title = restorable
    ? `Stellt den Stand vor dem Update vom ${formatWhen(restorable.timestamp)} wieder her (${count(restorable.files, "Datei", "Dateien")}).`
    : "";
  undoBtn.classList.remove("danger");
}

// Two clicks instead of a confirm() dialog (not available in every webview).
async function doUndo() {
  if (!undoArmed) {
    undoBtn.textContent = "Wirklich rückgängig machen?";
    undoBtn.classList.add("danger");
    undoArmed = setTimeout(() => disarmUndo(), 5000);
    return;
  }
  disarmUndo();
  clearNotice();
  busy = true;
  work = "undo";
  refreshButtons();
  renderStepper();
  setStatus("Mache das letzte Update rückgängig …", "busy");
  try {
    const r = await invoke("undo_update", { instance: selected.path });
    showResult([
      h("div", { class: "result-title" }, "↶ Rückgängig gemacht"),
      h("p", {}, `${count(r.restored, "Datei", "Dateien")} wiederhergestellt, ${count(r.removed, "Datei", "Dateien")} des Updates beiseitegelegt. ` +
        "Beim nächsten Prüfen wird das Update wieder angeboten."),
      h("div", { class: "result-actions" }, folderButton("Beiseitegelegte Dateien öffnen", r.backupDir)),
    ]);
    setStatus("");
  } catch (e) {
    showError(e, { retry: doUndo });
    return;
  } finally {
    work = null;
    busy = false;
    refreshButtons();
    renderStepper();
    afterWork();
  }
  await doCheck({ keepResult: true, quiet: true });
}

// --- closing while working ----------------------------------------------------------

function hideCloseDialog() {
  closeDialog.classList.add("hidden");
}

/** Called after every long-running command: quit if the window was closed meanwhile. */
function afterWork() {
  if (closeAfterWork && !work) invoke("quit_app");
}

function onCloseRequested() {
  if (!work) {
    invoke("quit_app");
    return;
  }
  const stay = el("closeStay");
  const go = el("closeCancel");
  const downloading = work === "apply" && phase !== "apply" && phase !== "done";
  if (downloading) {
    el("closeTitle").textContent = "Das Update läuft noch";
    el("closeText").textContent =
      "Schließen bricht den Download ab. Es wird nichts verändert; bereits geladene Dateien werden beim nächsten Mal wiederverwendet.";
    go.textContent = "Abbrechen und schließen";
    go.classList.remove("hidden");
  } else {
    closeAfterWork = true;
    el("closeTitle").textContent = "Gleich fertig";
    el("closeText").textContent =
      work === "appupdate"
        ? "Bonegrader installiert gerade seine neue Version und startet danach neu."
        : "Bonegrader schreibt gerade Änderungen in deine Instanz. Das Fenster schließt sich danach von selbst.";
    go.classList.add("hidden");
  }
  stay.textContent = downloading ? "Weiterlaufen lassen" : "Doch nicht schließen";
  closeDialog.classList.remove("hidden");
  stay.focus();
}

// --- startup ----------------------------------------------------------------------

function wire() {
  baseUrlInput.addEventListener("input", () => {
    serverInfo = null;
    renderServer();
    showCompat(null);
    invalidatePlan(1);
  });
  baseUrlInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") checkServer();
  });
  el("resetUrl").addEventListener("click", () => {
    baseUrlInput.value = DEFAULT_URL;
    baseUrlInput.dispatchEvent(new Event("input"));
  });
  next1.addEventListener("click", checkServer);
  checkBtn.addEventListener("click", () => doCheck());
  recheckBtn.addEventListener("click", () => doCheck({ keepResult: false }));
  applyBtn.addEventListener("click", doApply);
  undoBtn.addEventListener("click", doUndo);
  cancelBtn.addEventListener("click", cancelUpdate);
  el("pickFolder").addEventListener("click", pickFolder);
  el("createInstance").addEventListener("click", createInstance);
  const useManual = () => {
    const p = el("manualPath").value.trim();
    if (p) selectInstance({ name: baseName(p), path: p, launcher: "Manual" });
  };
  el("useManual").addEventListener("click", useManual);
  el("manualPath").addEventListener("keydown", (e) => {
    if (e.key === "Enter") useManual();
  });
  stepDots.forEach((dot) => dot.addEventListener("click", () => goStep(Number(dot.dataset.step))));
  document.querySelectorAll("[data-goto]").forEach((btn) =>
    btn.addEventListener("click", () => goStep(Number(btn.dataset.goto)))
  );
  el("closeStay").addEventListener("click", () => {
    closeAfterWork = false;
    hideCloseDialog();
  });
  el("closeCancel").addEventListener("click", () => {
    closeAfterWork = true;
    hideCloseDialog();
    cancelUpdate();
  });
  closeDialog.addEventListener("keydown", (e) => {
    if (e.key === "Escape") el("closeStay").click();
  });
}

async function init() {
  wire();
  const storedUrl = load(STORE_URL);
  baseUrlInput.value = storedUrl || DEFAULT_URL;
  renderServer();
  refreshButtons();
  goStep(1, { focus: false });

  if (!invoke) {
    instancesBox.replaceChildren(h("p", { class: "muted" }, "Vorschau-Modus (kein Tauri) — Erkennung inaktiv."));
    return;
  }
  invoke("app_version")
    .then((v) => (el("version").textContent = "v" + v))
    .catch(() => {});
  if (listen) {
    await listen("update-progress", (e) => updateProgress(e.payload));
    await listen("close-requested", onCloseRequested);
    await listen("app-update-progress", (e) => {
      if (work !== "appupdate") return;
      appProgress = e.payload;
      renderBanner();
    });
  }
  checkAppUpdate(); // in the background; shows a banner if there is one
  const detecting = invoke("detect_instances")
    .then((list) => (instances = list))
    .catch((e) => showError(e));

  // Returning player (server and instance known): check right away, so the
  // first screen already says "up to date" or "N changes — Aktualisieren".
  const remembered = rememberedInstance();
  if (!storedUrl || !remembered) {
    await detecting;
    renderInstances();
    return;
  }
  maxStep = 3;
  goStep(3, { focus: false });
  planBody.replaceChildren(
    h("div", { class: "hero pending" }, icon("download"),
      h("div", {}, h("div", { class: "hero-title" }, "Prüfe, ob alles aktuell ist …"),
        h("div", { class: "hero-text" }, `Instanz „${remembered.name}“`)))
  );
  busy = true;
  refreshButtons();
  try {
    serverInfo = await invoke("manifest_info", { baseUrl: storedUrl });
  } catch (e) {
    busy = false;
    maxStep = 1;
    goStep(1, { focus: false });
    planBody.replaceChildren();
    showError(e, { retry: checkServer });
    await detecting;
    renderInstances();
    refreshButtons();
    return;
  }
  renderServer();
  showCompat(serverInfo.compat);
  await detecting;
  selected =
    instances.find((i) => i.path === remembered.path && (!remembered.profileKey || i.profileKey === remembered.profileKey)) ||
    remembered;
  if (!instances.some((i) => sameInstance(i, selected))) instances.push(selected);
  renderInstances();
  renderContext();
  busy = false;
  await doCheck({ quiet: true });
}

init();
