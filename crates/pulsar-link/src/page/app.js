"use strict";
// The save manager's page: polls pulsar-link for what it is doing and draws it.
// Offline (pulsar-link not running) is told apart from an empty card or an error.

const $ = (id) => document.getElementById(id);
const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
let lastSaves = "";
let lastPhase = "";
let choosing = false;
// The start-at-login switch is being saved; the poll leaves it alone meanwhile.
let switching = false;
// The card may be changed: it is read, and no game holds it.
let canWrite = false;

function applyLock() {
  for (const b of document.querySelectorAll(".write")) b.disabled = !canWrite;
}

function say(el, text, ok) {
  el.textContent = text;
  el.className = ok ? "secondary" : "error";
}

// A change to the card; the answer is what to tell the owner.
async function send(url, body, method = "POST") {
  try {
    const headers = typeof body === "string" ? { "content-type": "application/json" } : {};
    const res = await fetch(url, { method, body, headers });
    return { ok: res.ok, text: await res.text() };
  } catch {
    return { ok: false, text: "Not sent: pulsar-link is not running." };
  }
}

function button(cls, text, label) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "btn " + cls;
  b.textContent = text;
  if (label) b.setAttribute("aria-label", label);
  return b;
}

function status(el, text, kind) {
  el.textContent = text;
  el.className = "status " + kind;
}

function showPhase(s) {
  // Just stopped, it still answers for a moment; after that, an answer means it was
  // started again.
  if (stoppedNote) {
    if (Date.now() - stoppedAt < 3000) return;
    stoppedNote = "";
  }
  let text;
  let kind;
  switch (s.phase) {
    case "ready": text = "Ready for Flycast"; kind = "success"; break;
    case "reading":
      kind = "warning";
      text = s.doing === "PUT PAD DOWN"
        ? "Put the pad down: the card is read only while it is still"
        : s.of ? `Reading the card: ${s.done} of ${s.of} blocks` : "Reading the card";
      break;
    case "waiting": text = "Waiting for the Pulsar"; kind = "warning"; break;
    case "changed": text = "Not the card expected"; kind = "error"; break;
    default: text = s.phase; kind = "neutral";
  }
  // The live region speaks only when the wording changes, not on every poll.
  if (text !== lastPhase) {
    status($("phase"), text, kind);
    lastPhase = text;
  }
  $("pulsar").textContent = s.pulsar || (s.phase === "waiting" ? s.why : "");
  // Flycast decides at a game's boot whether to use the VMU. A changed card is not
  // served either, until the owner chooses.
  $("not-ready").hidden = s.phase === "ready";
  const bar = $("progress");
  $("progress-row").hidden = !(s.phase === "reading" && s.of);
  if (s.of) { bar.max = s.of; bar.value = s.done; }
  const changed = s.phase === "changed";
  $("changed").hidden = !changed;
  if (changed) {
    // Set only when it changes: an alert re-announces on every write.
    if ($("changed-why").textContent !== s.why) $("changed-why").textContent = s.why;
    $("use-card").textContent = s.pending
      ? `Set aside the ${s.pending === 1 ? "write" : `${s.pending} writes`} and use this card`
      : "Use this card as it is";
  }
}

async function drawIcon(canvas, name, speed) {
  const r = await fetch(`/api/icon/${encodeURIComponent(name)}`);
  if (!r.ok) return;
  const bytes = new Uint8ClampedArray(await r.arrayBuffer());
  const size = 32 * 32 * 4;
  const frames = [];
  for (let i = 0; i + size <= bytes.length; i += size) {
    frames.push(new ImageData(bytes.slice(i, i + size), 32, 32));
  }
  if (!frames.length) return;
  const ctx = canvas.getContext("2d");
  ctx.putImageData(frames[0], 0, 0);
  if (frames.length < 2) return;
  // The header's speed is in 1/30 s units. Under reduced motion the first frame stays.
  let n = 0;
  setInterval(() => {
    if (reduceMotion.matches) return;
    n = (n + 1) % frames.length;
    ctx.putImageData(frames[n], 0, 0);
  }, (Math.max(1, speed) * 1000) / 30);
}

function link(href, text, label) {
  const a = document.createElement("a");
  a.className = "btn btn-secondary";
  a.href = href;
  a.textContent = text;
  a.setAttribute("aria-label", label);
  return a;
}

