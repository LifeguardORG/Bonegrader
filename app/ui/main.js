// Bonegrader frontend. Talks to the Rust core via the global Tauri bridge
// (enabled by `withGlobalTauri: true`). No bundler, no npm imports.

const invoke = window.__TAURI__?.core?.invoke;

const el = (id) => document.getElementById(id);
const baseUrlInput = el("baseUrl");
const instancesBox = el("instances");
const selectedLabel = el("selected");
const checkBtn = el("checkBtn");
const planCard = el("planCard");
const planBody = el("planBody");
const applyBtn = el("applyBtn");
const statusLine = el("status");
const progressBox = el("progress");
const progressBar = el("bar");
const progressText = el("progressText");

const stepper = el("stepper");
const stepDots = [...stepper.querySelectorAll(".step-dot")];
const panels = [...document.querySelectorAll(".panel")];
const next1 = el("next1");

const listen = window.__TAURI__?.event?.listen;

let selected = null; // { path, name }
let lastPlan = null;
let currentStep = 1; // step shown on the right
let maxStep = 1; // furthest step reached so far (controls what's clickable)

function setStatus(msg, kind = "") {
  statusLine.textContent = msg;
  statusLine.className = "status " + kind;
}

function refreshCheckEnabled() {
  const hasUrl = !!baseUrlInput.value.trim();
  next1.disabled = !hasUrl;
  checkBtn.disabled = !(selected && hasUrl);
}

// --- Wizard step machine ---------------------------------------------------
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

function humanBytes(n) {
  if (n < 1024) return n + " B";
  if (n < 1024 * 1024) return (n / 1024).toFixed(0) + " KB";
  return (n / 1024 / 1024).toFixed(1) + " MB";
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
    const tail = p.current ? " · " + p.current.split("/").pop() : "";
    progressText.textContent =
      `Lade ${p.doneFiles}/${p.totalFiles} · ${humanBytes(p.doneBytes)} / ${humanBytes(p.totalBytes)}${tail}`;
  } else if (p.phase === "apply") {
    progressText.textContent = "Wende Änderungen an …";
  } else if (p.phase === "done") {
    progressText.textContent = "Fertig.";
  }
}

function selectInstance(inst, node) {
  selected = inst;
  document.querySelectorAll(".instance.active").forEach((n) => n.classList.remove("active"));
  if (node) node.classList.add("active");
  selectedLabel.textContent = "Gewählt: " + inst.name + "  —  " + inst.path;
  refreshCheckEnabled();
}

function renderInstances(list) {
  instancesBox.innerHTML = "";
  instancesBox.classList.toggle("scroll", list.length > 3);
  if (!list.length) {
    instancesBox.innerHTML = '<p class="muted">Keine Instanz automatisch gefunden — bitte manuell angeben.</p>';
    return;
  }
  let firstNode = null;
  for (const inst of list) {
    const node = document.createElement("div");
    node.className = "instance";
    const loaderStr = [inst.loaderType, inst.loaderVersion].filter(Boolean).join(" ");
    const mc = inst.mcVersion ? "MC " + inst.mcVersion : "";
    let meta;
    if (loaderStr) meta = [loaderStr, mc].filter(Boolean).join(" · ");
    else if (mc) meta = mc + " · NeoForge nicht installiert";
    else meta = "Loader unbekannt";
    node.innerHTML =
      '<div><div class="name"></div><div class="meta"></div></div>' +
      '<span class="badge"></span>';
    node.querySelector(".name").textContent = inst.name;
    node.querySelector(".meta").textContent = meta;
    node.querySelector(".badge").textContent = inst.launcher;
    node.addEventListener("click", () => selectInstance(inst, node));
    instancesBox.appendChild(node);
    if (!firstNode) firstNode = node;
  }
  // Pre-select the first instance so "Prüfen" is ready (unless one is already chosen).
  if (!selected && firstNode) selectInstance(list[0], firstNode);
}

// Re-detect instances and re-render the list, keeping the current selection
// bound to its freshly detected data — so its loader/version reflect reality
// after an install, not just the values read at startup.
async function refreshInstances() {
  let list = [];
  try {
    list = await invoke("detect_instances");
  } catch {
    return;
  }
  renderInstances(list);
  if (!selected) return;
  const idx = list.findIndex((i) =>
    (selected.profileKey && i.profileKey === selected.profileKey) ||
    (i.path === selected.path && i.name === selected.name));
  if (idx < 0) return;
  const nodes = instancesBox.querySelectorAll(".instance");
  selectInstance(list[idx], nodes[idx] || null);
}

function row(tagClass, tagText, text, control) {
  const r = document.createElement("div");
  r.className = "plan-row";
  const tag = document.createElement("span");
  tag.className = "tag " + tagClass;
  tag.textContent = tagText;
  const code = document.createElement("code");
  code.textContent = text;
  r.append(tag, code);
  if (control) r.appendChild(control);
  return r;
}

