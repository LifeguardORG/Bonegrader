// Bonegrader frontend. Talks to the Rust core via the global Tauri bridge
// (enabled by `withGlobalTauri: true`). No bundler, no npm imports.
//
// All dynamic text is inserted with textContent (see `h`) — never as HTML —
// so names from launcher files or the server manifest cannot inject markup.

const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const DEFAULT_URL = "https://bonegrader.rescue-compete.de/main";
const STORE_URL = "bonegrader.baseUrl";
const STORE_INSTANCE = "bonegrader.lastInstance";
const STALE_PREFIX = "[stale] "; // see app/src-tauri/src/main.rs

const el = (id) => document.getElementById(id);
const baseUrlInput = el("baseUrl");
const serverInfoBox = el("serverInfo");
const instancesBox = el("instances");
const selectedLabel = el("selected");
const checkBtn = el("checkBtn");
const planBody = el("planBody");
const applyBtn = el("applyBtn");
const undoBtn = el("undoBtn");
const resultBox = el("result");
const statusLine = el("status");
const banner = el("banner");
const progressBox = el("progress");
const progressBar = el("bar");
const progressText = el("progressText");
const next1 = el("next1");
const stepDots = [...el("stepper").querySelectorAll(".step-dot")];
const panels = [...document.querySelectorAll(".panel")];

let serverInfo = null; // manifest_info for the current URL
let instances = []; // detected + manually added
let selected = null; // { path, name, launcher, ... }
let lastPlan = null; // plan view of the last check
let confirmedInstance = false; // player confirmed a suspicious instance
let busy = false;
let currentStep = 1; // step shown on the right
let maxStep = 1; // furthest step reached so far (controls what's clickable)

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

/** "1 Datei" / "3 Dateien". */
const count = (n, one, many) => `${n} ${n === 1 ? one : many}`;