function showSaves(card) {
  const key = JSON.stringify(card.saves);
  if (key === lastSaves) return;
  lastSaves = key;
  const list = $("saves");
  list.replaceChildren();
  for (const s of card.saves) {
    const li = document.createElement("li");
    const canvas = document.createElement("canvas");
    canvas.width = 32;
    canvas.height = 32;
    canvas.setAttribute("aria-hidden", "true");

    const what = document.createElement("div");
    what.className = "what";
    const title = document.createElement("div");
    title.textContent = s.dc || s.vmu || s.name;
    const meta = document.createElement("div");
    meta.className = "meta";
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = s.name;
    meta.append(name, ` · ${s.blocks} blocks`, s.date ? ` · ${s.date}` : "", s.game ? " · game" : "");
    what.append(title, meta);

    const get = document.createElement("div");
    get.className = "get";
    const q = encodeURIComponent(s.name);
    const who = title.textContent;
    const rm = button("btn-secondary write", "Remove", `Remove ${who} from the card`);
    const actions = [
      link(`/api/save/${q}/vms`, "VMS", `Download ${who} as a VMS file`),
      link(`/api/save/${q}/vmi`, "VMI", `Download the VMI description for ${who}`),
      link(`/api/save/${q}/dci`, "DCI", `Download ${who} as a DCI file`),
      rm,
    ];
    get.append(...actions);
    // Asked in place, not in a dialog: the question sits where the owner is looking.
    rm.addEventListener("click", () => {
      const yes = button("btn-danger write", `Remove ${s.name}`);
      const no = button("btn-secondary", "Keep it");
      get.replaceChildren(yes, no);
      applyLock();
      yes.focus();
      no.addEventListener("click", () => {
        get.replaceChildren(...actions);
        rm.focus();
      });
      yes.addEventListener("click", async () => {
        const r = await send(`/api/save/${q}`, undefined, "DELETE");
        say($("write-note"), r.text, r.ok);
        if (!r.ok) {
          get.replaceChildren(...actions);
          rm.focus();
        }
        refresh();
      });
    });

    li.append(canvas, what, get);
    list.append(li);
    if (s.icon_frames > 0) drawIcon(canvas, s.name, s.icon_speed);
  }
  applyLock();
}

// Flycast's link, and starting at login. `null` from the program means this system has
// no Flycast config found, or no start-at-login support; each is said, not hidden.
function showSetup(s) {
  const linked = s.flycast_linked;
  const text = $("flycast-link");
  if (linked === null) {
    text.textContent = "Flycast's settings were not found here. If Flycast is installed, " +
      "start it once and quit it; it can then be set up from here.";
    text.className = "secondary";
  } else if (linked) {
    text.textContent = "Flycast is set to use the Pulsar's VMU.";
    text.className = "secondary";
  } else {
    text.textContent = "Flycast is not set to use the Pulsar's VMU yet. It needs " +
      "Flycast 2.7 or later, closed while it is set.";
    text.className = "warning";
  }
  $("link-flycast").hidden = linked !== false;
  $("flycast-setup").hidden = false;
  $("at-login-row").hidden = s.at_login === null;
  if (!switching) $("at-login").checked = !!s.at_login;
  // Stopping leaves the start-at-login switch as it is: unticked first, it stays stopped.
  atLogin = s.at_login === true;
  flycastOn = !!s.flycast;
}

// Saves newer in Flycast's own files than on the card: told, never moved.
let lastNewer = "";
function showNewer(list) {
  const key = JSON.stringify(list);
  if (key === lastNewer) return;
  lastNewer = key;
  $("newer").hidden = !list.length;
  const ul = $("newer-list");
  ul.replaceChildren();
  for (const n of list) {
    const li = document.createElement("li");
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = n.save;
    const file = document.createElement("span");
    file.className = "name";
    file.textContent = n.file;
    li.append(name, ` of ${n.here}, in `, file, ` (on the VMU: ${n.card}) `);
    const put = button("btn-secondary write", "Put on the VMU", `Put ${n.save} from ${n.file} on the VMU`);
    put.addEventListener("click", async () => {
      const r = await send("/api/newer", JSON.stringify({ save: n.save, file: n.file }));
      say($("newer-note"), r.text, r.ok);
      refresh();
    });
    li.append(put);
    ul.append(li);
  }
}

