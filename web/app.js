// Remote Audio v0.1 built-in Web UI.
// Vanilla JS, no framework. Poll every POLL_MS.
// i18n: language packs live under /lang/<code>.json. Adding a new language
// means dropping a new JSON with a `_lang: { code, name }` block; the server
// auto-discovers it via GET /api/languages.

const POLL_MS = 3000;
const VOLUME_DEBOUNCE_MS = 200;
const TOKEN_KEY = "audioremote.token";
const LANG_KEY = "audioremote.lang";

const app = document.getElementById("app");
const toastEl = document.getElementById("toast");

let state = {
  token: readTokenFromHashOrStorage(),
  status: null,
  devices: null,
  volume: null,
  showSettings: false,
  showAbout: false,
  about: null,
  showInactive: false,
  pendingId: null,
  pollTimer: null,
  needsToken: false,
  about_build: null,    // build_id the `about` payload was fetched for
  lang: null,           // active language code, e.g. "ja"
  strings: {},          // key -> translated string
  langs: [],            // available packs [{code, name}]
};

// Kept outside render state so rebuilding the page during polling never loses
// an in-progress slider drag or reorders a newer volume request behind an old
// response.
const volumeControl = {
  active: false,
  draft: null,
  // Field names the user actually changed and the server has not confirmed yet.
  // Only these get posted: sending the whole object would write back a `muted`
  // we never touched and silently undo a change made in Windows or by another
  // client between two polls.
  dirty: new Set(),
  timer: null,
  sending: false,
  queued: false,
  revision: 0,
};

/** True while a modal sheet is open.
 *
 *  Polling must not re-render then. render() rebuilds #app from scratch, which
 *  recreates the sheet — and the sheet is the scroll container, so its scrollTop
 *  snaps back to 0 every POLL_MS. Scrolling down to the links at the bottom and
 *  having them yanked away (or replaced mid-click) is the same defect as the
 *  volume slider losing its drag to a poll: render() must not fire while the
 *  user is interacting. The content behind a modal is not visible anyway, and
 *  closing the sheet re-renders with fresh state. */
function sheetOpen() {
  return state.showAbout || state.showSettings;
}

/** True while a local volume interaction is still unresolved. Polling must not
 *  overwrite the panel (or re-render it out from under the pointer) until it is. */
function volumeInteractionInFlight() {
  return (
    volumeControl.active ||
    volumeControl.timer !== null ||
    volumeControl.sending ||
    volumeControl.queued ||
    volumeControl.dirty.size > 0
  );
}

// ---------- Token from hash ----------

function readTokenFromHashOrStorage() {
  const stored = localStorage.getItem(TOKEN_KEY) || "";
  const hash = location.hash.startsWith("#") ? location.hash.slice(1) : "";
  const params = new URLSearchParams(hash);
  const fromUrl = params.get("t");
  if (fromUrl && fromUrl.trim()) {
    const t = fromUrl.trim();
    localStorage.setItem(TOKEN_KEY, t);
    const clean = location.pathname + location.search;
    history.replaceState(null, "", clean);
    return t;
  }
  return stored;
}

// ---------- i18n ----------

function pickInitialLang(available) {
  const stored = localStorage.getItem(LANG_KEY);
  if (stored && available.some((l) => l.code === stored)) return stored;
  const nav = (navigator.language || "en").toLowerCase();
  const base = nav.split("-")[0];
  const match = available.find((l) => l.code === base);
  if (match) return match.code;
  return available[0]?.code || "en";
}

async function loadLang(code) {
  try {
    const res = await fetch("/lang/" + code + ".json");
    if (!res.ok) throw new Error("lang " + code + " HTTP " + res.status);
    state.strings = await res.json();
    state.lang = code;
    localStorage.setItem(LANG_KEY, code);
    document.documentElement.lang = code;
  } catch (e) {
    console.warn("lang load failed:", e);
    state.strings = state.strings || {};
  }
}

async function bootI18n() {
  try {
    state.langs = await api("/api/languages");
  } catch (_) {
    state.langs = [{ code: "en", name: "English" }];
  }
  const pick = pickInitialLang(state.langs);
  await loadLang(pick);
}

