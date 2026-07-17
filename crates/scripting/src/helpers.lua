-- helpers.lua — pure-Lua surface installed after every Rust binding is in
-- place. Kept separate from the Rust source so user-facing ergonomics stay
-- readable + hackable without a recompile.
--
-- Loaded AFTER `build_and_store_ctx`, which exposed the shared `ctx` facade as
-- the temporary global `__agentd_ctx`. We capture it in a module-level upvalue
-- and augment `ctx.http` / `ctx.ws` — the same table injected into every
-- handler/service. Sandbox lockdown nils the temp global afterward; this
-- upvalue keeps the table identity alive (and lets the `timer` error sink log).
local ctx = __agentd_ctx

-- ---------------------------------------------------------------- string extras
-- Quality-of-life additions on the stock `string` table. Because Lua strings
-- share a metatable whose __index is `string`, these also work as methods:
-- `("  hi "):trim()`, `s:startswith("ws-")`, `s:split(",")`.
do
  function string.trim(s)  return (s:gsub("^%s+", ""):gsub("%s+$", "")) end
  function string.ltrim(s) return (s:gsub("^%s+", "")) end
  function string.rtrim(s) return (s:gsub("%s+$", "")) end

  function string.startswith(s, prefix)
    return s:sub(1, #prefix) == prefix
  end

  function string.endswith(s, suffix)
    return suffix == "" or s:sub(-#suffix) == suffix
  end

  -- Plain-text containment (no Lua patterns).
  function string.contains(s, needle)
    return s:find(needle, 1, true) ~= nil
  end

  -- Split on a PLAIN separator (no patterns). Empty fields are kept:
  -- ("a,,b"):split(",") -> {"a", "", "b"}. With no separator, splits on
  -- whitespace runs and drops empties: ("a  b"):split() -> {"a", "b"}.
  function string.split(s, sep)
    local out = {}
    if sep == nil then
      for w in s:gmatch("%S+") do out[#out + 1] = w end
      return out
    end
    if sep == "" then error("string.split: separator must be non-empty", 2) end
    local pos = 1
    while true do
      local i, j = s:find(sep, pos, true)
      if i == nil then
        out[#out + 1] = s:sub(pos)
        return out
      end
      out[#out + 1] = s:sub(pos, i - 1)
      pos = j + 1
    end
  end
end

-- ---------------------------------------------------------------- timer
-- Bare global `timer`. `timer.every(ms, fn)` ticks every `ms` ms; each tick
-- runs inside an `async(...)` coroutine so a slow tick never blocks peers.
-- Stop with `handle:stop()`. A handler that errors stops the timer and emits a
-- warn-level log. `timer.after(ms, fn)` is the one-shot.
do
  local t = {}
  function t.every(ms, fn)
    local handle = { _stopped = false }
    function handle:stop() self._stopped = true end
    async(function()
      while not handle._stopped do
        sleep(ms)
        if handle._stopped then return end
        local ok, err = pcall(fn)
        if not ok then
          ctx.log.warn("timer: tick errored: " .. tostring(err))
          return
        end
      end
    end)
    return handle
  end
  function t.after(ms, fn)
    local handle = { _stopped = false }
    function handle:stop() self._stopped = true end
    async(function()
      sleep(ms)
      if not handle._stopped then fn() end
    end)
    return handle
  end
  timer = t
end

-- ---------------------------------------------------------------- ctx.http.client
-- `ctx.http.client{ base_url=, headers=, timeout_ms= }` returns a session
-- handle. Headers merge against the per-call `opts.headers`; table bodies
-- auto-encode as JSON. Cuts boilerplate when hammering the same API.
--
--   local api = ctx.http.client{
--     base_url = "https://discord.com/api/v10",
--     headers  = { Authorization = "Bot " .. token },
--   }
--   api:post("/channels/" .. id .. "/messages", { content = "hi" })
do
  local http = ctx.http
  function http.client(cfg)
    cfg = cfg or {}
    local base, defaults, dt = cfg.base_url or "", cfg.headers or {}, cfg.timeout_ms
    local function resolve(path)
      if path:match("^https?://") then return path end
      return base .. path
    end
    local function merge(extra)
      local out = {}
      for k, v in pairs(defaults) do out[k] = v end
      if extra then for k, v in pairs(extra) do out[k] = v end end
      return out
    end
    local function build(method, path, body, opts)
      opts = opts or {}
      local req = {
        method = method,
        url = resolve(path),
        headers = merge(opts.headers),
        timeout_ms = opts.timeout_ms or dt,
      }
      if body ~= nil then
        if type(body) == "table" then req.json = body else req.body = body end
      elseif opts.json ~= nil then req.json = opts.json
      elseif opts.body ~= nil then req.body = opts.body end
      return http.request(req)
    end
    local client = { base_url = base }
    function client:get(path, opts)         return build("GET",    path, nil,  opts) end
    function client:post(path, body, opts)  return build("POST",   path, body, opts) end
    function client:put(path, body, opts)   return build("PUT",    path, body, opts) end
    function client:patch(path, body, opts) return build("PATCH",  path, body, opts) end
    function client:delete(path, opts)      return build("DELETE", path, nil,  opts) end
    function client:request(opts)
      return build(opts.method or "GET", opts.url or opts.path or "", opts.body or opts.json, opts)
    end
    return client
  end
end

-- ---------------------------------------------------------------- ctx.ws extras
-- Adds high-level helpers on top of the raw `ws:recv` frame API:
--
--   handle:recv_text(timeout) -> string | nil, frame_on_close
--   handle:each(cb)           -- loops until close, cb(frame) per frame
--
-- `connect` also gains a second arg for `heartbeat_ms` + `heartbeat(handle)`;
-- when both are set, a `timer` kicks the supplied function on cadence.
do
  local ws = ctx.ws
  local raw_connect = ws.connect
  function ws.connect(url, opts)
    local h = raw_connect(url)
    function h:recv_text(timeout_ms)
      local f = self:recv(timeout_ms)
      if f == nil then return nil end
      if f.kind == "text" then return f.text end
      return nil, f
    end
    function h:each(cb)
      while not self:is_closed() do
        local f = self:recv()
        if f == nil then return end
        cb(f)
        if f.kind == "close" then return end
      end
    end
    if opts and opts.heartbeat_ms and opts.heartbeat then
      timer.every(opts.heartbeat_ms, function()
        if h:is_closed() then return end
        opts.heartbeat(h)
      end)
    end
    return h
  end
end

-- ---------------------------------------------------------------- channels
-- Ergonomic note: `channel:send` accepts plain Lua tables (the payload is
-- serialised + delivered as an independent copy). No json.encode/decode
-- round-trip needed:
--
--     local ev = events:recv()
--     if ev.type == "MESSAGE_CREATE" then ... end

-- ---------------------------------------------------------------- parallel
-- Fan-out / join over independent work. Each branch runs in its own `async`
-- coroutine, so the moment a branch yields on IO (`ctx.run`, `ctx.http`,
-- `ctx.ai`, `await`) its peers make progress — N model calls overlap instead
-- of running back to back. This is the ergonomic gap that made callers write
-- slow sequential loops even though the runtime already supported overlap.
--
--   local verdicts = parallel{
--     function() return ctx.run("agent_a", {...}) end,
--     function() return ctx.run("agent_b", {...}) end,
--   }
--
-- Results come back in input order. Branch return values round-trip through
-- JSON (same as `async`/`await`), so return plain data, not functions.
--
-- Options (second arg):
--   limit   = max branches live at once (default: all)
--   settled = true -> never raise; each result is { ok, value, error }
--             false/nil -> raise the first branch error after the join
do
  function parallel(fns, opts)
    opts = opts or {}
    local n = #fns
    local results = {}
    if n == 0 then return results end
    local settled = opts.settled == true
    local limit = math.min(tonumber(opts.limit) or n, n)
    if limit < 1 then limit = 1 end

    -- Workers share a cursor. Incrementing + reading it is a pure-Lua step
    -- with no yield in between, so two workers never grab the same index.
    local cursor = 0
    local first_error = nil
    local function worker()
      while true do
        cursor = cursor + 1
        local i = cursor
        if i > n then return end
        local ok, value = pcall(fns[i])
        if settled then
          results[i] = {
            ok = ok,
            value = ok and value or nil,
            error = (not ok) and tostring(value) or nil,
          }
        elseif ok then
          results[i] = value
        elseif not first_error then
          first_error = tostring(value)
        end
      end
    end

    local handles = {}
    for _ = 1, limit do handles[#handles + 1] = async(worker) end
    for _, h in ipairs(handles) do await(h) end
    if first_error and not settled then error(first_error, 0) end
    return results
  end

  -- parallel_map(items, fn, opts) — fn(item, index) per element.
  function parallel_map(items, fn, opts)
    local fns = {}
    for i, item in ipairs(items) do
      fns[i] = function() return fn(item, i) end
    end
    return parallel(fns, opts)
  end
end

-- ---------------------------------------------------------------- ctx.structured
-- Guaranteed-shape runner output. Runs a runner, strips markdown fences,
-- JSON-decodes, and (optionally) validates. On any failure it reprompts the
-- model with the exact rejection reason and retries. The caller gets a valid
-- table or a hard error — never malformed JSON silently flowing downstream.
-- This replaces the hand-rolled "decode once, abort the whole pipeline on the
-- first stray token" pattern every structured-output project reinvents.
--
--   local verdict = ctx.structured("scorer", {
--     prompt   = json.encode(payload),
--     system   = CONTRACT,
--     retries  = 2,                 -- extra attempts after the first (default 2)
--     validate = function(t)        -- optional; return ok, err
--       if type(t.scores) ~= "table" then return false, "missing scores" end
--       return true
--     end,
--   })
do
  local function strip_fences(text)
    local body = string.trim(text or "")
    local fenced = body:match("^```[%w]*%s*(.-)%s*```$")
    return fenced or body
  end

  function ctx.structured(runner, opts)
    opts = opts or {}
    local retries = tonumber(opts.retries) or 2
    local validate = opts.validate
    -- validate = "inherit": the contract is the enclosing action's declared
    -- `output` schema. Same reprompt-on-reject loop, schema-backed. Raises
    -- immediately if the action declares no output schema (caller bug, not
    -- a model failure).
    if validate == "inherit" then
      validate = function(t)
        return ctx.validate_output(t)
      end
    end
    local last_err
    local correction

    for attempt = 0, retries do
      local prompt = opts.prompt
      local msgs = opts.messages
      if correction then
        if msgs then
          local copy = {}
          for i, m in ipairs(msgs) do copy[i] = m end
          copy[#copy + 1] = { role = "user", content = correction }
          msgs = copy
        else
          prompt = (prompt or "") .. "\n\n" .. correction
        end
      end

      local response = ctx.run(runner, {
        prompt = prompt,
        messages = msgs,
        system = opts.system,
        model = opts.model,
      })
      local text = response.text or ""
      local ok, decoded = pcall(json.decode, strip_fences(text), { nulls = "nil" })

      if not ok or type(decoded) ~= "table" then
        last_err = "reply was not a JSON object: " .. string.sub(text, 1, 160)
      elseif validate then
        local vok, verr = validate(decoded)
        if vok then
          decoded.model = response.model
          return decoded, response
        end
        last_err = tostring(verr or "validation failed")
      else
        decoded.model = response.model
        return decoded, response
      end

      correction = "Your previous reply was rejected: " .. last_err ..
        "\nReturn ONLY one valid JSON object that satisfies the contract." ..
        " No prose, no markdown fences."
    end

    error(runner .. ": structured output failed after " ..
      tostring(retries + 1) .. " attempts: " .. tostring(last_err), 0)
  end
end