function toggle(labelText, checked, onChange) {
  const label = document.createElement("label");
  const cb = document.createElement("input");
  cb.type = "checkbox";
  cb.checked = checked;
  cb.addEventListener("change", () => onChange(cb.checked));
  label.append(cb, document.createTextNode(labelText));
  return label;
}

// Decisions the user makes on the plan.
const removeExtras = new Set();
const keepCollisions = new Set();

function renderPlan(plan) {
  lastPlan = plan;
  removeExtras.clear();
  keepCollisions.clear();
  planBody.innerHTML = "";

  const group = (title, rows) => {
    if (!rows.length) return;
    const g = document.createElement("div");
    g.className = "plan-group";
    const h = document.createElement("h3");
    h.textContent = title;
    g.appendChild(h);
    rows.forEach((r) => g.appendChild(r));
    planBody.appendChild(g);
  };

  const installs = plan.downloads.filter((d) => !d.replaces);
  const updates = plan.downloads.filter((d) => d.replaces);

  group("Neu installieren", installs.map((d) => row("add", "neu", d.entry.path)));
  group("Aktualisieren", updates.map((d) => row("upd", "update", d.entry.path)));
  group("Entfernen", plan.removals.map((p) => row("rem", "entfernen", p)));

  group(
    "Konflikte (gleiche modId)",
    plan.collisions.map((c) =>
      row("clash", "Konflikt", c.localPath + "  ⟷  " + c.manifestPath,
        toggle("meine Version behalten", false, (keep) => {
          keep ? keepCollisions.add(c.localPath) : keepCollisions.delete(c.localPath);
        }))
    )
  );

  group(
    "Deine zusätzlichen Mods (bleiben erhalten)",
    plan.userExtras.map((p) =>
      row("add", "eigen", p,
        toggle("entfernen", false, (rm) => {
          rm ? removeExtras.add(p) : removeExtras.delete(p);
        }))
    )
  );

  const nothing = plan.downloads.length === 0 && plan.removals.length === 0 && plan.collisions.length === 0;
  if (nothing) {
    planBody.innerHTML = '<p class="muted">Alles aktuell — nichts zu tun.</p>';
    applyBtn.disabled = true;
  } else {
    applyBtn.disabled = false;
  }
}

async function doCheck() {
  if (!invoke) return setStatus("Läuft nur in der Bonegrader-App (Tauri).", "err");
  // Go straight to the changes step; the check status shows above its card.
  goStep(3, { grow: true });
  planBody.innerHTML = "";
  setStatus("Prüfe …", "busy");
  try {
    const baseUrl = baseUrlInput.value.trim();
    const plan = await invoke("plan_update", { instance: selected.path, baseUrl });
    renderPlan(plan);
    await checkLoader(baseUrl);
    setStatus("Prüfung abgeschlossen.", "ok");
  } catch (e) {
    setStatus("Fehler: " + e, "err");
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
      launcher: selected.launcher || "manual",
      loaderType: selected.loaderType || null,
      loaderVersion: selected.loaderVersion || null,
    });
  } catch {
    return;
  }
  if (st.state === "ok") return;

  const req = st.requiredType + " " + st.requiredVersion;
  const have = st.installedType || st.installedVersion
    ? (st.installedType || "?") + " " + (st.installedVersion || "?")
    : null;

  const box = document.createElement("div");
  box.className = "plan-group loader-needed";

  if (st.canInstall) {
    const headline = st.state === "mismatch" ? "Falsche NeoForge-Version" : "NeoForge wird benötigt";
    const detail =
      st.state === "mismatch"
        ? "Diese Instanz nutzt <b>" + have + "</b>, der Server braucht aber <b>NeoForge " +
          st.requiredVersion + "</b> (MC " + st.mcVersion + ")."
        : "Für diese Instanz fehlt der Mod-Loader <b>NeoForge " + st.requiredVersion +
          "</b> (MC " + st.mcVersion + ").";
    box.innerHTML =
      "<h3>" + headline + "</h3>" +
      '<p class="muted">' + detail + "</p>" +
      (st.javaAvailable ? "" : '<p class="warn-line">Achtung: Kein Java gefunden — starte Minecraft einmal oder installiere Java, dann erneut prüfen.</p>') +
      '<p class="muted">Danach im offiziellen Launcher das Profil <b>' + (selected.name || "BonesAndBees") +
      "</b> (NeoForge " + st.requiredVersion +
      ") auswählen — sonst startet weiter Vanilla und der Server-Beitritt schlägt fehl.</p>" +
      '<button id="installLoaderBtn" class="btn btn-primary">' +
      (st.state === "mismatch" ? "NeoForge korrigieren" : "NeoForge installieren") +
      "</button>";
    planBody.prepend(box);

    const btn = box.querySelector("#installLoaderBtn");
    if (!st.javaAvailable) btn.setAttribute("aria-disabled", "true");
    btn.addEventListener("click", async () => {
      setStatus("Richte NeoForge ein … (kann etwas dauern)", "busy");
      btn.setAttribute("aria-disabled", "true");
      try {
        await invoke("install_loader", {
          baseUrl,
          gameDir: selected.path,
          packName: selected.name || "BonesAndBees",
          profileKey: selected.profileKey || null,
        });
        const name = selected.name || "BonesAndBees";
        setStatus(
          selected.profileKey
            ? "NeoForge " + st.requiredVersion + " eingerichtet — das Profil „" + name +
                "\" nutzt es jetzt. Im offiziellen Launcher einfach „" + name + "\" starten."
            : "NeoForge " + st.requiredVersion + " eingerichtet. Im offiziellen Launcher das NeoForge-Profil „" +
                name + "\" starten.",
          "ok"
        );
        box.remove();
        // Re-detect so the (now NeoForge) profile updates in the list immediately.
        await refreshInstances();
      } catch (e) {
        setStatus("Fehler bei der Installation: " + e, "err");
        btn.removeAttribute("aria-disabled");
      }
    });
  } else {
    // CurseForge / manual / non-NeoForge: Bonegrader can't install here, only warn.
    const headline = st.state === "unknown" ? "Loader-Version prüfen" : "Falsche Loader-Version";
    const line = have
      ? "Diese Instanz nutzt <b>" + have + "</b>, der Server braucht <b>" + req + "</b> (MC " + st.mcVersion + ")."
      : "Der Server braucht <b>" + req + "</b> (MC " + st.mcVersion +
        "). Die Loader-Version dieser Instanz ist unbekannt — bitte prüfen.";
    box.innerHTML =
      "<h3>" + headline + "</h3>" +
      '<p class="muted">' + line +
      " Bitte im Launcher/CurseForge genau diese Version einstellen — sonst schlägt der Server-Beitritt fehl.</p>";
    planBody.prepend(box);
  }
}