/** Translate a key. params values are substituted into `{name}` placeholders. */
function t(key, params) {
  let s = state.strings[key];
  if (s == null) s = key; // fall back to the key so missing translations are visible
  if (params) {
    for (const [k, v] of Object.entries(params)) {
      s = s.replaceAll("{" + k + "}", String(v));
    }
  }
  return s;
}

/** Like t(), but returns "" for a missing key instead of echoing the key.
 *  For optional prose — per-crate notes, where "no note" is a valid state and
 *  rendering "about.crate.ipnet" would be worse than rendering nothing. */
function tOptional(key) {
  const s = state.strings[key];
  return typeof s === "string" ? s : "";
}

// ---------- Networking ----------

async function api(path, opts = {}) {
  const headers = new Headers(opts.headers || {});
  if (state.token) headers.set("authorization", "Bearer " + state.token);
  const res = await fetch(path, { ...opts, headers });
  if (res.status === 401) {
    if (state.token) {
      state.token = "";
      localStorage.removeItem(TOKEN_KEY);
    }
    // Preserve what the guest has typed so far: re-rendering below rebuilds
    // the entry form from scratch.
    const typed = document.getElementById("tokenInput")?.value || "";
    const entering = !state.needsToken;
    state.needsToken = true;
    // Polling has to stop here and stay stopped: every further cycle would
    // render() over the token box the guest is typing into. stopPolling is
    // idempotent, and the parallel requests inside one poll cycle can all
    // 401 in the same instant — hence the `entering` guard on render.
    stopPolling();
    if (entering) {
      render();
      if (typed) {
        const inp = document.getElementById("tokenInput");
        if (inp) inp.value = typed;
      }
    }
    throw new Error("unauthorized");
  }
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(res.status + " " + res.statusText + " " + text);
  }
  if (res.status === 204) return null;
  return res.json();
}

async function pollOnce() {
  try {
    const results = await Promise.allSettled([
      api("/api/status"),
      api("/api/devices"),
      api("/api/volume"),
    ]);
    const [statusResult, devicesResult, volumeResult] = results;
    if (statusResult.status === "rejected" || devicesResult.status === "rejected") {
      const failure = statusResult.status === "rejected" ? statusResult.reason : devicesResult.reason;
      if (failure?.message === "unauthorized") return;
      throw failure;
    }

    const status = statusResult.value;
    const devicesResp = devicesResult.value;
    state.status = status;
    state.devices = devicesResp.devices || [];
    if (volumeResult.status === "fulfilled") {
      if (!volumeInteractionInFlight()) {
        state.volume = volumeResult.value;
      }
    } else if (volumeResult.reason?.message !== "unauthorized") {
      console.warn("volume poll failed:", volumeResult.reason);
    }
    state.needsToken = false;
    if (state.pendingId) {
      const d = state.devices.find((x) => x.id === state.pendingId);
      // Clear when the switch landed — or when it provably cannot land
      // anymore: the device vanished from the list or went inactive mid-switch.
      // Without those two, one unplugged headset locks the picker forever.
      if (!d || d.state !== "active" || d.is_default_multimedia) state.pendingId = null;
    }
    // Skip the full re-render while the user is interacting with something that
    // render() would destroy: a mid-drag volume slider (implicit pointer capture
    // is released when the element is removed) or an open sheet (whose scroll
    // position and in-flight clicks live in nodes innerHTML="" throws away).
    if (!volumeInteractionInFlight() && !sheetOpen()) {
      render();
    }
  } catch (e) {
    if (e.message === "unauthorized") return;
    console.warn("poll failed:", e);
  }
}

function startPolling() {
  stopPolling();
  pollOnce();
  state.pollTimer = setInterval(pollOnce, POLL_MS);
}
function stopPolling() {
  if (state.pollTimer) { clearInterval(state.pollTimer); state.pollTimer = null; }
}

// ---------- Actions ----------

async function setDefault(deviceId) {
  state.pendingId = deviceId;
  render();
  try {
    await api("/api/devices/" + encodeURIComponent(deviceId) + "/default", {
      method: "POST",
    });
    setTimeout(pollOnce, 400);
  } catch (e) {
    state.pendingId = null;
    toast(t("toast.switchFailed", { msg: e.message }), "err");
    render();
    pollOnce();
  }
}

