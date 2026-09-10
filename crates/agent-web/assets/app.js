/* kaeru app.js — vanilla client (C12: SSE over POST via fetch + ReadableStream).
   No framework, no build step. Talk to /api/* only; the provider key never
   reaches this file. */

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

  const state = {
    streaming: false,
    fetchCtrl: null,
    stopped: false,
    model: null,
    fake: false,
    sticky: true,
    usage: { in: 0, out: 0 },
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
    hint.body.textContent = "hi! type something below and hit send — I stream token by token.";
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

  /* ---------- accumulated usage (M2) ---------- */

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

    const applyText = () => {
      textNode.data = turn.text;
      scrollToBottom();
    };

    try {
      const payload = { message };
      if (state.model) payload.model = state.model;
      const response = await fetch("/api/chat", {
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
          handleEvent(JSON.parse(data), turn, applyText);
          if (turn.terminal) break;
        }
        if (turn.terminal) {
          // The server closes right after the terminal event; release the
          // connection promptly instead of waiting for the next read.
          try { reader.cancel(); } catch { /* already closed */ }
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
      }
      ai.label.textContent += fmtUsage(turn.usage);
      scrollToBottom();
    }
  }

  function handleEvent(event, turn, applyText) {
    switch (event.type) {
      case "delta":
        turn.text += event.text;
        applyText();
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
      await fetch("/api/abort", {
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

  /* ---------- bootstrap ---------- */

  function renderRestored(session) {
    if (session.summary) {
      const box = addBox("kaeru", "kaeru · summary");
      box.body.textContent = session.summary;
    }
    for (const message of session.history || []) {
      const mine = message.role === "user";
      const box = addBox(mine ? "you" : "kaeru", mine ? "you" : "kaeru");
      box.body.textContent = message.content;
    }
    if ((session.history || []).length || session.summary) {
      removeEmptyHint();
      scrollToBottom(true);
    }
  }

  async function bootstrap() {
    applyTheme(loadTheme());
    showEmptyHint();
    try {
      const session = await (await fetch("/api/session")).json();
      state.fake = Boolean(session.fake);
      state.model = session.model || null;
      state.usage = {
        in: (session.usage && session.usage.input_tokens) || 0,
        out: (session.usage && session.usage.output_tokens) || 0,
      };
      updateUsageBadge();
      if (state.fake) fakeBadge.hidden = false;
      renderRestored(session);
      const { models } = await (await fetch("/api/models")).json();
      const ids = models.map((m) => m.id);
      if (state.model && !ids.includes(state.model)) ids.unshift(state.model);
      for (const id of ids) {
        const option = document.createElement("option");
        option.value = id;
        option.textContent = id;
        if (id === state.model) option.selected = true;
        modelSelect.append(option);
      }
    } catch (err) {
      console.warn("kaeru: bootstrap failed (is the auth token set?)", err);
    }
    composerEl.focus();
  }

  modelSelect.addEventListener("change", () => {
    state.model = modelSelect.value || null;
  });

  bootstrap();
})();