function humanBytes(n) {
  if (n < 1024) return n + " B";
  if (n < 1024 * 1024) return (n / 1024).toFixed(0) + " KB";
  if (n < 1024 * 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + " MB";
  return (n / 1024 / 1024 / 1024).toFixed(2) + " GB";
}

const LOADER_NAMES = { neoforge: "NeoForge", forge: "Forge", fabric: "Fabric", quilt: "Quilt" };
const loaderName = (t) => LOADER_NAMES[t] || t || "?";
const LAUNCHER_NAMES = { CurseForge: "CurseForge", Prism: "Prism", Vanilla: "Offizieller Launcher", Manual: "Ordner" };
const launcherName = (l) => LAUNCHER_NAMES[l] || l || "Ordner";

/** "20260801-120000" (UTC) -> "01.08.2026, 12:00 UTC". */
function formatStamp(ts) {
  const m = /^(\d{4})(\d{2})(\d{2})-(\d{2})(\d{2})/.exec(ts || "");
  return m ? `${m[3]}.${m[2]}.${m[1]}, ${m[4]}:${m[5]} UTC` : ts;
}

function setStatus(msg, kind = "") {
  statusLine.textContent = msg;
  statusLine.className = "status " + kind;
}

function refreshButtons() {
  const hasUrl = !!baseUrlInput.value.trim();
  next1.disabled = !hasUrl || busy;
  checkBtn.disabled = !(selected && hasUrl) || busy;
  const blocked = lastPlan?.compat?.state === "updateRequired";
  const unconfirmed = lastPlan?.assessment?.suspicious && !confirmedInstance;
  const nothing = !lastPlan || (lastPlan.noop && removeExtras.size === 0);
  applyBtn.disabled = busy || blocked || unconfirmed || nothing;
  undoBtn.disabled = busy;
}

// --- wizard step machine ---------------------------------------------------------

function renderStepper() {
  stepDots.forEach((dot) => {
    const n = Number(dot.dataset.step);
    const reachable = n <= maxStep;
    dot.classList.toggle("active", n === currentStep);
    dot.classList.toggle("done", reachable && n !== currentStep);
    dot.disabled = !reachable;
  });
}

// Show step `n`. `grow` unlocks it (and marks it reachable) when advancing.
function goStep(n, { grow = false } = {}) {
  if (n < 1 || n > panels.length) return;
  if (grow) maxStep = Math.max(maxStep, n);
  if (n > maxStep) return;
  currentStep = n;
  panels.forEach((p) => p.classList.toggle("active", Number(p.dataset.step) === n));
  renderStepper();
}

/** A changed URL or instance invalidates the reviewed plan. */
function invalidatePlan(toStep) {
  lastPlan = null;
  maxStep = Math.min(maxStep, toStep);
  renderStepper();
  refreshButtons();
}

// --- update hints ----------------------------------------------------------------

function showCompat(compat) {
  banner.replaceChildren();
  banner.className = "banner hidden";
  if (!compat || compat.state === "current") return;
  const required = compat.state === "updateRequired";
  banner.append(
    required
      ? `Diese Bonegrader-Version ist zu alt für den Server – bitte auf ${compat.min} oder neuer aktualisieren. `
      : `Bonegrader ${compat.latest} ist verfügbar. `
  );
  if (compat.downloadUrl) {
    banner.append(
      h(
        "a",
        {
          href: "#",
          onclick: (e) => {
            e.preventDefault();
            invoke("open_url", { url: compat.downloadUrl }).catch((err) => setStatus("Link: " + err, "err"));
          },
        },
        "Zum Download"
      )
    );
  }
  banner.className = "banner " + (required ? "err" : "info");
}

// --- step 1: server ----------------------------------------------------------------

function renderServerInfo() {
  serverInfoBox.replaceChildren();
  serverInfoBox.classList.toggle("hidden", !serverInfo);
  if (!serverInfo) return;
  const l = serverInfo.loader;
  put(
    serverInfoBox,
    h("div", { class: "name" }, serverInfo.packName),
    h(
      "div",
      { class: "meta" },
      `${loaderName(l.type)} ${l.loaderVersion} · Minecraft ${l.mcVersion} · ${serverInfo.files} Dateien`
    ),
    serverInfo.signature === "verified" ? h("div", { class: "signed" }, "✓ Signatur des Servers geprüft") : null
  );
}

async function checkServer() {
  const baseUrl = baseUrlInput.value.trim();
  if (!baseUrl || !invoke) return;
  busy = true;
  refreshButtons();
  setStatus("Verbinde mit dem Server …", "busy");
  try {
    serverInfo = await invoke("manifest_info", { baseUrl });
    store(STORE_URL, baseUrl);
    renderServerInfo();
    showCompat(serverInfo.compat);
    setStatus("");
    goStep(2, { grow: true });
    renderInstances();
  } catch (e) {
    serverInfo = null;
    renderServerInfo();
    setStatus("Server nicht erreichbar oder ungültig: " + e, "err");
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
  return inst.path;
}

const sameInstance = (a, b) => a && b && a.path === b.path && (a.profileKey || null) === (b.profileKey || null);

function renderInstances() {
  const list = [...instances].sort((a, b) => matchScore(b) - matchScore(a) || a.name.localeCompare(b.name));
  instancesBox.replaceChildren();
  instancesBox.classList.toggle("scroll", list.length > 3);
  if (!list.length) {
    instancesBox.append(
      h("p", { class: "muted" }, "Keine Instanz gefunden – Ordner wählen oder eine eigene Instanz anlegen.")
    );
  }
  for (const inst of list) {
    const warn = mismatch(inst);
    const node = h(
      "div",
      { class: "instance" + (sameInstance(inst, selected) ? " active" : ""), role: "button", tabindex: "0" },
      h(
        "div",
        { class: "instance-text" },
        h("div", { class: "name" }, inst.name),
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
  // Pre-select the instance used last time, else only a clear match — never
  // just "the first one" (that might be a different modpack).
  if (!selected && list.length) {
    const last = load(STORE_INSTANCE);
    const pick = list.find((i) => i.path === last) || (matchScore(list[0]) >= 4 ? list[0] : null);
    if (pick) selectInstance(pick);
  }
}

function selectInstance(inst) {
  if (!sameInstance(inst, selected)) invalidatePlan(2);
  selected = inst;
  confirmedInstance = false;
  if (!instances.some((i) => sameInstance(i, inst))) instances.push(inst);
  selectedLabel.textContent = "Gewählt: " + inst.name + "  —  " + inst.path;
  renderInstances();
  refreshButtons();
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
    if (fresh) {
      selected = fresh;
      selectedLabel.textContent = "Gewählt: " + fresh.name + "  —  " + fresh.path;
    }
  }
  renderInstances();
}

async function pickFolder() {
  try {
    const path = await invoke("pick_folder");
    if (path) selectInstance({ name: baseName(path), path, launcher: "Manual" });
  } catch (e) {
    setStatus("Ordnerauswahl fehlgeschlagen: " + e, "err");
  }
}

async function createInstance() {
  const packName = serverInfo?.packName || "BonesAndBees";
  try {
    const inst = await invoke("create_instance", { packName });
    selectInstance(inst);
    setStatus("Eigener Ordner angelegt: " + inst.path + " – nach dem Prüfen kannst du NeoForge dafür einrichten.", "ok");
  } catch (e) {
    setStatus("Instanz konnte nicht angelegt werden: " + e, "err");
  }
}

// --- step 3: plan ----------------------------------------------------------------

function row(tagClass, tagText, text, control, note) {
  return h(
    "div",
    { class: "plan-row" },
    h("span", { class: "tag " + tagClass }, tagText),
    h("code", {}, text),
    note ? h("span", { class: "muted small" }, note) : null,
    control
  );
}

function toggle(labelText, onChange) {
  const cb = h("input", { type: "checkbox" });
  cb.addEventListener("change", () => {
    onChange(cb.checked);
    refreshButtons();
  });
  return h("label", {}, cb, labelText);
}

function group(title, rows) {
  if (!rows.length) return;
  planBody.append(h("div", { class: "plan-group" }, h("h3", {}, `${title} (${rows.length})`), rows));
}

function renderPlan(view) {
  removeExtras.clear();
  keepCollisions.clear();
  planBody.replaceChildren();
  const plan = view.plan;
  const a = view.assessment;

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
      h(
        "div",
        { class: "plan-group warn-box" },
        h("h3", {}, "Ist das die richtige Instanz?"),
        h("p", { class: "muted" }, msg),
        h("label", { class: "confirm" }, cb, "Ja, das ist die richtige Instanz")
      )
    );
  }

  if (view.noop) {
    planBody.append(h("p", { class: "muted" }, "Alles aktuell — nichts zu tun."));
  } else if (view.downloadBytes > 0) {
    const n = plan.downloads.length + view.seeds.length;
    planBody.append(
      h("p", { class: "muted summary" }, `${count(n, "Datei", "Dateien")} · ${humanBytes(view.downloadBytes)} zu laden`)
    );
  }

  const installs = plan.downloads.filter((d) => !d.replaces);
  const updates = plan.downloads.filter((d) => d.replaces);
  group("Neu installieren", installs.map((d) => row("add", "neu", d.entry.path)));
  group(
    "Aktualisieren",
    updates.map((d) =>
      row("upd", "update", d.entry.path, null, d.replaces !== d.entry.path ? "ersetzt " + baseName(d.replaces) : null)
    )
  );
  group("Entfernen", plan.removals.map((p) => row("rem", "entfernen", p)));
  group(
    "Doppelte Dateien (identische Kopie – wird entfernt)",
    plan.duplicates.map((p) => row("rem", "doppelt", p))
  );
  group(
    "Konflikte (gleiche modId)",
    plan.collisions.map((c) =>
      row(
        "clash",
        "Konflikt",
        c.localPath + "  ⟷  " + c.manifestPath,
        toggle("meine Version behalten", (keep) =>
          keep ? keepCollisions.add(c.localPath) : keepCollisions.delete(c.localPath)
        )
      )
    )
  );
  group("Standard-Einstellungen (nur weil sie fehlen)", view.seeds.map((p) => row("add", "neu", p)));
  if (view.server) {
    group("Serverliste", [row("add", "neu", `${view.server.name} (${view.server.address})`)]);
  }
  group(
    "Deine zusätzlichen Mods (bleiben erhalten)",
    plan.userExtras.map((p) =>
      row("add", "eigen", p, toggle("entfernen", (rm) => (rm ? removeExtras.add(p) : removeExtras.delete(p))))
    )
  );

  undoBtn.classList.toggle("hidden", !view.restorable);
  if (view.restorable) {
    undoBtn.title = "Update vom " + formatStamp(view.restorable.timestamp) + " zurücknehmen";
  }
}

// Compare the selected instance's loader against the pack's required loader.
// Runs for every launcher type: the vanilla + NeoForge path can be installed or
// repaired from here; other launchers only get a warning so the player can fix
// the version in their own launcher.
async function checkLoader(baseUrl) {
  let st;
  try {
    st = await invoke("loader_status", {
      baseUrl,
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
  const box = h("div", { class: "plan-group loader-needed" });

  if (st.canInstall) {
    const name = selected.name || "BonesAndBees";
    put(
      box,
      h("h3", {}, st.state === "mismatch" ? "Falsche NeoForge-Version" : "NeoForge wird benötigt"),
      st.state === "mismatch"
        ? h("p", { class: "muted" }, "Diese Instanz nutzt ", h("b", {}, have), ", der Server braucht aber ",
            h("b", {}, "NeoForge " + st.requiredVersion), ` (MC ${st.mcVersion}).`)
        : h("p", { class: "muted" }, "Für diese Instanz fehlt der Mod-Loader ", h("b", {}, "NeoForge " + st.requiredVersion),
            ` (MC ${st.mcVersion}).`),
      st.javaAvailable
        ? null
        : h("p", { class: "warn-line" }, "Achtung: Kein Java gefunden — starte Minecraft einmal oder installiere Java, dann erneut prüfen."),
      h("p", { class: "muted" }, "Danach im offiziellen Launcher das Profil ", h("b", {}, name),
        ` (NeoForge ${st.requiredVersion}) auswählen — sonst startet weiter Vanilla und der Server-Beitritt schlägt fehl.`)
    );
    const btn = h("button", { type: "button" }, st.state === "mismatch" ? "NeoForge korrigieren" : "NeoForge installieren");
    btn.disabled = !st.javaAvailable;
    btn.addEventListener("click", async () => {
      setStatus("Richte NeoForge ein … (kann etwas dauern)", "busy");
      btn.disabled = true;
      try {
        await invoke("install_loader", {
          baseUrl,
          gameDir: selected.path,
          packName: name,
          profileKey: selected.profileKey || null,
        });
        setStatus(
          `NeoForge ${st.requiredVersion} eingerichtet — im offiziellen Launcher das Profil „${name}“ starten.`,
          "ok"
        );
        box.remove();
        await refreshInstances();
      } catch (e) {
        setStatus("Fehler bei der Installation: " + e, "err");
        btn.disabled = false;
      }
    });
    box.append(btn);
  } else {
    put(
      box,
      h("h3", {}, st.state === "unknown" ? "Loader-Version prüfen" : "Falsche Loader-Version"),
      h(
        "p",
        { class: "muted" },
        have
          ? ["Diese Instanz nutzt ", h("b", {}, have), ", der Server braucht ", h("b", {}, req), ` (MC ${st.mcVersion}). `]
          : ["Der Server braucht ", h("b", {}, req), ` (MC ${st.mcVersion}). Die Loader-Version dieser Instanz ist unbekannt — bitte prüfen. `],
        "Bitte im Launcher genau diese Version einstellen — sonst schlägt der Server-Beitritt fehl."
      )
    );
  }
  planBody.prepend(box);
}

function showProgress(visible) {
  progressBox.classList.toggle("hidden", !visible);
  if (visible) {
    progressBar.style.width = "0%";
    progressText.textContent = "";
  }
}

function updateProgress(p) {
  if (!p) return;
  let pct = 0;
  if (p.totalBytes > 0) pct = (p.doneBytes / p.totalBytes) * 100;
  else if (p.totalFiles > 0) pct = (p.doneFiles / p.totalFiles) * 100;
  if (p.phase === "apply" || p.phase === "done") pct = 100;
  progressBar.style.width = pct.toFixed(1) + "%";

  if (p.phase === "download") {
    const tail = p.current ? " · " + baseName(p.current) : "";
    progressText.textContent = `Lade ${p.doneFiles}/${p.totalFiles} · ${humanBytes(p.doneBytes)} / ${humanBytes(p.totalBytes)}${tail}`;
  } else if (p.phase === "apply") {
    progressText.textContent = "Wende Änderungen an …";
  } else if (p.phase === "done") {
    progressText.textContent = "Fertig.";
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

async function doCheck({ keepResult = false } = {}) {
  if (!invoke || !selected) return;
  goStep(3, { grow: true });
  planBody.replaceChildren();
  if (!keepResult) hideResult();
  undoBtn.classList.add("hidden");
  lastPlan = null;
  busy = true;
  refreshButtons();
  setStatus("Prüfe … (beim ersten Mal dauert das bei großen Instanzen etwas)", "busy");
  const baseUrl = baseUrlInput.value.trim();
  try {
    const view = await invoke("plan_update", { instance: selected.path, baseUrl });
    lastPlan = view;
    confirmedInstance = false;
    store(STORE_INSTANCE, selected.path);
    showCompat(view.compat);
    renderPlan(view);
    await checkLoader(baseUrl);
    setStatus(view.noop ? "Alles aktuell." : "Prüfung abgeschlossen – bitte die Änderungen ansehen.", "ok");
  } catch (e) {
    setStatus("Fehler: " + e, "err");
  } finally {
    busy = false;
    refreshButtons();
  }
}

async function doApply() {
  if (!invoke || !lastPlan) return;
  busy = true;
  refreshButtons();
  hideResult();
  setStatus("Aktualisiere … (Fenster offen lassen)", "busy");
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
    showProgress(false);
    busy = false;
    const msg = String(e);
    if (msg.startsWith(STALE_PREFIX)) {
      // The server published something new while the plan was open: show the new plan.
      await doCheck();
      setStatus(msg.slice(STALE_PREFIX.length) + " Die neuen Änderungen werden unten angezeigt.", "busy");
    } else {
      setStatus("Fehler: " + msg, "err");
      refreshButtons();
    }
    return;
  }
  showProgress(false);
  busy = false;
  const parts = [`${res.installed} installiert`];
  if (res.reused) parts[0] += ` (${res.downloaded} geladen, ${res.reused} wiederverwendet)`;
  if (res.replaced) parts.push(count(res.replaced, "alte Version", "alte Versionen") + " ersetzt");
  if (res.deleted) parts.push(`${res.deleted} entfernt`);
  if (res.seeded) parts.push(count(res.seeded, "Einstellungsdatei", "Einstellungsdateien") + " angelegt");
  showResult([
    h("div", { class: "result-title" }, "✓ Update abgeschlossen: " + parts.join(", ") + "."),
    res.serverAdded ? h("div", {}, "Der Server steht jetzt in deiner Mehrspieler-Liste.") : null,
    res.backupDir ? h("div", { class: "muted small" }, "Ersetzte/entfernte Dateien: " + res.backupDir) : null,
    ...res.warnings.map((w) => h("div", { class: "warn-line small" }, "⚠ " + w)),
  ]);
  await doCheck({ keepResult: true });
}

let undoArmed = null;
function disarmUndo() {
  clearTimeout(undoArmed);
  undoArmed = null;
  undoBtn.textContent = "Letztes Update rückgängig";
  undoBtn.classList.remove("danger");
}

// Two clicks instead of a confirm() dialog (not available in every webview).
async function doUndo() {
  if (!undoArmed) {
    undoBtn.textContent = "Wirklich rückgängig machen?";
    undoBtn.classList.add("danger");
    undoArmed = setTimeout(disarmUndo, 5000);
    return;
  }
  disarmUndo();
  busy = true;
  refreshButtons();
  setStatus("Mache das letzte Update rückgängig …", "busy");
  try {
    const r = await invoke("undo_update", { instance: selected.path });
    showResult([
      h("div", { class: "result-title" },
        `↶ Rückgängig gemacht: ${count(r.restored, "Datei", "Dateien")} wiederhergestellt, ` +
          `${count(r.removed, "Datei", "Dateien")} des Updates beiseitegelegt.`),
      h("div", { class: "muted small" }, "Beim nächsten Prüfen wird das Update wieder angeboten. Beiseitegelegt in: " + r.backupDir),
    ]);
  } catch (e) {
    busy = false;
    setStatus("Fehler: " + e, "err");
    refreshButtons();
    return;
  }
  busy = false;
  await doCheck({ keepResult: true });
}

// --- startup ----------------------------------------------------------------------

async function init() {
  baseUrlInput.value = load(STORE_URL) || DEFAULT_URL;
  baseUrlInput.addEventListener("input", () => {
    serverInfo = null;
    renderServerInfo();
    showCompat(null);
    invalidatePlan(1);
  });
  baseUrlInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") checkServer();
  });
  next1.addEventListener("click", checkServer);
  checkBtn.addEventListener("click", () => doCheck());
  applyBtn.addEventListener("click", doApply);
  undoBtn.addEventListener("click", doUndo);
  el("pickFolder").addEventListener("click", pickFolder);
  el("createInstance").addEventListener("click", createInstance);
  el("useManual").addEventListener("click", () => {
    const p = el("manualPath").value.trim();
    if (p) selectInstance({ name: baseName(p), path: p, launcher: "Manual" });
  });
  stepDots.forEach((dot) => dot.addEventListener("click", () => goStep(Number(dot.dataset.step))));
  document.querySelectorAll("[data-goto]").forEach((btn) =>
    btn.addEventListener("click", () => goStep(Number(btn.dataset.goto)))
  );

  refreshButtons();
  goStep(1);

  if (!invoke) {
    instancesBox.replaceChildren(h("p", { class: "muted" }, "Vorschau-Modus (kein Tauri) — Erkennung inaktiv."));
    return;
  }
  invoke("app_version")
    .then((v) => (el("version").textContent = "v" + v))
    .catch(() => {});
  if (listen) await listen("update-progress", (e) => updateProgress(e.payload));
  try {
    instances = await invoke("detect_instances");
  } catch (e) {
    setStatus("Erkennung fehlgeschlagen: " + e, "err");
  }
  renderInstances();
}

init();