/* Ask the host to restart its server. The reply arrives before the process dies
   (the supervisor does the killing, not the server itself), so a 202 means the
   request landed — not that the server is back. Polling picks that up on its
   own; there is nothing here to await. */
async function restartServer() {
  try {
    await api("/api/restart", { method: "POST" });
    toast(t("toast.restartRequested"));
  } catch (e) {
    toast(t("toast.restartFailed", { msg: e.message }), "err");
  }
}

function saveToken(tok) {
  state.token = tok.trim();
  localStorage.setItem(TOKEN_KEY, state.token);
  state.needsToken = false;
  render();
  startPolling();
}

function clearToken() {
  state.token = "";
  localStorage.removeItem(TOKEN_KEY);
  state.status = null;
  state.devices = null;
  state.volume = null;
  state.showSettings = false;
  state.about = null;
  state.about_build = null;
  state.pendingId = null;
  state.needsToken = true;
  stopPolling();
  render();
}

async function changeLang(code) {
  await loadLang(code);
  render();
}

// ---------- Master volume ---------------------------------------------------

function volumePercent(level) {
  return Math.round(Math.max(0, Math.min(1, Number(level))) * 100);
}

function volumeButtonText(muted) {
  return (muted ? "🔇 " : "🔊 ") + t(muted ? "volume.unmute" : "volume.mute");
}

function volumeButtonLabel(muted) {
  return t(muted ? "volume.unmuteLabel" : "volume.muteLabel");
}

function updateVolumeDom(volume) {
  if (!volume) return;
  const percent = volumePercent(volume.level);
  const slider = document.getElementById("volumeSlider");
  const output = document.getElementById("volumeLevel");
  const muteButton = document.getElementById("volumeMuteButton");
  if (slider) slider.value = String(percent);
  if (output) output.textContent = percent + "%";
  if (muteButton) {
    muteButton.textContent = volumeButtonText(volume.muted);
    muteButton.setAttribute("aria-label", volumeButtonLabel(volume.muted));
    muteButton.setAttribute("aria-pressed", String(volume.muted));
  }
}

/** Body for the next POST: the dirty fields only, never the whole state.
 *  Returns null when there is nothing left to send. */
function nextVolumeRequest() {
  const source = volumeControl.draft || state.volume;
  if (!source) return null;
  const body = {};
  for (const key of volumeControl.dirty) {
    if (source[key] !== undefined) body[key] = source[key];
  }
  if (!Object.keys(body).length) return null;
  return { revision: volumeControl.revision, body };
}

function scheduleVolumePost() {
  if (volumeControl.timer) clearTimeout(volumeControl.timer);
  volumeControl.timer = setTimeout(() => {
    volumeControl.timer = null;
    // Rebuild the body when the in-flight request finishes rather than queueing a
    // snapshot: edits made while it was in flight have to be included too.
    if (volumeControl.sending) {
      volumeControl.queued = true;
      return;
    }
    const request = nextVolumeRequest();
    if (request) void sendVolume(request);
  }, VOLUME_DEBOUNCE_MS);
}

function updateVolumeDraft(patch) {
  const current = volumeControl.draft || state.volume;
  if (!current) return;
  volumeControl.draft = { ...current, ...patch };
  for (const key of Object.keys(patch)) volumeControl.dirty.add(key);
  volumeControl.revision += 1;
  volumeControl.active = true;
  state.volume = { ...state.volume, ...patch };
  updateVolumeDom(state.volume);
  scheduleVolumePost();
}

async function refreshVolumeAfterFailure(revision) {
  try {
    const volume = await api("/api/volume");
    if (revision !== volumeControl.revision) return;
    state.volume = volume;
    volumeControl.draft = null;
    volumeControl.dirty.clear();
    updateVolumeDom(volume);
    // render() rebuilds open sheets (scroll position and all); the DOM patch
    // above is everything that is needed while one is up.
    if (!sheetOpen()) render();
  } catch (e) {
    if (e.message !== "unauthorized") console.warn("volume refresh failed:", e);
  }
}

