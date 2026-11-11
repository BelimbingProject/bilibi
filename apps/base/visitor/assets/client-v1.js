/*! Bilibi visitor chat client v1. No dependencies. One script tag:
 *   <script src="https://HOST/visitor/v1/client.js" data-channel="…" data-site="…" async></script>
 * It runs under a strict Content-Security-Policy: no inline script or style
 * attribute, no eval, no remote font or image. A page needs only
 *   script-src HOST; connect-src HOST (and wss://HOST).
 * Contract: apps/base/visitor/README.md. */
(function () {
  "use strict";
  var VERSION = "1.0.0";
  if (window.BilibiVisitor) return;
  var script = document.currentScript;
  if (!script) return;
  var channel = script.getAttribute("data-channel") || "";
  var site = script.getAttribute("data-site") || "";
  if (!channel || !site) return;
  var details = {};
  try {
    details = JSON.parse(script.getAttribute("data-details") || "{}");
  } catch (e) {
    details = {};
  }
  var title = script.getAttribute("data-title") || "Chat with us";
  var api = new URL(script.src).origin;
  var socketUrl = api.replace(/^http/, "ws") + "/visitor/v1/live";
  var storeKey = "bilibi-visitor:" + api + ":" + channel + ":" + site;
  var CHUNK = 64 * 1024;
  var TEXT_MAX = 4000;
  var TYPING_EVERY = 3000;

  var state = {
    token: null,
    cursor: null,
    socket: null,
    online: false,
    ended: false,
    open: false,
    attempts: 0,
    next: 1,
    uploading: null,
    lastTyping: 0,
    reconnect: 0,
    seen: {},
  };

  // Storage can be blocked or full: the chat then lasts for this page only.
  function load() {
    try {
      var saved = JSON.parse(sessionStorage.getItem(storeKey) || "null");
      if (saved && typeof saved.token === "string") {
        state.token = saved.token;
        state.cursor = typeof saved.cursor === "string" ? saved.cursor : null;
      }
    } catch (e) {}
  }
  function save() {
    try {
      if (state.token) sessionStorage.setItem(storeKey, JSON.stringify({ token: state.token, cursor: state.cursor }));
      else sessionStorage.removeItem(storeKey);
    } catch (e) {}
  }

  // The same request shape for every call: a "simple" cross-origin POST, so
  // the browser sends no preflight to an address nobody has checked.
  function call(path, body) {
    return fetch(api + path, {
      method: "POST",
      mode: "cors",
      credentials: "omit",
      referrerPolicy: "no-referrer",
      headers: { "Content-Type": "text/plain;charset=UTF-8" },
      body: JSON.stringify(body),
    }).then(function (response) {
      return response.json().then(
        function (json) {
          return { status: response.status, json: json };
        },
        function () {
          return { status: response.status, json: {} };
        }
      );
    });
  }

  // ---- the window ----
  var host = document.createElement("div");
  host.setAttribute("data-bilibi-visitor", VERSION);
  var root = host.attachShadow({ mode: "open" });
  var css =
    ":host{all:initial;--ink:#1c1917;--muted:#57534e;--surface:#fff;--canvas:#fafaf9;--sunken:#e7e5e4;--line:#d6d3d1;--action:#1a2e05;--action-ink:#f7fee7;--brand:#bef264;--danger:#b91c1c;--mine:#ecfccb;font:14px/20px system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;color:var(--ink)}" +
    "@media (prefers-color-scheme:dark){:host{--ink:#f5f5f4;--muted:#a8a29e;--surface:#1c1917;--canvas:#0c0a09;--sunken:#292524;--line:#44403c;--action:#65a30d;--action-ink:#1a2e05;--brand:#a3e635;--danger:#fca5a5;--mine:#1a2e05}}" +
    "*{box-sizing:border-box}" +
    ".launcher{position:fixed;right:16px;bottom:16px;z-index:2147483000;min-height:40px;padding:8px 16px;border:0;border-radius:8px;background:var(--action);color:var(--action-ink);font:inherit;font-weight:600;cursor:pointer;box-shadow:0 2px 8px rgba(0,0,0,.25)}" +
    ".launcher:hover{filter:brightness(1.1)}" +
    ".panel{position:fixed;right:16px;bottom:16px;z-index:2147483001;display:flex;flex-direction:column;width:min(360px,calc(100vw - 32px));height:min(520px,calc(100vh - 32px));background:var(--surface);border:1px solid var(--line);border-radius:12px;box-shadow:0 8px 32px rgba(0,0,0,.3);overflow:hidden}" +
    ".panel[hidden],.launcher[hidden]{display:none}" +
    ".bar{display:flex;align-items:center;gap:8px;padding:8px 12px;background:var(--canvas);border-bottom:1px solid var(--line);border-top:3px solid var(--brand)}" +
    ".bar h2{flex:1;margin:0;font-size:14px;font-weight:600}" +
    ".state{font-size:12px;color:var(--muted)}" +
    ".log{flex:1;overflow-y:auto;padding:12px;display:flex;flex-direction:column;gap:8px;background:var(--canvas)}" +
    ".msg{max-width:85%;padding:8px 12px;border-radius:12px;background:var(--surface);border:1px solid var(--line);overflow-wrap:anywhere;white-space:pre-wrap}" +
    ".msg.visitor{align-self:flex-end;background:var(--mine)}" +
    ".msg.system{align-self:center;background:transparent;border:0;color:var(--muted);font-size:12px}" +
    ".file{display:block;margin-top:4px;color:var(--muted);font-size:12px}" +
    ".typing{min-height:20px;padding:0 12px 4px;font-size:12px;color:var(--muted);background:var(--canvas)}" +
    ".note{padding:8px 12px;font-size:12px;color:var(--danger);background:var(--canvas)}" +
    ".compose{display:flex;gap:8px;align-items:flex-end;padding:8px;border-top:1px solid var(--line);background:var(--surface)}" +
    "textarea{flex:1;min-height:40px;max-height:120px;resize:none;padding:8px;border:1px solid var(--line);border-radius:8px;background:var(--surface);color:var(--ink);font:inherit}" +
    "textarea:focus-visible,button:focus-visible{outline:2px solid var(--action);outline-offset:2px}" +
    "button.go,button.again{min-height:40px;padding:8px 12px;border:0;border-radius:8px;background:var(--action);color:var(--action-ink);font:inherit;font-weight:600;cursor:pointer}" +
    "button.icon{min-width:40px;min-height:40px;border:1px solid var(--line);border-radius:8px;background:var(--surface);color:var(--ink);font:inherit;cursor:pointer}" +
    "button[disabled]{opacity:.5;cursor:not-allowed}" +
    ".again{margin:8px 12px}" +
    ".sr{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)}";
  var sheet = null;
  try {
    sheet = new CSSStyleSheet();
    sheet.replaceSync(css);
    root.adoptedStyleSheets = [sheet];
  } catch (e) {
    // A browser without constructable stylesheets gets an unstyled window;
    // nothing here loads a stylesheet that a strict policy would refuse.
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text) node.textContent = text;
    return node;
  }
  var launcher = el("button", "launcher", title);
  launcher.type = "button";
  launcher.setAttribute("aria-haspopup", "dialog");
  var panel = el("section", "panel");
  panel.hidden = true;
  panel.setAttribute("role", "dialog");
  panel.setAttribute("aria-label", title);
  var bar = el("div", "bar");
  var heading = el("h2", "", title);
  var status = el("span", "state", "");
  var close = el("button", "icon", "×");
  close.type = "button";
  close.setAttribute("aria-label", "Close chat");
  bar.appendChild(heading);
  bar.appendChild(status);
  bar.appendChild(close);
  var log = el("div", "log");
  log.setAttribute("role", "log");
  log.setAttribute("aria-live", "polite");
  var typing = el("div", "typing");
  typing.setAttribute("aria-live", "polite");
  var note = el("div", "note");
  note.hidden = true;
  note.setAttribute("role", "alert");
  var compose = el("form", "compose");
  var input = el("textarea");
  input.rows = 1;
  input.maxLength = TEXT_MAX;
  input.setAttribute("aria-label", "Your message");
  input.placeholder = "Type a message";
  var attach = el("button", "icon", "+");
  attach.type = "button";
  attach.setAttribute("aria-label", "Attach a file");
  var picker = el("input");
  picker.type = "file";
  picker.className = "sr";
  picker.tabIndex = -1;
  var send = el("button", "go", "Send");
  send.type = "submit";
  compose.appendChild(attach);
  compose.appendChild(input);
  compose.appendChild(send);
  compose.appendChild(picker);
  panel.appendChild(bar);
  panel.appendChild(log);
  panel.appendChild(typing);
  panel.appendChild(note);
  panel.appendChild(compose);
  root.appendChild(launcher);
  root.appendChild(panel);

  function say(text) {
    status.textContent = text;
  }
  function warn(text) {
    note.textContent = text || "";
    note.hidden = !text;
  }
  function scroll() {
    log.scrollTop = log.scrollHeight;
  }
  function append(item) {
    if (item.typing !== undefined && !item.text && !item.attachment) {
      typing.textContent = item.typing ? "Typing…" : "";
      return;
    }
    if (item.seq !== undefined) {
      if (state.seen[item.seq]) return;
      state.seen[item.seq] = true;
    }
    var from = item.from === "visitor" ? "visitor" : item.from === "system" ? "system" : "agent";
    var bubble = el("div", "msg " + from);
    if (typeof item.text === "string") bubble.appendChild(document.createTextNode(item.text));
    if (item.attachment || item.name) {
      var file = el("span", "file", "Attachment: " + String((item.attachment && item.attachment.name) || item.name));
      bubble.appendChild(file);
    }
    log.appendChild(bubble);
    if (from === "agent") typing.textContent = "";
    scroll();
  }
  function enable(on) {
    send.disabled = !on;
    attach.disabled = !on;
    input.disabled = !on;
  }
  function end(message) {
    state.ended = true;
    state.online = false;
    state.token = null;
    state.cursor = null;
    save();
    enable(false);
    say("Ended");
    warn(message);
    if (!panel.querySelector(".again")) {
      var again = el("button", "again", "Start a new chat");
      again.type = "button";
      again.addEventListener("click", function () {
        again.remove();
        state.ended = false;
        state.seen = {};
        log.textContent = "";
        warn("");
        connect();
      });
      panel.insertBefore(again, compose);
    }
  }

  // ---- the session and the socket ----
  function session() {
    if (state.token) {
      return call("/visitor/v1/session/renew", { token: state.token }).then(
        function (reply) {
          if (reply.status === 200) return state.token;
          state.token = null;
          state.cursor = null;
          return open();
        },
        function () {
          return state.token;
        }
      );
    }
    return open();
  }
  function open() {
    return call("/visitor/v1/sessions", { channel: channel, site: site, details: details }).then(function (reply) {
      if (reply.status === 200 && reply.json.token) {
        state.token = reply.json.token;
        state.seen = {};
        save();
        return state.token;
      }
      throw new Error(reply.status === 429 ? "busy" : "refused");
    });
  }
  function connect() {
    if (state.socket || state.ended) return;
    say("Connecting…");
    enable(false);
    session().then(
      function () {
        var ws = new WebSocket(socketUrl);
        ws.binaryType = "arraybuffer";
        state.socket = ws;
        ws.onopen = function () {
          ws.send(JSON.stringify({ type: "hello", token: state.token, cursor: state.cursor }));
        };
        ws.onmessage = function (event) {
          var frame;
          try {
            frame = JSON.parse(event.data);
          } catch (e) {
            return;
          }
          receive(frame);
        };
        ws.onclose = function () {
          if (state.socket === ws) state.socket = null;
          state.online = false;
          if (state.ended) return;
          enable(false);
          say("Offline");
          retry();
        };
      },
      function (error) {
        warn(
          error.message === "busy"
            ? "The chat is busy. Please try again in a minute."
            : "The chat is not available from this page."
        );
        say("Unavailable");
        retry();
      }
    );
  }
  function retry() {
    if (state.ended || !state.open) return;
    clearTimeout(state.reconnect);
    var wait = Math.min(30000, 1000 * Math.pow(2, state.attempts++));
    state.reconnect = setTimeout(connect, wait);
  }
  function receive(frame) {
    if (frame.type === "ready") {
      state.online = true;
      state.attempts = 0;
      warn("");
      enable(true);
      say("Online");
    } else if (frame.type === "items") {
      (frame.items || []).forEach(append);
      if (typeof frame.cursor === "string") {
        state.cursor = frame.cursor;
        save();
      }
    } else if (frame.type === "error") {
      if (frame.code === "rate_limited") warn("You are sending too fast. Please wait a moment.");
      else if (frame.code === "invalid") warn("That could not be sent.");
      else warn("Something went wrong. Please try again.");
      if (state.uploading && frame.id === state.uploading) state.uploading = null;
    } else if (frame.type === "ack") {
      if (state.uploading && frame.id === state.uploading) state.uploading = null;
      warn("");
    } else if (frame.type === "upload_ready") {
      stream();
    } else if (frame.type === "closed") {
      if (frame.reason === "ended" || frame.reason === "refused") {
        end("This chat has ended.");
      }
    }
  }
  function transmit(value) {
    if (!state.socket || state.socket.readyState !== 1 || !state.online) return false;
    state.socket.send(JSON.stringify(value));
    return true;
  }

  // ---- sending ----
  compose.addEventListener("submit", function (event) {
    event.preventDefault();
    var text = input.value.trim();
    if (!text) return;
    if (transmit({ type: "message", id: state.next++, body: { text: text.slice(0, TEXT_MAX) } })) {
      input.value = "";
      transmit({ type: "typing", body: { typing: false } });
    } else {
      warn("Not connected yet. Your message was kept; try again in a moment.");
    }
  });
  input.addEventListener("keydown", function (event) {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      compose.requestSubmit();
    }
  });
  input.addEventListener("input", function () {
    var now = Date.now();
    if (input.value && now - state.lastTyping > TYPING_EVERY) {
      state.lastTyping = now;
      transmit({ type: "typing", body: { typing: true } });
    }
  });
  attach.addEventListener("click", function () {
    picker.click();
  });
  var pending = null;
  picker.addEventListener("change", function () {
    var file = picker.files && picker.files[0];
    picker.value = "";
    if (!file || state.uploading) return;
    var id = state.next++;
    state.uploading = id;
    pending = { id: id, file: file };
    if (!transmit({ type: "upload", id: id, name: file.name.slice(0, 255), size: file.size })) {
      state.uploading = null;
      warn("Not connected yet. Try again in a moment.");
    } else {
      warn("");
      say("Sending " + file.name + "…");
    }
  });
  function stream() {
    if (!pending) return;
    var job = pending;
    pending = null;
    job.file.arrayBuffer().then(function (buffer) {
      var offset = 0;
      while (offset < buffer.byteLength) {
        var part = buffer.slice(offset, offset + CHUNK);
        var frame = new Uint8Array(8 + part.byteLength);
        var view = new DataView(frame.buffer);
        view.setUint32(0, job.id);
        view.setUint32(4, offset);
        frame.set(new Uint8Array(part), 8);
        if (!state.socket || state.socket.readyState !== 1) return;
        state.socket.send(frame.buffer);
        offset += part.byteLength;
      }
      say("Online");
    });
  }

  // ---- opening and closing ----
  function show() {
    state.open = true;
    launcher.hidden = true;
    panel.hidden = false;
    input.focus();
    connect();
  }
  function hide() {
    state.open = false;
    panel.hidden = true;
    launcher.hidden = false;
    launcher.focus();
  }
  launcher.addEventListener("click", show);
  close.addEventListener("click", hide);
  panel.addEventListener("keydown", function (event) {
    if (event.key === "Escape") hide();
  });

  load();
  function mount() {
    document.body.appendChild(host);
  }
  if (document.body) mount();
  else document.addEventListener("DOMContentLoaded", mount);
  window.BilibiVisitor = { version: VERSION, open: show, close: hide };
})();
