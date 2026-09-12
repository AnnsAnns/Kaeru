/* kaeru app.js — vanilla client, no build step (C12: SSE over POST via
   fetch + ReadableStream). Talks to /api/* only; the provider key never
   reaches this file. Assistant Markdown is rendered to sanitized HTML by the
   server (ADR-026), so this file never parses Markdown. */

(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const messagesEl = $("messages");
  const composerEl = $("composer");
  const sendBtn = $("send-btn");
  const stopBtn = $("stop-btn");
  const regenBtn = $("regen-btn");
  const themeBtn = $("theme-btn");
  const themeNameEl = $("theme-name");
  const modelSelect = $("model-select");
  const effortSelect = $("effort-select");
  const fakeBadge = $("fake-badge");
  const usageBadge = $("usage-badge");
  const threadListEl = $("thread-list");
  const newThreadBtn = $("new-thread-btn");
  const threadsBtn = $("threads-btn");
  const workspaceEl = $("workspace");
  const memoryBtn = $("memory-btn");
  const memoryFormEl = $("memory-search");
  const memoryQueryEl = $("memory-query");
  const memoryListEl = $("memory-list");
  const memoryEmptyEl = $("memory-empty");
  const reflectBtn = $("reflect-btn");
  const reflectStatusEl = $("reflect-status");
  const attachBtn = $("attach-btn");
  const fileInput = $("file-input");
  const attachmentsEl = $("attachments");

  // Bort's theme cycle order (themes.ts enum); CSS additionally ships "trans".
  const THEMES = [
    "latenightbath",
    "ayy4",
    "curiosities",
    "sunnyswamp",
    "standard_og",
    "werwolvdark",
    "nostalgia",
  ];
  const THEME_KEY = "kaeru-theme";
  const AUTH_KEY = "kaeru-auth-token";
  const THREAD_KEY = "kaeru-thread";

  const state = {
    streaming: false,
    fetchCtrl: null,
    stopped: false,
    model: null,
    reasoningEffort: null,
    modelEffort: {},
    fake: false,
    sticky: true,
    usage: { in: 0, out: 0 },
    threadId: null,
    threads: [],
    // Workspace files uploaded for the next message (M5).
    attachments: [],
  };

  // Blob URLs created for workspace files (images/downloads); revoked when the
  // thread view is replaced so they do not pile up.
  const objectUrls = [];
  function releaseObjectUrls() {
    for (const url of objectUrls) URL.revokeObjectURL(url);
    objectUrls.length = 0;
  }

  /* ---------- theme (Bort switcher port: cycle + localStorage) ---------- */

  function applyTheme(theme) {
    document.documentElement.dataset.theme = theme;
    themeNameEl.textContent = theme;
  }
  function loadTheme() {
    const stored = localStorage.getItem(THEME_KEY);
    return THEMES.includes(stored) ? stored : THEMES[0];
  }
  themeBtn.addEventListener("click", () => {
    const next = THEMES[(THEMES.indexOf(loadTheme()) + 1) % THEMES.length];
    localStorage.setItem(THEME_KEY, next);
    applyTheme(next);
  });

  /* ---------- auth token (M2: second layer behind CF Access) ---------- */

  // Every /api/* call carries the shared secret when one is stored. It is
  // entered once per browser (see showTokenPrompt) and never leaves the
  // browser except as this header — the provider key never reaches it.

  // A rejected request is handled once, here: show the token prompt and stop
  // the caller with a marker the UI silently ignores. Callers may pass their
  // own retry (default: bootstrap).
  class AuthRequired extends Error {
    constructor() {
      super("auth required");
      this.name = "AuthRequired";
    }
  }

  async function apiFetch(path, options = {}, onAuth) {
    const token = localStorage.getItem(AUTH_KEY);
    if (token) {
      options.headers = { ...(options.headers || {}), "x-auth-token": token };
    }
    const response = await fetch(path, options);
    if (response.status === 401) {
      showTokenPrompt(onAuth || (() => bootstrap()));
      throw new AuthRequired();
    }
    return response;
  }

  function showTokenPrompt(retry) {
    removeEmptyHint();
    const { body } = addBox("error", "access");
    const text = document.createElement("p");
    text.textContent =
      "This server requires its shared secret (the auth_token from its config). " +
      "Paste it once — it is stored only in this browser.";
    const row = document.createElement("div");
    row.className = "token-row";
    const input = document.createElement("input");
    input.type = "password";
    input.className = "token-input";
    input.placeholder = "auth token";
    input.autocomplete = "off";
    input.spellcheck = false;
    const save = document.createElement("button");
    save.type = "button";
    save.className = "action-btn";
    save.textContent = "save";
    save.addEventListener("click", () => {
      const value = input.value.trim();
      if (!value) return;
      localStorage.setItem(AUTH_KEY, value);
      retry();
    });
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") save.click();
    });
    row.append(input, save);
    body.append(text, row);
    scrollToBottom(true);
    input.focus();
  }

  /* ---------- rendering ---------- */

  function isNearBottom() {
    return messagesEl.scrollHeight - messagesEl.scrollTop - messagesEl.clientHeight < 80;
  }
  messagesEl.addEventListener("scroll", () => {
    state.sticky = isNearBottom();
  });
  function scrollToBottom(force) {
    if (force || state.sticky) messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function addBox(role, label) {
    const box = document.createElement("article");
    box.className = `box msg ${role}`;
    const titleBar = document.createElement("div");
    titleBar.className = "box-title";
    const labelEl = document.createElement("span");
    labelEl.className = "title-label";
    labelEl.textContent = label;
    const chrome = document.createElement("span");
    chrome.className = "title-chrome";
    chrome.textContent = "➖ ⏹️ ❌";
    chrome.setAttribute("aria-hidden", "true");
    titleBar.append(labelEl, chrome);
    const body = document.createElement("div");
    body.className = "msg-body";
    box.append(titleBar, body);
    messagesEl.append(box);
    return { box, body, label: labelEl };
  }

  let emptyHint = null;
  function removeEmptyHint() {
    if (emptyHint) {
      emptyHint.remove();
      emptyHint = null;
    }
  }
  function showEmptyHint() {
    const hint = addBox("kaeru", "kaeru");
    hint.body.textContent = "hi! pick a thread or type something below and hit send — I stream token by token.";
    emptyHint = hint.box;
  }

  function addErrorBox(message, retryMessage) {
    const { body } = addBox("error", "error");
    const text = document.createElement("p");
    text.textContent = message;
    body.append(text);
    if (retryMessage) {
      const retry = document.createElement("button");
      retry.type = "button";
      retry.className = "chip retry-btn";
      retry.textContent = "retry";
      retry.addEventListener("click", () => {
        retry.disabled = true;
        sendMessage(retryMessage);
      });
      body.append(retry);
    }
    scrollToBottom();
  }

  // Surface an error unless its prompt is already on screen (401).
  function reportError(err) {
    if (err && err.name === "AuthRequired") return;
    addErrorBox(err.message || String(err));
  }

  function fmtUsage(usage) {
    if (!usage) return "";
    const parts = [];
    if (usage.input_tokens != null) parts.push(`${usage.input_tokens} in`);
    if (usage.output_tokens != null) parts.push(`${usage.output_tokens} out`);
    return parts.length ? ` · ${parts.join(" · ")}` : "";
  }

  function updateUsageBadge() {
    const parts = [];
    if (state.usage.in) parts.push(`${state.usage.in} in`);
    if (state.usage.out) parts.push(`${state.usage.out} out`);
    if (!parts.length) {
      usageBadge.hidden = true;
      return;
    }
    usageBadge.textContent = `tokens: ${parts.join(" · ")}`;
    usageBadge.hidden = false;
  }

  /* ---------- SSE parsing (mirror of the server framing) ---------- */

  function createSseParser() {
    let buf = "";
    return (chunk) => {
      buf += chunk;
      const frames = [];
      let idx;
      while ((idx = buf.indexOf("\n\n")) !== -1) {
        frames.push(buf.slice(0, idx));
        buf = buf.slice(idx + 2);
      }
      return frames.map(parseFrame).filter(Boolean);
    };
  }
  function parseFrame(frame) {
    const dataLines = [];
    for (const raw of frame.split("\n")) {
      const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
      if (line.startsWith(":")) continue; // keep-alive comment
      if (line.startsWith("data:")) dataLines.push(line.slice(5).replace(/^ /, ""));
    }
    if (!dataLines.length) return null;
    return dataLines.join("\n");
  }

  async function readApiError(response) {
    try {
      const body = await response.json();
      return new Error(body.error?.message || `${response.status} ${response.statusText}`);
    } catch {
      return new Error(`${response.status} ${response.statusText}`);
    }
  }

  /* ---------- threads (M2.5, §6.7) ---------- */

  function fmtTime(iso) {
    const date = new Date(iso);
    if (Number.isNaN(date.getTime())) return "";
    return date.toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
  }

  function renderThreadList() {
    threadListEl.replaceChildren();
    for (const thread of state.threads) {
      const item = document.createElement("li");
      item.className = "thread-item";
      if (thread.id === state.threadId) item.classList.add("active");

      const open = document.createElement("button");
      open.type = "button";
      open.className = "thread-open";
      const title = document.createElement("span");
      title.className = "thread-title";
      title.textContent = thread.title || "untitled";
      const meta = document.createElement("span");
      meta.className = "thread-meta";
      meta.textContent = `${thread.messageCount} msg · ${fmtTime(thread.updatedAt)}`;
      open.append(title, meta);
      open.addEventListener("click", () => selectThread(thread.id));

      const del = document.createElement("button");
      del.type = "button";
      del.className = "thread-delete";
      del.title = "Delete thread";
      del.textContent = "🗑";
      del.addEventListener("click", (event) => {
        event.stopPropagation();
        deleteThread(thread.id).catch(reportError);
      });

      item.append(open, del);
      threadListEl.append(item);
    }
  }

  async function loadThreads() {
    const response = await apiFetch("/api/threads");
    if (!response.ok) throw await readApiError(response);
    const body = await response.json();
    state.threads = body.threads || [];
    renderThreadList();
  }

  function renderThread(payload) {
    state.threadId = payload.id;
    localStorage.setItem(THREAD_KEY, payload.id);
    state.usage = {
      in: payload.usage?.input_tokens || 0,
      out: payload.usage?.output_tokens || 0,
    };
    updateUsageBadge();
    fakeBadge.hidden = !payload.fake;
    if (payload.model) {
      state.model = payload.model;
      syncModelSelect();
    }
    state.reasoningEffort = payload.reasoning_effort || null;
    syncEffortSelect();

    messagesEl.replaceChildren();
    releaseObjectUrls();
    emptyHint = null;
    if (payload.summary) {
      const box = addBox("kaeru", "kaeru · summary");
      box.body.textContent = payload.summary;
    }
    for (const message of payload.history || []) {
      renderMessage(message);
    }
    if ((payload.history || []).length || payload.summary) {
      scrollToBottom(true);
    } else {
      showEmptyHint();
    }
    renderThreadList();
    // A turn may still be running server-side: re-attach and replay it (M3).
    if (payload.active) {
      attachStream().catch(() => {});
    }
  }

  /* Model thinking: a collapsible block above the answer. Shown live while a
     turn streams, then folded away (but re-openable) once the answer starts. */
  function thinkingBlock(text, open) {
    const details = document.createElement("details");
    details.className = "thinking";
    if (open) details.open = true;
    const summary = document.createElement("summary");
    summary.textContent = "thinking";
    const body = document.createElement("div");
    body.className = "thinking-body";
    body.textContent = text || "";
    details.append(summary, body);
    return details;
  }

  function renderMessage(message) {
    const role = message.role;
    // Tool results (M3) render as their own compact card, not a chat bubble.
    if (role === "tool") {
      const box = addBox("tool", "🔧 tool result");
      const pre = document.createElement("pre");
      pre.className = "tool-body";
      pre.textContent = message.content || "";
      box.body.append(pre);
      return box;
    }
    const mine = role === "user";
    const box = addBox(mine ? "you" : "kaeru", mine ? "you" : "kaeru");
    if (mine) {
      box.body.textContent = message.content;
    } else if (message.html) {
      box.body.classList.add("markdown");
      box.body.innerHTML = message.html;
    } else {
      box.body.textContent = message.content;
    }
    if (!mine && message.reasoning) {
      box.body.prepend(thinkingBlock(message.reasoning, false));
    }
    // The assistant's tool requests (M3) show as collapsed steps below.
    if (!mine && Array.isArray(message.tool_calls)) {
      const steps = document.createElement("div");
      steps.className = "steps";
      for (const call of message.tool_calls) {
        steps.append(toolCardFromCall(call));
      }
      box.body.append(steps);
    }
    return box;
  }

  async function selectThread(id) {
    if (state.streaming) await stop();
    const response = await apiFetch(`/api/threads/${encodeURIComponent(id)}`);
    if (response.status === 404) {
      localStorage.removeItem(THREAD_KEY);
      await bootstrap();
      return;
    }
    if (!response.ok) throw await readApiError(response);
    renderThread(await response.json());
  }

  async function createThread() {
    if (state.streaming) await stop();
    const response = await apiFetch("/api/threads", { method: "POST" });
    if (!response.ok) throw await readApiError(response);
    const body = await response.json();
    await selectThread(body.id);
    return body.id;
  }

  async function deleteThread(id) {
    const response = await apiFetch(`/api/threads/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
    if (!response.ok && response.status !== 404) throw await readApiError(response);
    const wasSelected = id === state.threadId;
    await loadThreads();
    if (!wasSelected) return;
    state.threadId = null;
    if (state.threads.length) await selectThread(state.threads[0].id);
    else await createThread();
  }

  /* ---------- chat ---------- */

  function setBusy(busy) {
    state.streaming = busy;
    sendBtn.hidden = busy;
    stopBtn.hidden = !busy;
    sendBtn.disabled = busy;
    regenBtn.disabled = busy;
  }

  /* A fresh assistant box wired for streaming: thinking block, live text,
     a steps container (tool + approval cards), and a caret. */
  function newTurnView(label) {
    const ai = addBox("kaeru", label || "kaeru");
    const thinking = thinkingBlock("", true);
    thinking.hidden = true;
    const thinkingBody = thinking.querySelector(".thinking-body");
    const textNode = document.createTextNode("");
    const steps = document.createElement("div");
    steps.className = "steps";
    const cursor = document.createElement("span");
    cursor.className = "cursor";
    ai.body.append(thinking, textNode, steps, cursor);
    const turn = {
      ai,
      thinking,
      thinkingBody,
      textNode,
      steps,
      cursor,
      cards: {},
      text: "",
      reasoning: "",
      usage: null,
      terminal: false,
      errorMsg: null,
      aborted: false,
      auth: false,
    };
    turn.paint = () => {
      if (turn.reasoning) {
        thinking.hidden = false;
        thinkingBody.textContent = turn.reasoning;
      }
      textNode.data = turn.text;
      scrollToBottom();
    };
    return turn;
  }

  /* Read an SSE response body into the turn view until a terminal event. */
  async function readStream(response, turn) {
    const parseChunk = createSseParser();
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      for (const data of parseChunk(decoder.decode(value, { stream: true }))) {
        handleEvent(JSON.parse(data), turn);
        if (turn.terminal) break;
      }
      if (turn.terminal) {
        try {
          reader.cancel();
        } catch {
          /* already closed */
        }
        break;
      }
    }
  }

  /* Shared finish: label the box, reload the thread on success. */
  async function finishTurn(turn, message) {
    turn.cursor.remove();
    state.fetchCtrl = null;
    setBusy(false);

    if (turn.errorMsg) {
      if (!turn.text) turn.ai.box.remove();
      turn.ai.label.textContent = "kaeru · error";
      addErrorBox(turn.errorMsg, message);
    } else if (turn.auth) {
      // The shared-secret prompt is already on screen; drop the empty box.
      turn.ai.box.remove();
    } else if (turn.aborted || (state.stopped && !turn.terminal)) {
      turn.ai.label.textContent = "kaeru · stopped";
    } else if (!turn.terminal) {
      // The turn is still running server-side: re-attach and replay it (M3).
      turn.ai.label.textContent = "kaeru · reconnecting…";
      setTimeout(() => attachStream(turn).catch(() => {}), 1000);
    } else {
      // Completed: swap the streamed plain text for the server's rendered
      // HTML (the same renderer used on reload).
      try {
        await selectThread(state.threadId);
      } catch {
        /* keep the plain-text fallback already on screen */
      }
    }
    turn.ai.label.textContent += fmtUsage(turn.usage);
    scrollToBottom();
    // The sidebar re-sorts by updatedAt and picks up the auto title.
    loadThreads().catch(() => {});
  }

  async function sendMessage(text) {
    const typed = text.trim();
    if (!typed || state.streaming) return;
    if (!state.threadId) await createThread();

    // Uploaded workspace files ride along with the message as a plain note,
    // so the model knows they exist (and that they live in the workspace).
    const attached = state.attachments.slice();
    const message = attached.length
      ? `${typed}\n\n[attached in the workspace: ${attached.join(", ")}]`
      : typed;
    state.attachments = [];
    renderAttachments();

    composerEl.value = "";
    autosize();
    setBusy(true);
    state.stopped = false;
    removeEmptyHint();

    addBox("you", "you").body.textContent = message;
    scrollToBottom(true);
    state.sticky = true;

    const turn = newTurnView("kaeru");
    const ctrl = new AbortController();
    state.fetchCtrl = ctrl;

    try {
      const payload = { message, thread: state.threadId };
      if (state.model) payload.model = state.model;
      // "" clears any per-thread override; the server then uses its default.
      payload.reasoning_effort = state.reasoningEffort || "";
      const response = await apiFetch("/api/chat", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(payload),
        signal: ctrl.signal,
      });
      if (!response.ok) throw await readApiError(response);
      await readStream(response, turn);
    } catch (err) {
      if (err.name === "AbortError") {
        turn.aborted = true;
      } else if (err.name === "AuthRequired") {
        turn.auth = true;
      } else {
        turn.errorMsg = err.message || String(err);
      }
    } finally {
      await finishTurn(turn, message);
    }
  }

  /* Regenerate the last answer (M3): the server drops the old reply and
     re-runs the last user message. */
  async function regenerate() {
    if (state.streaming || !state.threadId) return;
    setBusy(true);
    state.stopped = false;
    const turn = newTurnView("kaeru · regenerating");
    try {
      const query = `?thread=${encodeURIComponent(state.threadId)}`;
      const response = await apiFetch(`/api/regenerate${query}`, { method: "POST" });
      if (!response.ok) throw await readApiError(response);
      await readStream(response, turn);
    } catch (err) {
      if (err.name === "AuthRequired") turn.auth = true;
      else turn.errorMsg = err.message || String(err);
    } finally {
      await finishTurn(turn, null);
    }
  }

  /* Re-attach to the active turn after a reconnect/reload (M3, §6.3a). */
  async function attachStream(previous) {
    if (state.streaming || !state.threadId) return;
    let response;
    try {
      response = await apiFetch(`/api/stream?thread=${encodeURIComponent(state.threadId)}`);
    } catch {
      return;
    }
    if (response.status === 204 || !response.ok) {
      // The turn finished between scheduling and re-attach: say so instead of
      // leaving the previous box labelled "reconnecting…".
      if (previous) previous.ai.label.textContent = "kaeru · finished";
      return;
    }
    setBusy(true);
    const turn = newTurnView("kaeru · resuming");
    try {
      await readStream(response, turn);
    } catch (err) {
      if (err.name === "AuthRequired") turn.auth = true;
      else turn.errorMsg = err.message || String(err);
    } finally {
      await finishTurn(turn, null);
    }
  }

  function toolCardFromCall(call) {
    const details = document.createElement("details");
    details.className = "tool";
    const summary = document.createElement("summary");
    summary.textContent = `🔧 ${call.name}`;
    const body = document.createElement("pre");
    body.className = "tool-body";
    body.textContent =
      typeof call.arguments === "string"
        ? call.arguments
        : JSON.stringify(call.arguments ?? {}, null, 2);
    details.append(summary, body);
    return details;
  }

  function addApprovalCard(turn, event) {
    const card = document.createElement("div");
    card.className = "approval";
    const text = document.createElement("p");
    text.className = "approval-text";
    text.textContent = event.summary || "This action needs your consent.";
    const row = document.createElement("div");
    row.className = "approval-row";
    const allow = document.createElement("button");
    allow.type = "button";
    allow.className = "action-btn";
    allow.textContent = "allow";
    const deny = document.createElement("button");
    deny.type = "button";
    deny.className = "action-btn stop";
    deny.textContent = "deny";
    const decide = async (decision) => {
      allow.disabled = true;
      deny.disabled = true;
      try {
        const response = await apiFetch("/api/approval", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ id: event.id, decision, thread: state.threadId }),
        });
        if (!response.ok && response.status !== 404) throw await readApiError(response);
        card.classList.add(decision === "allow" ? "allowed" : "denied");
        text.textContent = `${
          decision === "allow" ? "allowed" : "denied"
        }: ${event.summary || ""}`;
        row.remove();
      } catch (err) {
        if (err.name !== "AuthRequired") {
          text.textContent = `could not record decision: ${err.message || err}`;
        }
        allow.disabled = false;
        deny.disabled = false;
      }
    };
    allow.addEventListener("click", () => decide("allow"));
    deny.addEventListener("click", () => decide("deny"));
    row.append(allow, deny);
    card.append(text, row);
    turn.steps.append(card);
    scrollToBottom();
  }

  /* ---------- workspace files (M5) ---------- */

  /* Fetch a workspace file with the auth header and expose it as a blob URL;
     an <img>/<a> tag cannot carry a custom header, so this indirection is
     what keeps file serving authenticated (ADR-017). */
  async function fileObjectUrl(path) {
    const encoded = path.split("/").map(encodeURIComponent).join("/");
    const response = await apiFetch(`/api/files/${encoded}`);
    if (!response.ok) throw await readApiError(response);
    const blob = await response.blob();
    const url = URL.createObjectURL(blob);
    objectUrls.push(url);
    return url;
  }

  async function downloadArtifact(path) {
    try {
      const url = await fileObjectUrl(path);
      const link = document.createElement("a");
      link.href = url;
      link.download = path.split("/").pop() || "file";
      document.body.append(link);
      link.click();
      link.remove();
    } catch (err) {
      reportError(err);
    }
  }

  /* Artifact events (M5): images render inline, everything else becomes a
     download card. */
  function addArtifactCard(turn, event) {
    const card = document.createElement("div");
    card.className = "artifact";
    const name = document.createElement("span");
    name.className = "artifact-name";
    name.textContent = `📎 ${event.path}`;
    const download = document.createElement("button");
    download.type = "button";
    download.className = "artifact-download";
    download.textContent = "download";
    download.addEventListener("click", () => downloadArtifact(event.path));
    card.append(name, download);
    if ((event.mime_hint || "").startsWith("image/")) {
      const image = document.createElement("img");
      image.className = "artifact-image";
      image.alt = event.path;
      image.loading = "lazy";
      fileObjectUrl(event.path)
        .then((url) => {
          image.src = url;
        })
        .catch(() => {
          /* the card remains downloadable */
        });
      card.append(image);
    }
    turn.steps.append(card);
    scrollToBottom();
  }

  function uploadFile(file) {
    if (!file) return;
    apiFetch(`/api/files?name=${encodeURIComponent(file.name)}`, {
      method: "POST",
      headers: { "content-type": file.type || "application/octet-stream" },
      body: file,
    })
      .then(async (response) => {
        if (!response.ok) throw await readApiError(response);
        const body = await response.json();
        state.attachments.push(body.path);
        renderAttachments();
      })
      .catch(reportError)
      .finally(() => {
        fileInput.value = "";
      });
  }

  function renderAttachments() {
    attachmentsEl.replaceChildren();
    attachmentsEl.hidden = state.attachments.length === 0;
    for (const path of state.attachments) {
      const chip = document.createElement("span");
      chip.className = "attachment-chip";
      const label = document.createElement("span");
      label.textContent = `📎 ${path}`;
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "attachment-remove";
      remove.title = "Remove";
      remove.textContent = "✕";
      remove.addEventListener("click", () => {
        state.attachments = state.attachments.filter((kept) => kept !== path);
        renderAttachments();
      });
      chip.append(label, remove);
      attachmentsEl.append(chip);
    }
  }

  function handleEvent(event, turn) {
    switch (event.type) {
      case "delta":
        turn.text += event.text;
        // Fold the live thinking block away once the answer starts; it stays
        // available behind the summary triangle.
        if (turn.thinking && turn.reasoning && turn.thinking.open) {
          turn.thinking.open = false;
        }
        turn.paint();
        break;
      case "reasoning":
        turn.reasoning += event.text;
        turn.paint();
        break;
      case "tool_call": {
        const card = toolCardFromCall(event);
        turn.cards[event.id] = card;
        turn.steps.append(card);
        scrollToBottom();
        break;
      }
      case "tool_result": {
        const card = turn.cards[event.id];
        if (card) {
          card.classList.toggle("tool-error", !!event.is_error);
          const body = card.querySelector(".tool-body");
          if (body) body.textContent = event.output || "";
          if (event.is_error) card.open = true;
        }
        scrollToBottom();
        break;
      }
      case "approval_request":
        addApprovalCard(turn, event);
        break;
      case "artifact":
        addArtifactCard(turn, event);
        break;
      case "turn_done":
        turn.terminal = true;
        turn.usage = event.usage;
        if (event.usage) {
          state.usage.in += event.usage.input_tokens || 0;
          state.usage.out += event.usage.output_tokens || 0;
          updateUsageBadge();
        }
        // A memory_write may just have landed; keep an open browser fresh.
        refreshMemoryIfOpen();
        break;
      case "error":
        turn.terminal = true;
        if (event.kind === "aborted") {
          turn.aborted = true;
        } else {
          turn.errorMsg = event.message;
        }
        break;
      default:
        break;
    }
  }

  async function stop() {
    if (!state.streaming) return;
    state.stopped = true;
    try {
      const query = state.threadId ? `?thread=${encodeURIComponent(state.threadId)}` : "";
      await apiFetch(`/api/abort${query}`, {
        method: "POST",
        signal: AbortSignal.timeout(2000),
      });
      // The stream itself will deliver the terminal aborted event.
    } catch {
      // Abort request failed (network hiccup): abort locally instead; the
      // core keeps the turn alive until its replay buffer is abandoned.
      state.fetchCtrl?.abort();
    }
  }

  /* ---------- composer ---------- */

  function autosize() {
    composerEl.style.height = "auto";
    composerEl.style.height = `${Math.min(composerEl.scrollHeight, innerHeight * 0.3)}px`;
  }
  composerEl.addEventListener("input", autosize);
  composerEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey) {
      // Enter sends on desktop keyboards; on touch devices enter makes a
      // newline and the send button does the work.
      if (!matchMedia("(pointer: coarse)").matches) {
        event.preventDefault();
        sendMessage(composerEl.value);
      }
    }
  });
  sendBtn.addEventListener("click", () => sendMessage(composerEl.value));
  stopBtn.addEventListener("click", stop);
  attachBtn.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => uploadFile(fileInput.files[0]));
  regenBtn.addEventListener("click", () => {
    regenerate().catch(reportError);
  });
  newThreadBtn.addEventListener("click", () => {
    createThread().catch(reportError);
  });
  threadsBtn.addEventListener("click", () => {
    const open = workspaceEl.classList.toggle("sidebar-open");
    threadsBtn.setAttribute("aria-expanded", String(open));
  });

  /* ---------- memory browser (M4) ---------- */

  function renderMemory(notes) {
    memoryListEl.replaceChildren();
    memoryEmptyEl.hidden = notes.length > 0;
    for (const note of notes) {
      const item = document.createElement("li");
      item.className = "memory-note";
      const head = document.createElement("div");
      head.className = "memory-note-head";
      const day = document.createElement("span");
      day.textContent = note.day;
      head.append(day);
      for (const tag of note.tags || []) {
        const tagEl = document.createElement("span");
        tagEl.className = "memory-tag";
        tagEl.textContent = `#${tag}`;
        head.append(tagEl);
      }
      const body = document.createElement("p");
      body.className = "memory-note-body";
      body.textContent = note.content || "";
      item.append(head, body);
      memoryListEl.append(item);
    }
  }

  async function loadMemory(query) {
    const path = query
      ? `/api/memory?q=${encodeURIComponent(query)}`
      : "/api/memory";
    const response = await apiFetch(path, {}, () => loadMemory(query));
    if (!response.ok) throw await readApiError(response);
    const body = await response.json();
    renderMemory(body.notes || []);
  }

  function refreshMemoryIfOpen() {
    if (workspaceEl.classList.contains("memory-open")) {
      loadMemory(memoryQueryEl.value.trim()).catch(() => {});
    }
  }

  memoryBtn.addEventListener("click", () => {
    const open = workspaceEl.classList.toggle("memory-open");
    memoryBtn.setAttribute("aria-expanded", String(open));
    if (open) {
      loadMemory(memoryQueryEl.value.trim()).catch(reportError);
    }
  });
  memoryFormEl.addEventListener("submit", (event) => {
    event.preventDefault();
    loadMemory(memoryQueryEl.value.trim()).catch(reportError);
  });

  // Evening reflection on demand (M4.5): the same digest the nightly job runs.
  function refreshReflectStatus(outcome) {
    reflectStatusEl.hidden = false;
    if (outcome.status === "disabled") {
      reflectStatusEl.textContent = "reflection is off — enable [reflect] in config";
      return;
    }
    const parts = [
      `${outcome.notes} note(s) from ${outcome.conversations} conversation(s)`,
    ];
    if (outcome.persona_changed) parts.push("persona nudged");
    reflectStatusEl.textContent = parts.join(" · ");
  }
  reflectBtn.addEventListener("click", async () => {
    reflectBtn.disabled = true;
    try {
      const response = await apiFetch(
        "/api/reflect",
        { method: "POST" },
        () => reflectBtn.click(),
      );
      if (!response.ok) throw await readApiError(response);
      const outcome = await response.json();
      refreshReflectStatus(outcome);
      loadMemory(memoryQueryEl.value.trim()).catch(() => {});
    } catch (err) {
      reportError(err);
    } finally {
      reflectBtn.disabled = false;
    }
  });

  /* ---------- models ---------- */

  // Keep the active model selectable even when the provider's /models list
  // omits it (a partial or filtered list must not blank the dropdown).
  function ensureModelOption(id) {
    if (!id) return;
    for (const child of modelSelect.children) {
      if (child.value === id) return;
    }
    const option = document.createElement("option");
    option.value = id;
    option.textContent = id;
    modelSelect.append(option);
  }

  function syncModelSelect() {
    if (!state.model) return;
    ensureModelOption(state.model);
    modelSelect.value = state.model;
  }

  // The effort selector is shown only for models that advertise reasoning
  // levels (else hidden); "" means "use the provider default".
  function syncEffortSelect() {
    const levels = state.modelEffort[state.model]?.levels || [];
    effortSelect.replaceChildren();
    if (!levels.length) {
      effortSelect.hidden = true;
      state.reasoningEffort = null;
      return;
    }
    const auto = document.createElement("option");
    auto.value = "";
    auto.textContent = "effort: default";
    effortSelect.append(auto);
    for (const level of levels) {
      const option = document.createElement("option");
      option.value = level;
      option.textContent = `effort: ${level}`;
      effortSelect.append(option);
    }
    if (state.reasoningEffort && levels.includes(state.reasoningEffort)) {
      effortSelect.value = state.reasoningEffort;
    } else {
      effortSelect.value = "";
      state.reasoningEffort = null;
    }
    effortSelect.hidden = false;
  }

  async function loadModels() {
    const response = await apiFetch("/api/models");
    // Not fatal: the selected thread still names the active model, which
    // syncModelSelect keeps as an option.
    if (!response.ok) return;
    const body = await response.json();
    modelSelect.replaceChildren();
    state.modelEffort = {};
    for (const model of body.models || []) {
      const option = document.createElement("option");
      option.value = model.id;
      option.textContent = model.id;
      modelSelect.append(option);
      state.modelEffort[model.id] = {
        levels: model.reasoning_effort_levels || [],
        default: model.default_reasoning_effort || null,
      };
    }
    syncModelSelect();
    syncEffortSelect();
  }
  modelSelect.addEventListener("change", () => {
    state.model = modelSelect.value;
    // A new model may support a different set of effort levels.
    state.reasoningEffort = state.modelEffort[state.model]?.default || null;
    syncEffortSelect();
  });
  effortSelect.addEventListener("change", () => {
    state.reasoningEffort = effortSelect.value || null;
  });

  /* ---------- bootstrap ---------- */

  async function bootstrap() {
    applyTheme(loadTheme());
    try {
      await loadModels();
    } catch {
      // The model list is a nicety; a failure here must never block the
      // threads sidebar or the chat itself.
    }

    const saved = localStorage.getItem(THREAD_KEY);
    if (saved) {
      const response = await apiFetch(`/api/threads/${encodeURIComponent(saved)}`);
      if (response.ok) {
        renderThread(await response.json());
        await loadThreads();
        return;
      }
      localStorage.removeItem(THREAD_KEY);
    }

    await loadThreads();
    if (state.threads.length) await selectThread(state.threads[0].id);
    else await createThread();
  }

  bootstrap().catch(reportError);
})();