async function sendVolume(request) {
  volumeControl.sending = true;
  try {
    const volume = await api("/api/volume", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(request.body),
    });
    if (request.revision === volumeControl.revision) {
      state.volume = volume;
      volumeControl.draft = volume;
      // Confirmed by the server, so these fields are no longer pending. A newer
      // revision means the user kept going: keep them dirty for the next post.
      volumeControl.dirty.clear();
      updateVolumeDom(volume);
    }
  } catch (e) {
    if (request.revision === volumeControl.revision) {
      volumeControl.active = false;
      volumeControl.dirty.clear();
      toast(t("toast.volumeFailed", { msg: e.message }), "err");
      await refreshVolumeAfterFailure(request.revision);
    }
  } finally {
    volumeControl.sending = false;
    const queued = volumeControl.queued;
    volumeControl.queued = false;
    const next = queued ? nextVolumeRequest() : null;
    if (next) {
      void sendVolume(next);
    } else if (request.revision === volumeControl.revision) {
      volumeControl.active = false;
      volumeControl.draft = null;
    }
  }
}

// ---------- Rendering ----------

function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "onclick") el.addEventListener("click", v);
    else if (k === "oninput") el.addEventListener("input", v);
    else if (k === "onchange") el.addEventListener("change", v);
    else if (k === "class") el.className = v;
    else if (k === "html") el.innerHTML = v;
    else el.setAttribute(k, v);
  }
  for (const c of children.flat()) {
    if (c == null) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

function render() {
  app.innerHTML = "";
  app.className = "app";
  if (state.needsToken) return renderTokenEntry();
  if (!state.status || !state.devices) return renderLoading();
  renderMain();
  if (state.showSettings) renderSettingsSheet();
  if (state.showAbout) renderAboutSheet();
}

function renderLoading() {
  app.append(h("div", { class: "loading" }, t("connecting")));
}

function renderTokenEntry() {
  app.append(
    h("div", { class: "center" },
      h("h1", {}, t("brand.name")),
      h("p", {}, t("tokenEntry.hint")),
      h("input", {
        id: "tokenInput", class: "field", type: "text",
        placeholder: "ar_live_…", autocomplete: "off",
      }),
      h("br"),
      h("button", {
        class: "btn",
        onclick: () => {
          const v = document.getElementById("tokenInput").value;
          if (v.trim()) saveToken(v);
        },
      }, t("tokenEntry.button")),
    )
  );
  const inp = document.getElementById("tokenInput");
  if (inp) {
    inp.focus();
    inp.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && e.target.value.trim()) saveToken(e.target.value);
    });
  }
}

function renderMain() {
  const s = state.status;
  const lanBadge = s.lan_exposed
    ? h("span", { class: "badge badge-warn" }, t("header.lanExposed"))
    : null;

  const header = h("div", { class: "header" },
    h("div", { class: "brand" },
      h("img", { src: "/icons/favicon.svg", class: "brand-icon", alt: "" }),
      h("h1", {}, t("brand.name")),
    ),
    h("div", { class: "actions" },
      lanBadge,
      h("button", {
        class: "icon-btn", title: t("header.settingsTitle"),
        onclick: () => { state.showSettings = true; render(); },
      }, "⚙"),
    ),
  );

  const sortName = t("sort." + s.device_sort);

  const meta = h("div", { class: "meta" },
    t("meta.line", { bind: s.bind, port: s.port, sort: sortName }),
  );

  const share = renderSharePanel(s.share_urls || []);
  const volume = renderVolumePanel();

  const activeDevices = state.devices.filter((d) => d.state === "active");
  const hiddenCount = state.devices.length - activeDevices.length;
  const visible = state.showInactive ? state.devices : activeDevices;
  const cards = visible.map((d) => renderDeviceCard(d));

  app.append(header, meta, share, volume, h("div", {}, ...cards));

  if (hiddenCount > 0) {
    app.append(h("button", {
      class: "show-all",
      onclick: () => { state.showInactive = !state.showInactive; render(); },
    }, state.showInactive
      ? t("devices.hideInactive")
      : t("devices.showInactive", { count: hiddenCount })));
  }

  app.append(
    h("div", { class: "footer-note" }, t("devices.footerNote")),
  );
}