async function refresh() {
  let s;
  try {
    const r = await fetch("/api/status", { cache: "no-store" });
    s = await r.json();
  } catch {
    // Offline: the page is loaded but pulsar-link is not answering. Stopped from here,
    // that is what was asked for, and the page says how to start it again.
    const text = stoppedNote || "pulsar-link is not running. Start it, and this page picks up again.";
    if (text !== lastPhase) {
      status($("phase"), text, stoppedNote ? "neutral" : "error");
      lastPhase = text;
    }
    return;
  }
  showPhase(s);
  $("flycast").textContent = s.flycast ? "Flycast is connected" : "Flycast is not connected";
  const pending = $("pending");
  const writes = s.pending === 1 ? "1 write" : `${s.pending} writes`;
  pending.textContent = !s.pending ? ""
    : s.phase === "changed" ? `${writes} kept for the card they were made for`
    : `${writes} on the way to the card`;
  pending.className = s.pending ? "warning tabular" : "";
  // The Pulsar writes its VMU only at pad idle, which nothing else on the page says.
  $("pending-why").hidden = !s.pending || s.phase === "changed";
  if (!choosing) {
    for (const r of document.querySelectorAll('input[name="dest"]')) {
      r.checked = r.value === s.destination;
    }
  }
  showSetup(s);
  showNewer(s.newer_here || []);
  // On `read`, not `card`: an unformatted card (a restore cut short) has no filesystem
  // to list, and a restore is exactly what it needs.
  canWrite = s.phase === "ready" && !s.flycast && s.read;
  $("locked").hidden = !s.flycast;
  const orphans = s.card ? s.card.orphans : 0;
  $("orphans").hidden = !orphans;
  $("orphans-text").textContent =
    `${orphans} blocks are marked used but belong to no save, as a change cut short leaves.`;
  applyLock();
  const msg = $("cardmsg");
  if (s.card) {
    msg.textContent = s.card.saves.length ? "" : "There are no saves on this card.";
    msg.removeAttribute("role");
    msg.className = "secondary";
    $("free").textContent = `${s.card.free_blocks} blocks free`;
    $("backup").hidden = false;
    showSaves(s.card);
  } else {
    if (s.card_error) {
      msg.textContent = `The card's saves cannot be listed: ${s.card_error}. ` +
        "Download a backup of it as it is, then restore a good backup below.";
      msg.setAttribute("role", "alert");
      msg.className = "error";
    } else {
      msg.textContent = "The card shows here once it has been read.";
      msg.removeAttribute("role");
      msg.className = "secondary";
    }
    $("free").textContent = "";
    $("backup").hidden = !s.read;
    showSaves({ saves: [] });
  }
}

for (const r of document.querySelectorAll('input[name="dest"]')) {
  r.addEventListener("change", async () => {
    choosing = true;
    const note = $("note");
    try {
      const res = await fetch("/api/destination", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ destination: r.value }),
      });
      note.textContent = await res.text();
      note.className = res.ok ? "secondary" : "error";
    } catch {
      note.textContent = "Not saved: pulsar-link is not running.";
      note.className = "error";
    }
    choosing = false;
    refresh();
  });
}

$("at-login").addEventListener("change", async (ev) => {
  switching = true;
  const note = $("setup-note");
  try {
    const res = await fetch("/api/at-login", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ on: ev.target.checked }),
    });
    say(note, await res.text(), res.ok);
  } catch {
    say(note, "Not saved: pulsar-link is not running.", false);
  }
  switching = false;
  refresh();
});

let stoppedNote = "";
let stoppedAt = 0;
let atLogin = false;
let flycastOn = false;

async function stopIt() {
  let ask = "Stop pulsar-link? The VMU stops showing the game's screen until it runs again.";
  if (flycastOn) ask += "\n\nFlycast is connected: quit the game first, or its next save fails.";
  if (!confirm(ask)) return;
  const r = await send("/api/stop");
  if (!r.ok) {
    say($("setup-note"), r.text, false);
    return;
  }
  stoppedNote = atLogin
    ? "pulsar-link is stopped. It starts again the next time you log in, or when you open it."
    : "pulsar-link is stopped. Open pulsar-link to start it again.";
  stoppedAt = Date.now();
  status($("phase"), stoppedNote, "neutral");
  lastPhase = stoppedNote;
}

$("stop").addEventListener("click", stopIt);

$("link-flycast").addEventListener("click", async () => {
  const r = await send("/api/flycast/link");
  say($("setup-note"), r.text, r.ok);
  refresh();
});

$("reclaim").addEventListener("click", async () => {
  const r = await send("/api/reclaim");
  say($("write-note"), r.text, r.ok);
  refresh();
});

const ext = (f) => f.name.split(".").pop().toLowerCase();
const stem = (f) => f.name.slice(0, f.name.length - ext(f).length - 1).toLowerCase();
const VMI_BYTES = 108;

