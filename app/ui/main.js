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

const listen = window.__TAURI__?.event?.listen;

let selected = null; // { path, name }
let lastPlan = null;

function setStatus(msg, kind = "") {
  statusLine.textContent = msg;
  statusLine.className = "status " + kind;
}

function refreshCheckEnabled() {
  checkBtn.disabled = !(selected && baseUrlInput.value.trim());
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
  if (!list.length) {
    instancesBox.innerHTML = '<p class="muted">Keine Instanz automatisch gefunden — bitte manuell angeben.</p>';
    return;
  }
  for (const inst of list) {
    const node = document.createElement("div");
    node.className = "instance";
    const loader = [inst.loaderType, inst.loaderVersion].filter(Boolean).join(" ") || "?";
    const mc = inst.mcVersion ? "MC " + inst.mcVersion : "";
    node.innerHTML =
      '<div><div class="name"></div><div class="meta"></div></div>' +
      '<span class="badge"></span>';
    node.querySelector(".name").textContent = inst.name;
    node.querySelector(".meta").textContent = [loader, mc].filter(Boolean).join(" · ");
    node.querySelector(".badge").textContent = inst.launcher;
    node.addEventListener("click", () => selectInstance(inst, node));
    instancesBox.appendChild(node);
  }
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
  planCard.classList.remove("hidden");
}

async function doCheck() {
  if (!invoke) return setStatus("Läuft nur in der Bonegrader-App (Tauri).", "err");
  setStatus("Prüfe …", "busy");
  planCard.classList.add("hidden");
  try {
    const plan = await invoke("plan_update", {
      instance: selected.path,
      base_url: baseUrlInput.value.trim(),
    });
    renderPlan(plan);
    setStatus("Prüfung abgeschlossen.", "ok");
  } catch (e) {
    setStatus("Fehler: " + e, "err");
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
      base_url: baseUrlInput.value.trim(),
      remove_extras: [...removeExtras],
      keep_collisions: [...keepCollisions],
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
  checkBtn.addEventListener("click", doCheck);
  applyBtn.addEventListener("click", doApply);
  el("useManual").addEventListener("click", () => {
    const p = el("manualPath").value.trim();
    if (p) selectInstance({ path: p, name: p.split(/[/\\]/).pop() || p }, null);
  });

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
