const $ = (id) => document.getElementById(id);

const fromHash = new URLSearchParams(location.hash.slice(1)).get("token");
if (fromHash) {
  sessionStorage.setItem("kcore-agent-token", fromHash);
  history.replaceState(null, "", location.pathname);
}
const token = sessionStorage.getItem("kcore-agent-token") || "";

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: { authorization: `Bearer ${token}`, ...(options.body ? { "content-type": "application/json" } : {}), ...options.headers },
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || `${response.status}`);
  return body;
}

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else if (value !== undefined && value !== false) node.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children) if (child) node.append(child);
  return node;
}

const state = {
  providers: [],
  sessions: [],
  current: null,
  busy: false,
  openForms: new Set(),
  stream: null,
  tools: new Map(),
  forms: new Map(),
  live: null,
};

function prettyJson(text) {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

function renderText(container, text) {
  const parts = text.split(/```/);
  parts.forEach((part, index) => {
    if (index % 2 === 1) {
      const body = part.replace(/^[a-zA-Z0-9_-]*\n/, "");
      container.append(el("pre", {}, el("code", { text: body.replace(/\n$/, "") })));
    } else if (part.trim()) {
      container.append(el("p", { text: part.replace(/^\n+|\n+$/g, "") }));
    }
  });
}

function scrollDown() {
  const t = $("transcript");
  t.scrollTop = t.scrollHeight;
}

function resetTranscript() {
  $("transcript").replaceChildren();
  state.tools.clear();
  state.forms.clear();
  state.openForms.clear();
  state.live = null;
  state.busy = false;
  updateComposer();
}

function updateComposer() {
  const blocked = state.openForms.size > 0;
  $("send").disabled = state.busy || !state.current;
  $("input").disabled = state.busy || blocked || !state.current;
  $("cancel").hidden = !state.busy;
  const notice = $("notice");
  notice.hidden = !blocked;
  notice.textContent = blocked ? "Waiting for your answer above." : "";
}

function renderEvent(event) {
  const transcript = $("transcript");
  switch (event.type) {
    case "user":
      transcript.append(el("div", { class: "msg user" }, el("div", { class: "bubble", text: event.text })));
      break;
    case "assistant": {
      if (state.live) {
        state.live.remove();
        state.live = null;
      }
      const bubble = el("div", { class: "bubble" });
      renderText(bubble, event.text);
      transcript.append(el("div", { class: "msg assistant" }, bubble));
      break;
    }
    case "tool_call": {
      const status = el("span", { class: "status run", text: "running" });
      const argsText = typeof event.args === "string" ? event.args : JSON.stringify(event.args, null, 2);
      const result = el("pre", { hidden: true });
      const resultLabel = el("div", { class: "label", text: "Result", hidden: true });
      const card = el(
        "details",
        { class: "tool" },
        el("summary", {}, el("strong", { text: event.name }), status),
        el("div", { class: "body" }, el("div", { class: "label", text: "Arguments" }), el("pre", {}, el("code", { text: argsText })), resultLabel, result),
      );
      state.tools.set(event.callId, { status, result, resultLabel, card });
      transcript.append(card);
      break;
    }
    case "tool_result": {
      const tool = state.tools.get(event.callId);
      if (!tool) break;
      tool.status.className = `status ${event.ok ? "ok" : "bad"}`;
      tool.status.textContent = `${event.ok ? "done" : "failed"} ${event.ms} ms`;
      tool.result.textContent = prettyJson(event.text);
      tool.result.hidden = false;
      tool.resultLabel.hidden = false;
      break;
    }
    case "form": {
      const card = renderForm(event);
      state.forms.set(event.formId, card);
      state.openForms.add(event.formId);
      transcript.append(card);
      break;
    }
    case "form_done": {
      const card = state.forms.get(event.formId);
      if (card) {
        card.classList.add("done");
        for (const control of card.querySelectorAll("input, select, button")) control.disabled = true;
        card.append(el("div", { class: "status", text: event.action === "accept" ? "Answered" : event.action === "decline" ? "Declined" : "Cancelled" }));
      }
      state.openForms.delete(event.formId);
      break;
    }
    case "error":
      transcript.append(el("div", { class: "err-line", text: event.message }));
      break;
    case "turn_start":
      state.busy = true;
      break;
    case "turn_end":
      state.busy = false;
      if (state.live) {
        state.live.remove();
        state.live = null;
      }
      if (event.reason === "cancelled") transcript.append(el("div", { class: "turn-note", text: "Stopped." }));
      loadSessions();
      break;
  }
  updateComposer();
  scrollDown();
}

function renderForm(event) {
  const isApproval = event.kind === "approval";
  const card = el("div", { class: `card ${isApproval ? "approval" : ""}` });
  const error = el("div", { class: "error", hidden: true });
  const submit = async (action, content) => {
    error.hidden = true;
    try {
      await api(`/api/sessions/${state.current}/forms/${event.formId}`, { method: "POST", body: JSON.stringify({ action, content }) });
    } catch (e) {
      error.textContent = e.message;
      error.hidden = false;
    }
  };

  if (isApproval) {
    const property = event.schema.properties.confirm;
    card.append(el("h3", { text: event.message }));
    if (property.description) card.append(el("pre", {}, el("code", { text: property.description })));
    card.append(
      el(
        "div",
        { class: "actions" },
        el("button", { class: "primary", type: "button", text: "Approve", onclick: () => submit("accept", { confirm: "yes" }) }),
        el("button", { class: "danger", type: "button", text: "Reject", onclick: () => submit("accept", { confirm: "no" }) }),
      ),
      error,
    );
    return card;
  }

  card.append(el("h3", { text: event.message }));
  const inputs = new Map();
  for (const [name, property] of Object.entries(event.schema.properties || {})) {
    const required = (event.schema.required || []).includes(name);
    const id = `${event.formId}-${name}`;
    let input;
    if (Array.isArray(property.enum)) {
      input = el("select", { id });
      input.append(el("option", { value: "", text: required ? "Choose" : "(none)" }));
      for (const choice of property.enum) input.append(el("option", { value: String(choice), text: String(choice) }));
    } else if (property.type === "boolean") {
      input = el("input", { id, type: "checkbox" });
    } else if (property.type === "number" || property.type === "integer") {
      input = el("input", { id, type: "number", step: property.type === "integer" ? "1" : "any", min: property.minimum, max: property.maximum });
    } else {
      input = el("input", { id, type: property.format === "uri" ? "url" : "text", autocomplete: "off", spellcheck: "false" });
    }
    inputs.set(name, { input, property });
    card.append(
      el(
        "div",
        { class: "field" },
        el("label", { for: id, text: `${property.title || name}${required ? " *" : ""}` }),
        input,
        property.description ? el("div", { class: "hint", text: property.description }) : null,
      ),
    );
  }
  card.append(
    el(
      "div",
      { class: "actions" },
      el("button", {
        class: "primary",
        type: "button",
        text: "Submit",
        onclick: () => {
          const content = {};
          for (const [name, { input, property }] of inputs) {
            if (property.type === "boolean") content[name] = input.checked;
            else if (input.value !== "") content[name] = property.type === "number" || property.type === "integer" ? Number(input.value) : input.value;
          }
          submit("accept", content);
        },
      }),
      el("button", { type: "button", text: "Skip", onclick: () => submit("decline") }),
    ),
    error,
  );
  return card;
}

async function openStream(sessionId) {
  state.stream?.abort();
  const controller = new AbortController();
  state.stream = controller;
  try {
    const response = await fetch(`/api/sessions/${sessionId}/events?after=0`, { headers: { authorization: `Bearer ${token}` }, signal: controller.signal });
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let split;
      while ((split = buffer.indexOf("\n\n")) >= 0) {
        const block = buffer.slice(0, split);
        buffer = buffer.slice(split + 2);
        let name = "message";
        let data = "";
        for (const line of block.split("\n")) {
          if (line.startsWith("event:")) name = line.slice(6).trim();
          else if (line.startsWith("data:")) data += line.slice(5).trim();
        }
        if (!data) continue;
        const parsed = JSON.parse(data);
        if (sessionId !== state.current) return;
        if (name === "delta") appendDelta(parsed.text);
        else renderEvent(parsed);
      }
    }
  } catch (e) {
    if (e.name !== "AbortError") $("transcript").append(el("div", { class: "err-line", text: `Connection lost: ${e.message}` }));
  }
}

function appendDelta(text) {
  if (!state.live) {
    state.live = el("div", { class: "msg assistant" }, el("div", { class: "bubble" }, el("p", { text: "" })));
    $("transcript").append(state.live);
  }
  state.live.querySelector("p").textContent += text;
  scrollDown();
}

async function selectSession(id) {
  state.current = id;
  resetTranscript();
  const session = state.sessions.find((s) => s.id === id);
  $("session-title").textContent = session ? session.title : "";
  renderSessionList();
  updateComposer();
  openStream(id);
}

function renderSessionList() {
  const list = $("session-list");
  list.replaceChildren();
  for (const session of state.sessions) {
    list.append(el("li", { class: session.id === state.current ? "active" : "", text: session.title, title: session.title, onclick: () => selectSession(session.id) }));
  }
}

async function loadSessions() {
  state.sessions = (await api("/api/sessions")).sessions;
  renderSessionList();
  const session = state.sessions.find((s) => s.id === state.current);
  if (session) $("session-title").textContent = session.title;
}

async function newSession() {
  const { id } = await api("/api/sessions", { method: "POST", body: "{}" });
  await loadSessions();
  await selectSession(id);
  $("input").focus();
}

function chosenProvider() {
  return $("provider").value;
}

async function loadProviders() {
  state.providers = (await api("/api/providers")).providers;
  const select = $("provider");
  const saved = localStorage.getItem("kcore-agent-provider");
  select.replaceChildren(...state.providers.filter((p) => p.connected).map((p) => el("option", { value: p.id, text: p.id })));
  if (saved && [...select.options].some((o) => o.value === saved)) select.value = saved;
  if (select.options.length === 0) select.append(el("option", { value: "", text: "no provider" }));
  await loadModels();
  renderProviderRows();
}

async function loadModels() {
  const select = $("model");
  select.replaceChildren();
  const id = chosenProvider();
  if (!id) return;
  const fallback = state.providers.find((p) => p.id === id)?.defaultModel;
  let models = [];
  try {
    models = (await api(`/api/providers/${id}/models`)).models;
  } catch {
    models = fallback ? [fallback] : [];
  }
  for (const model of models) select.append(el("option", { value: model, text: model }));
  const saved = localStorage.getItem(`kcore-agent-model-${id}`);
  if (saved && models.includes(saved)) select.value = saved;
  else if (fallback && models.includes(fallback)) select.value = fallback;
}

function renderProviderRows() {
  const rows = $("provider-rows");
  rows.replaceChildren();
  for (const provider of state.providers) {
    const keyInput = el("input", { type: "password", placeholder: "Paste API key", autocomplete: "off" });
    const form = el(
      "form",
      {
        onsubmit: async (event) => {
          event.preventDefault();
          if (!keyInput.value.trim()) return;
          await api(`/api/providers/${provider.id}/key`, { method: "PUT", body: JSON.stringify({ key: keyInput.value }) });
          keyInput.value = "";
          await loadProviders();
        },
      },
      keyInput,
      el("button", { type: "submit", text: provider.connected ? "Replace" : "Connect" }),
    );
    rows.append(
      el(
        "div",
        { class: "prow" },
        el("div", {}, el("div", { class: "name", text: provider.label }), el("div", { class: "muted", text: provider.connected ? `Connected (${provider.source === "env" ? "environment variable" : "stored key"})` : "Not connected" })),
        provider.connected && provider.source === "file"
          ? el("button", {
              text: "Remove",
              class: "danger",
              onclick: async () => {
                await api(`/api/providers/${provider.id}/key`, { method: "DELETE" });
                await loadProviders();
              },
            })
          : null,
        form,
      ),
    );
  }
}

async function loadCluster() {
  const summary = await api("/api/cluster/summary");
  const box = $("cluster");
  box.replaceChildren();
  if (summary.error) {
    box.append(el("div", { text: summary.error }));
    return;
  }
  box.append(el("div", {}, "Controller ", el("b", { text: summary.controller || "unknown" })));
  box.append(el("div", {}, "Identity ", el("b", { text: summary.operator || (summary.insecure ? "none (plaintext)" : "unknown") })));
}

$("new-session").addEventListener("click", () => newSession());
$("open-settings").addEventListener("click", () => $("settings").showModal());
$("close-settings").addEventListener("click", () => $("settings").close());
$("provider").addEventListener("change", () => {
  localStorage.setItem("kcore-agent-provider", chosenProvider());
  loadModels();
});
$("model").addEventListener("change", () => localStorage.setItem(`kcore-agent-model-${chosenProvider()}`, $("model").value));
$("cancel").addEventListener("click", () => api(`/api/sessions/${state.current}/cancel`, { method: "POST", body: "{}" }));
$("input").addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    $("composer").requestSubmit();
  }
});
$("composer").addEventListener("submit", async (event) => {
  event.preventDefault();
  const text = $("input").value.trim();
  if (!text || state.busy) return;
  if (!chosenProvider()) {
    $("settings").showModal();
    return;
  }
  $("input").value = "";
  try {
    await api(`/api/sessions/${state.current}/messages`, { method: "POST", body: JSON.stringify({ text, provider: chosenProvider(), model: $("model").value }) });
  } catch (e) {
    $("transcript").append(el("div", { class: "err-line", text: e.message }));
  }
});

async function start() {
  if (!token) {
    $("transcript").append(el("div", { class: "err-line", text: "No session token. Start this page from `kctl agent`." }));
    return;
  }
  await Promise.all([loadProviders(), loadCluster(), loadSessions()]);
  if (state.providers.every((p) => !p.connected)) $("settings").showModal();
  if (state.sessions.length > 0) await selectSession(state.sessions[0].id);
  else await newSession();
}

start().catch((e) => $("transcript").append(el("div", { class: "err-line", text: e.message })));