function renderVolumePanel() {
  const v = state.volume;
  if (!v) return document.createDocumentFragment();
  const percent = volumePercent(v.level);
  const slider = h("input", {
    id: "volumeSlider",
    class: "volume-slider",
    type: "range",
    min: "0",
    max: "100",
    step: "1",
    value: String(percent),
    "aria-label": t("volume.sliderLabel"),
    oninput: (e) => updateVolumeDraft({ level: Number(e.target.value) / 100 }),
    onchange: () => { volumeControl.active = false; },
  });
  const muteButton = h("button", {
    id: "volumeMuteButton",
    class: "btn volume-mute",
    type: "button",
    "aria-label": volumeButtonLabel(v.muted),
    "aria-pressed": String(v.muted),
    onclick: () => updateVolumeDraft({ muted: !((volumeControl.draft || state.volume).muted) }),
  }, volumeButtonText(v.muted));

  return h("section", { class: "volume-panel", "aria-labelledby": "volumeTitle" },
    h("div", { class: "volume-head" },
      h("div", {},
        h("h2", { id: "volumeTitle" }, t("volume.title")),
        h("div", { class: "volume-hint" }, t("volume.hint")),
      ),
      h("output", { id: "volumeLevel", class: "volume-level", for: "volumeSlider" }, percent + "%"),
    ),
    slider,
    muteButton,
  );
}

function renderSharePanel(entries) {
  const isLoopback = /^https?:\/\/(127\.0\.0\.1|localhost|\[::1\])(:|\/|$)/.test(location.origin);
  if (!isLoopback) return document.createDocumentFragment();
  if (!entries.length) return document.createDocumentFragment();

  const wrap = h("div", { class: "share" },
    h("div", { class: "share-title" }, t("share.title")),
    h("div", { class: "share-meta" }, t("share.hint")),
  );
  entries.forEach((e) => {
    const url = e.url;
    const iface = e.interface || "";
    const isVirtual = !!e.virtual_iface;

    const urlInput = h("input", { class: "share-url", type: "text", readonly: "readonly", value: url });
    const row = h("div", { class: "share-row" },
      urlInput,
      h("button", {
        class: "btn small",
        onclick: (evt) => {
          // Capture the button synchronously: currentTarget is nulled once
          // event dispatch ends, and the promise callback below runs long
          // after that.
          const btn = evt.currentTarget;
          navigator.clipboard.writeText(url).then(
            () => {
              const orig = btn.textContent;
              // Green flash + bounce on the button, plus a soft highlight on the
              // URL input so the user sees "this is what got copied".
              btn.textContent = "✓ " + t("share.copied");
              btn.classList.add("copied");
              urlInput.classList.remove("copied-flash");
              // Re-trigger animation by forcing reflow.
              void urlInput.offsetWidth;
              urlInput.classList.add("copied-flash");
              setTimeout(() => {
                btn.textContent = orig;
                btn.classList.remove("copied");
              }, 1600);
            },
            () => { toast(t("share.copyFailed"), "err"); }
          );
        },
      }, t("share.copy")),
    );

    const label = h("div", { class: "share-iface" + (isVirtual ? " virtual" : "") },
      iface + (isVirtual ? t("share.virtualSwitch") : ""),
    );

    wrap.append(row, label);
  });
  return wrap;
}

function renderDeviceCard(d) {
  const switching = !!state.pendingId;
  const optimisticSelected = state.pendingId
    ? d.id === state.pendingId
    : d.is_default_multimedia;
  const cls = [
    "card",
    optimisticSelected ? "selected" : "",
    d.state !== "active" ? "dim" : "",
    state.pendingId === d.id ? "busy" : "",
    // Every other card is locked while a switch is in flight — see the onclick.
    switching && state.pendingId !== d.id ? "locked" : "",
  ].filter(Boolean).join(" ");

  const stateLabel = t("deviceState." + d.state);

  const roleNames = [];
  if (d.is_default_console) roleNames.push(t("roles.console"));
  if (d.is_default_multimedia) roleNames.push(t("roles.multimedia"));
  if (d.is_default_communications) roleNames.push(t("roles.communications"));

  const sub = roleNames.length
    ? stateLabel + " / " + t("roles.default_prefix") + roleNames.join(", ")
    : stateLabel;

  return h(
    "div",
    {
      class: cls,
      onclick: () => {
        // One switch at a time. The server changes Console / Multimedia /
        // Communications one role at a time, so a second request that overlaps
        // the first can leave the three roles pointing at different devices —
        // exactly what this app exists to prevent. The server serializes and
        // verifies too; this just avoids provoking a guaranteed 409.
        if (switching) return;
        if (d.state !== "active") return;
        if (optimisticSelected && roleNames.length === 3) return;
        setDefault(d.id);
      },
    },
    h("div", { class: "dot" + (optimisticSelected ? " on" : "") }),
    h("div", { class: "info" },
      h("div", { class: "name" }, d.name || "(unnamed)"),
      h("div", { class: "sub" }, sub),
    ),
    state.pendingId === d.id
      ? h("div", { class: "status meta" }, t("devices.applying"))
      : null,
  );
}

