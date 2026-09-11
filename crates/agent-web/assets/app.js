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
  const themeBtn = $("theme-btn");
  const themeNameEl = $("theme-name");
  const modelSelect = $("model-select");
  const fakeBadge = $("fake-badge");
  const usageBadge = $("usage-badge");
  const threadListEl = $("thread-list");
  const newThreadBtn = $("new-thread-btn");
  const threadsBtn = $("threads-btn");
  const workspaceEl = $("workspace");

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
    fake: false,
    sticky: true,
    usage: { in: 0, out: 0 },
    threadId: null,
    threads: [],
  };

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
  function apiFetch(path, options = {}) {
    const token = localStorage.getItem(AUTH_KEY);
    if (token) {
      options.headers = { ...(options.headers || {}), "x-auth-token": token };
    }
    return fetch(path, options);
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
        deleteThread(thread.id);
      });

      item.append(open, del);
      threadListEl.append(item);
    }
  }

  async function loadThreads() {
    const response = await apiFetch("/api/threads");
    if (response.status === 401) {
      showTokenPrompt(() => bootstrap());
      return;
    }
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

    messagesEl.replaceChildren();
    emptyHint = null;
    if (payload.summary) {
      const box = addBox("kaeru", "kaeru · summary");
      box.body.textContent = payload.summary;
    }
    for (const message of payload.history || []) {
      renderMessage(message.role, message.content, message.html);
    }
    if ((payload.history || []).length || payload.summary) {
      scrollToBottom(true);
    } else {
      showEmptyHint();
    }
    renderThreadList();
  }

  function renderMessage(role, content, html) {
    const mine = role === "user";
    const box = addBox(mine ? "you" : "kaeru", mine ? "you" : "kaeru");
    if (mine) {
      box.body.textContent = content;
    } else if (html) {
      box.body.classList.add("markdown");
      box.body.innerHTML = html;
    } else {
      box.body.textContent = content;
    }
    return box;
  }

  async function selectThread(id) {
    if (state.streaming) await stop();
    const response = await apiFetch(`/api/threads/${encodeURIComponent(id)}`);
    if (response.status === 401) {
      showTokenPrompt(() => bootstrap());
      return;
    }
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
  }

  async function sendMessage(text) {
    const message = text.trim();
    if (!message || state.streaming) return;
    if (!state.threadId) await createThread();

    composerEl.value = "";
    autosize();
    setBusy(true);
    state.stopped = false;
    removeEmptyHint();

    addBox("you", "you").body.textContent = message;
    scrollToBottom(true);
    state.sticky = true;

    const ai = addBox("kaeru", "kaeru");
    const textNode = document.createTextNode("");
    ai.body.append(textNode);
    const cursor = document.createElement("span");
    cursor.className = "cursor";
    ai.body.append(cursor);

    const turn = { text: "", usage: null, terminal: false, errorMsg: null, aborted: false };
    const ctrl = new AbortController();
    state.fetchCtrl = ctrl;

    const paint = () => {
      textNode.data = turn.text;
      scrollToBottom();
    };

    try {
      const payload = { message, thread: state.threadId };
      if (state.model) payload.model = state.model;
      const response = await apiFetch("/api/chat", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(payload),
        signal: ctrl.signal,
      });
      if (!response.ok) throw await readApiError(response);

      const parseChunk = createSseParser();
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        for (const data of parseChunk(decoder.decode(value, { stream: true }))) {
          handleEvent(JSON.parse(data), turn, paint);
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
    } catch (err) {
      if (err.name === "AbortError") {
        turn.aborted = true;
      } else {
        turn.errorMsg = err.message || String(err);
      }
    } finally {
      cursor.remove();
      state.fetchCtrl = null;
      setBusy(false);

      if (turn.errorMsg) {
        if (!turn.text) ai.box.remove();
        ai.label.textContent = "kaeru · error";
        addErrorBox(turn.errorMsg, message);
      } else if (turn.aborted || (state.stopped && !turn.terminal)) {
        ai.label.textContent = "kaeru · stopped";
      } else if (!turn.terminal) {
        ai.label.textContent = "kaeru · connection lost";
      } else {
        // Completed: swap the streamed plain text for the server's rendered
        // HTML (the same renderer used on reload).
        try {
          await selectThread(state.threadId);
        } catch {
          /* keep the plain-text fallback already on screen */
        }
      }
      ai.label.textContent += fmtUsage(turn.usage);
      scrollToBottom();
      // The sidebar re-sorts by updatedAt and picks up the auto title.
      loadThreads().catch(() => {});
    }
  }

  function handleEvent(event, turn, paint) {
    switch (event.type) {
      case "delta":
        turn.text += event.text;
        paint();
        break;
      case "turn_done":
        turn.terminal = true;
        turn.usage = event.usage;
        if (event.usage) {
          state.usage.in += event.usage.input_tokens || 0;
          state.usage.out += event.usage.output_tokens || 0;
          updateUsageBadge();
        }
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
        // tool_call / approval cards render from M3 on.
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
      // server aborts the turn when its subscriber disappears (M1 wiring).
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
  newThreadBtn.addEventListener("click", () => {
    createThread().catch((err) => addErrorBox(err.message || String(err)));
  });
  threadsBtn.addEventListener("click", () => {
    const open = workspaceEl.classList.toggle("sidebar-open");
    threadsBtn.setAttribute("aria-expanded", String(open));
  });

  /* ---------- models ---------- */

  function syncModelSelect() {
    if (state.model) modelSelect.value = state.model;
  }

  async function loadModels() {
    const response = await apiFetch("/api/models");
    if (!response.ok) return;
    const body = await response.json();
    modelSelect.replaceChildren();
    for (const model of body.models || []) {
      const option = document.createElement("option");
      option.value = model.id;
      option.textContent = model.id;
      modelSelect.append(option);
    }
    syncModelSelect();
  }
  modelSelect.addEventListener("change", () => {
    state.model = modelSelect.value;
  });

  /* ---------- bootstrap ---------- */

  async function bootstrap() {
    applyTheme(loadTheme());
    await loadModels();

    const saved = localStorage.getItem(THREAD_KEY);
    if (saved) {
      const response = await apiFetch(`/api/threads/${encodeURIComponent(saved)}`);
      if (response.status === 401) {
        showTokenPrompt(() => bootstrap());
        return;
      }
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

  bootstrap().catch((err) => {
    addErrorBox(err.message || String(err));
  });
})();