async function doApply() {
  if (!invoke) return;
  setStatus("Aktualisiere … (Fenster offen lassen)", "busy");
  applyBtn.disabled = true;
  checkBtn.disabled = true;
  showProgress(true);
  try {
    const res = await invoke("apply_update", {
      instance: selected.path,
      baseUrl: baseUrlInput.value.trim(),
      removeExtras: [...removeExtras],
      keepCollisions: [...keepCollisions],
    });
    let msg = `Fertig: ${res.downloaded} geladen, ${res.deleted} entfernt.`;
    if (res.backupDir) msg += " Backup: " + res.backupDir;
    setStatus(msg, "ok");
    showProgress(false);
    refreshCheckEnabled();
    // Re-check so the plan reflects the new state.
    await doCheck();
  } catch (e) {
    setStatus("Fehler: " + e, "err");
    showProgress(false);
    applyBtn.disabled = false;
    refreshCheckEnabled();
  }
}

async function init() {
  const saved = localStorage.getItem("bonegrader.baseUrl");
  baseUrlInput.value = saved || "https://bonegrader.rescue-compete.de/main";
  baseUrlInput.addEventListener("input", () => {
    localStorage.setItem("bonegrader.baseUrl", baseUrlInput.value.trim());
    refreshCheckEnabled();
  });
  // Wizard navigation.
  next1.addEventListener("click", () => {
    if (baseUrlInput.value.trim()) goStep(2, { grow: true });
  });
  checkBtn.addEventListener("click", doCheck);
  applyBtn.addEventListener("click", doApply);
  stepDots.forEach((dot) =>
    dot.addEventListener("click", () => goStep(Number(dot.dataset.step)))
  );
  document.querySelectorAll("[data-goto]").forEach((btn) =>
    btn.addEventListener("click", () => goStep(Number(btn.dataset.goto)))
  );
  el("useManual").addEventListener("click", () => {
    const p = el("manualPath").value.trim();
    if (p) selectInstance({ path: p, name: p.split(/[/\\]/).pop() || p }, null);
  });

  refreshCheckEnabled();
  goStep(1);

  if (!invoke) {
    instancesBox.innerHTML = '<p class="muted">Vorschau-Modus (kein Tauri) — Erkennung inaktiv.</p>';
    return;
  }
  if (listen) {
    await listen("update-progress", (e) => updateProgress(e.payload));
  }
  try {
    renderInstances(await invoke("detect_instances"));
  } catch (e) {
    setStatus("Erkennung fehlgeschlagen: " + e, "err");
  }
}

init();