function renderSettingsSheet() {
  const s = state.status || {};
  const tokenPreview = state.token.length > 12
    ? state.token.slice(0, 8) + "…" + state.token.slice(-4)
    : state.token;

  const langSelect = h("select", {
    class: "field small",
    onchange: (e) => { changeLang(e.target.value); },
  },
    ...state.langs.map((l) =>
      h("option", { value: l.code, ...(l.code === state.lang ? { selected: "selected" } : {}) },
        l.name),
    ),
  );

  const sheet = h("div", { class: "sheet-bg", onclick: (e) => {
    if (e.target.classList.contains("sheet-bg")) {
      state.showSettings = false; render();
    }
  }},
    h("div", { class: "sheet" },
      h("h2", {}, t("settings.title")),
      h("div", { class: "row" },
        h("div", { class: "label" }, t("settings.server")),
        h("div", { class: "val" }, (s.bind || "?") + ":" + (s.port || "?")),
      ),
      h("div", { class: "row" },
        h("div", { class: "label" }, t("settings.version")),
        h("div", { class: "val" }, s.version ? s.version + (s.build_id ? " (" + s.build_id + ")" : "") : "?"),
      ),
      h("div", { class: "row" },
        h("div", { class: "label" }, t("settings.sort")),
        h("div", { class: "val" }, s.device_sort || "?"),
      ),
      h("div", { class: "row" },
        h("div", { class: "label" }, t("settings.token")),
        h("div", { class: "val" }, tokenPreview || t("settings.tokenEmpty")),
      ),
      h("div", { class: "row" },
        h("div", { class: "label" }, t("settings.lanExposed")),
        h("div", { class: "val" }, s.lan_exposed ? t("settings.lanExposed.yes") : t("settings.lanExposed.no")),
      ),
      h("div", { class: "row" },
        h("div", { class: "label" }, t("settings.language")),
        h("div", { class: "val" }, langSelect),
      ),
      h("div", { class: "row" },
        h("div", { class: "label" }, t("settings.about")),
        h("button", { class: "btn ghost small",
          onclick: () => {
            state.showSettings = false;
            state.showAbout = true;
            render();
            loadAbout();
          },
        }, t("settings.aboutOpen")),
      ),
      /* Only when a supervisor is there to start it again. Without one the
         endpoint answers 501, and a button that always fails is worse than no
         button. */
      s.supervised
        ? h("div", { class: "row" },
            h("div", { class: "label" }, t("settings.restart")),
            h("button", { class: "btn ghost small",
              onclick: () => {
                if (confirm(t("settings.restartConfirm"))) {
                  state.showSettings = false;
                  render();
                  restartServer();
                }
              },
            }, t("settings.restartButton")),
          )
        : null,
      // Classes, not inline `style` attributes: the server sends a strict
      // Content-Security-Policy without 'unsafe-inline'.
      h("div", { class: "sheet-actions" },
        h("button", { class: "btn ghost small spaced",
          onclick: () => { state.showSettings = false; render(); },
        }, t("settings.close")),
        h("button", { class: "btn small",
          onclick: () => {
            if (confirm(t("settings.reissueConfirm"))) {
              clearToken();
            }
          },
        }, t("settings.reissueToken")),
      ),
    ),
  );
  app.append(sheet);
}

async function loadAbout() {
  // Cache per host build: after a restart or a staged update the previous
  // About content (versions, OSS list) is wrong, not merely stale-looking.
  // status.build_id changes across both, so it is the invalidation key.
  const build = state.status ? state.status.build_id : null;
  if (state.about && state.about_build === build) return;
  try {
    state.about = await api("/api/about");
    state.about_build = build;
    if (state.showAbout) render();
  } catch (e) {
    console.warn("about fetch failed:", e);
  }
}