// A card image: list its saves, each with its own button to add it.
async function listImage(file, lines) {
  const res = await send("/api/inspect", file);
  if (!res.ok) {
    lines.push(`${file.name}: ${res.text}`);
    return;
  }
  const info = JSON.parse(res.text);
  if (!info.saves.length) {
    lines.push(`${file.name}: no saves on it.`);
    return;
  }
  lines.push(`${file.name}: choose the saves to add, below.`);
  for (const s of info.saves) {
    const li = document.createElement("li");
    li.append(document.createElement("span"));
    const what = document.createElement("div");
    what.className = "what";
    const title = document.createElement("div");
    title.textContent = s.dc || s.vmu || s.name;
    const meta = document.createElement("div");
    meta.className = "meta";
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = s.name;
    meta.append(name, ` · ${s.blocks} blocks · from ${file.name}`);
    what.append(title, meta);
    const get = document.createElement("div");
    get.className = "get";
    const add = button("btn-secondary write", "Add", `Add ${title.textContent} to the card`);
    get.append(add);
    add.addEventListener("click", async () => {
      const r = await send(`/api/put/image?name=${encodeURIComponent(s.name)}`, file);
      say($("add-note"), r.text, r.ok);
      if (r.ok) add.replaceWith("Added");
      refresh();
    });
    li.append(what, get);
    $("from-image").append(li);
  }
  applyLock();
}

$("add").addEventListener("click", async () => {
  const files = [...$("add-files").files];
  const note = $("add-note");
  if (!files.length) {
    say(note, "Choose the save files first.", false);
    return;
  }
  $("from-image").replaceChildren();
  const vmis = new Map(files.filter((f) => ext(f) === "vmi").map((f) => [stem(f), f]));
  const lines = [];
  let ok = true;
  for (const f of files) {
    let r;
    switch (ext(f)) {
      case "vmi":
        if (!files.some((v) => ext(v) === "vms" && stem(v) === stem(f))) {
          lines.push(`${f.name}: its .VMS was not chosen with it.`);
          ok = false;
        }
        continue;
      case "dci":
        r = await send("/api/put/dci", f);
        break;
      case "vms": {
        const vmi = vmis.get(stem(f));
        if (!vmi || vmi.size < VMI_BYTES) {
          lines.push(`${f.name}: choose its .VMI with it; the VMI names it on the card.`);
          ok = false;
          continue;
        }
        // The VMI's first 108 bytes, then the VMS: the program splits them there.
        r = await send("/api/put/vms", new Blob([vmi.slice(0, VMI_BYTES), f]));
        break;
      }
      case "bin":
        await listImage(f, lines);
        continue;
      default:
        lines.push(`${f.name}: not a save file this page reads.`);
        ok = false;
        continue;
    }
    lines.push(`${f.name}: ${r.text}`);
    ok &&= r.ok;
  }
  say(note, lines.join("\n"), ok);
  refresh();
});

function hideRestore() {
  $("restore-confirm").hidden = true;
  $("restore-file").value = "";
}

$("restore-file").addEventListener("change", async () => {
  const f = $("restore-file").files[0];
  const note = $("restore-note");
  note.textContent = "";
  if (!f) {
    $("restore-confirm").hidden = true;
    return;
  }
  const r = await send("/api/inspect", f);
  if (!r.ok) {
    say(note, `${f.name}: ${r.text}`, false);
    hideRestore();
    return;
  }
  const info = JSON.parse(r.text);
  const names = info.saves.map((s) => s.name).join(", ");
  $("restore-what").textContent = info.saves.length
    ? `${f.name} holds ${info.saves.length} saves (${names}) and ${info.free_blocks} free blocks. ` +
      "Everything on the card now is replaced by it."
    : `${f.name} holds no saves: restoring it empties the card.`;
  $("restore-confirm").hidden = false;
  applyLock();
  $("restore").focus();
});

$("restore").addEventListener("click", async () => {
  const f = $("restore-file").files[0];
  if (!f) return;
  const r = await send("/api/restore", f);
  say($("restore-note"), r.text, r.ok);
  hideRestore();
  refresh();
});

$("restore-cancel").addEventListener("click", () => {
  hideRestore();
  $("restore-file").focus();
});

// A changed card: read it again, or use it as it is.
for (const [id, choice] of [["retry", "retry"], ["use-card", "use-card"]]) {
  $(id).addEventListener("click", async () => {
    const note = $("changed-note");
    try {
      const res = await fetch(`/api/changed/${choice}`, { method: "POST" });
      note.textContent = await res.text();
      note.className = res.ok ? "secondary" : "error";
    } catch {
      note.textContent = "Not sent: pulsar-link is not running.";
      note.className = "error";
    }
    refresh();
  });
}

refresh();
setInterval(refresh, 2000);