function renderAboutSheet() {
  const bg = h("div", {
    class: "sheet-bg",
    onclick: (e) => {
      if (e.target.classList.contains("sheet-bg")) {
        state.showAbout = false; render();
      }
    },
  });

  const sheet = h("div", { class: "sheet about" });

  sheet.append(
    h("div", { class: "about-head" },
      h("h2", {}, t("about.title")),
      h("button", { class: "icon-btn", title: t("settings.close"),
        onclick: () => { state.showAbout = false; render(); },
      }, "×"),
    ),
  );

  if (!state.about) {
    sheet.append(h("div", { class: "loading" }, t("loading")));
    bg.append(sheet); app.append(bg); return;
  }

  const a = state.about.app || {};

  sheet.append(
    h("div", { class: "about-block" },
      h("div", { class: "about-name" }, a.name || "Remote Audio"),
      h("div", { class: "meta" }, t("about.versionRow", { version: a.version || "?" })),
      // Prefer the localized copy: `description` comes from Cargo.toml, which
      // stays English because crates.io shows it.
      h("p", {}, tOptional("about.description") || a.description || ""),
    ),
  );

  sheet.append(
    h("div", { class: "about-block" },
      h("div", { class: "about-h" }, t("about.licenseSection")),
      h("div", {}, t("about.licenseBody", {
        name: a.name || "Remote Audio",
        license: a.license || "MIT",
        copyright: a.copyright || "",
      })),
    ),
  );

  sheet.append(
    h("div", { class: "about-block" },
      h("div", { class: "about-h" }, t("about.frontendSection")),
      h("div", {}, t("about.frontendBody")),
    ),
  );

  const oss = state.about.oss || [];
  const listBox = h("div", { class: "about-block" },
    h("div", { class: "about-h" }, t("about.ossSection", { count: oss.length })),
  );
  oss.forEach((o) => {
    // Facts (name / version / license) come from the server; the "why we use it"
    // note is UI prose and therefore comes from the language pack.
    const purpose = tOptional("about.crate." + o.name);
    listBox.append(
      h("div", { class: "oss-row" },
        h("div", { class: "oss-name" }, o.name + " " + o.version),
        h("div", { class: "oss-meta" },
          o.license + (purpose ? " · " + purpose : "")),
      ),
    );
  });
  sheet.append(listBox);
  sheet.append(renderAboutLinks(a));

  bg.append(sheet);
  app.append(bg);
}

/** Build the GitHub "new issue" URL with a pre-filled body.
 *
 *  Only four values are ever collected: app version, build id, host OS, and
 *  the browser UA. Device names are deliberately left out — Bluetooth
 *  endpoints are routinely named after their owner — and so are tokens and
 *  LAN addresses. */
function buildIssueUrl(a) {
  const body = t("report.template", {
    version: a.version || "?",
    build: a.build_id || "?",
    os: a.os || "?",
    ua: (navigator.userAgent || "?").slice(0, 200),
  });
  return a.issues_new + "?body=" + encodeURIComponent(body);
}

function renderAboutLinks(a) {
  const box = h("div", { class: "about-block" },
    h("div", { class: "about-h" }, t("about.linksSection")),
  );

  const links = [];
  if (a.repository) links.push([a.repository, t("about.linkRepo")]);
  if (a.author_site) links.push([a.author_site, t("about.linkSite")]);
  if (a.issues_new) links.push([buildIssueUrl(a), t("about.linkReport")]);

  links.forEach(([href, label]) => {
    box.append(h("a", {
      class: "about-link", href, target: "_blank", rel: "noopener noreferrer",
    }, label));
  });

  if (a.issues_new) {
    box.append(h("div", { class: "about-note" }, t("about.reportNote")));
  }
  return box;
}

// ---------- Toast ----------

let toastTimer = null;
function toast(msg, kind = "") {
  toastEl.textContent = msg;
  toastEl.className = "toast" + (kind ? " " + kind : "");
  toastEl.hidden = false;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { toastEl.hidden = true; }, 2400);
}

// ---------- Boot ----------

(async function boot() {
  await bootI18n();
  render();
  startPolling();
})();

document.addEventListener("visibilitychange", () => {
  if (document.hidden) stopPolling();
  else startPolling();
});
