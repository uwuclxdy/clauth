-- Labels each Rex pane that runs Claude Code or codex with the clauth account
-- it spends, such as "claude · work" or "codex · codex". start-watch.sh
-- runs it as `rex do --all`, since event handlers only work in scripts, not in
-- init.lua.
--
-- A pane's own label is kept and the account is appended to it. When Claude
-- exits the original label comes back, unless someone relabeled the pane
-- meanwhile, in which case their label stands.

local TERMINAL = "com.superlogical.terminal"
local SEP = " · "

local root = debug.getinfo(1, "S").source:gsub("^@", ""):match("(.*)/[^/]*$") or "."
local resolver = root .. "/resolve-profile.sh"

-- Zero would hot-spin the timer and a huge value overflows nothing useful;
-- clamp to [1 s, 1 h] like the herdr plugin's tag_watch_secs.
local interval = math.floor(tonumber(os.getenv("CLAUTH_REX_WATCH_SECS") or "") or 10)
interval = math.max(1, math.min(3600, interval))

-- block_id -> { session_id, pid, orig, ours }
local tracked = {}

-- Cheap filter before forking the resolver: every shell command fires
-- process_changed, and only claude, clauth or codex can spend an account.
-- node, bun and deno run an npm-installed Claude Code or codex
-- (`node .../@anthropic-ai/claude-code/cli.js`), and Rex names no script, so
-- they pass too: `clauth which --pid` reads the script off the argv and
-- answers nothing for any other. Match program names, not whole paths:
-- everything run from ~/.codex/worktrees has "codex" in its path.
-- resolve-profile.sh makes the real decision.
local AGENTS = {
  claude = true, clauth = true, codex = true,
  node = true, bun = true, deno = true,
}

local function looks_like_agent(fg)
  if not fg then return false end
  for _, field in ipairs({ "name", "argv0", "invoked_path", "path" }) do
    local v = fg[field]
    if type(v) == "string" then
      if AGENTS[v:match("([^/]*)$")] or v:find("/claude/versions/", 1, true) then
        return true
      end
    end
  end
  return false
end

-- The account `pid` spends as profile, how and agent; nothing when it spends
-- none; or false when clauth could not answer (none on PATH, or one older than
-- `which --pid`), which is no answer at all rather than "no account".
local function resolve(pid)
  local r = rex.exec({ "sh", resolver, tostring(pid) }, { timeout = 10 })
  if not (r and r.ok) then return false end
  local profile, how, agent = r.stdout:match("^([^\t\n]+)\t([^\t\n]+)\t([^\t\n]+)")
  return profile, how, agent
end

-- The pane's label, "" for none, or nil when Rex could not describe the pane.
local function label_of(session_id, block_id)
  local d = rex.session.describe_block{ session_id = session_id, block_id = block_id }
  if not (d and d.block) then return nil end
  return d.block.label or ""
end

local function set_label(session_id, block_id, label)
  rex.session.set_block_label{ session_id = session_id, block_id = block_id, label = label or "" }
end

-- The label without the tags naming `profile`, so a label read back after a
-- watcher restart ("claude · work") is not tagged twice. Only the account the
-- resolver just named is peeled: a user's own "deploy · prod" keeps its
-- "· prod", whatever the clauth profiles are called.
local function base_of(label, profile)
  local tags = { SEP .. profile, SEP .. profile .. " (global)" }
  local peeled = true
  while peeled do
    peeled = false
    for _, tag in ipairs(tags) do
      if #label >= #tag and label:sub(-#tag) == tag then
        label = label:sub(1, #label - #tag)
        peeled = true
      end
    end
  end
  return label
end

local function apply(session_id, block_id, pid)
  local t = tracked[block_id]
  local profile, how, agent
  if pid then
    profile, how, agent = resolve(pid)
    -- A failed read keeps whatever label stands; the timer asks again.
    if profile == false then return end
  end
  local current = label_of(session_id, block_id)
  -- Rex could not say what the label is: change nothing, so a label read as
  -- missing is never kept as the one to restore. The timer asks again.
  if current == nil then return end

  if not profile then
    if t then
      -- Claude exited, or the pane runs something else now.
      if t.ours and current == t.ours then set_label(session_id, block_id, t.orig) end
      tracked[block_id] = nil
    end
    return
  end

  if not t then
    t = { orig = base_of(current, profile) }
    tracked[block_id] = t
  elseif t.ours and current ~= t.ours then
    t.orig = base_of(current, profile)
  end
  t.session_id, t.pid = session_id, pid

  local base = t.orig
  if base == "" or base == "shell" then
    -- The resolver names the agent: a `clauth start` pane runs the clauth
    -- supervisor whether it hosts Claude Code or codex.
    base = agent
  end
  local want = base .. SEP .. profile
  if how == "global" then want = want .. " (global)" end
  if current ~= want then
    set_label(session_id, block_id, want)
  end
  t.ours = want
end

-- A pane that looked like an agent but resolved to no account stays pending,
-- so the timer picks it up once `clauth start` writes its row, a moment after
-- it starts. Each miss doubles the ticks until the next ask, up to MAX_WAIT,
-- so a pane that never resolves (a codex on its own login, a node dev server)
-- costs one resolver run every few minutes rather than one every tick.
local pending = {}
local MAX_WAIT = 32

local function consider(session_id, block_id, fg)
  if looks_like_agent(fg) then
    apply(session_id, block_id, fg.pid)
    local p = pending[block_id]
    if tracked[block_id] then
      pending[block_id] = nil
    elseif not (p and p.pid == fg.pid) then
      pending[block_id] = { session_id = session_id, pid = fg.pid, wait = 1, left = 1 }
    end
  else
    pending[block_id] = nil
    if tracked[block_id] then apply(session_id, block_id, nil) end
  end
end

rex.on("terminal.process_changed", function(ctx, ev)
  consider(ctx.session_id, ctx.block_id, ev.foreground)
end)

rex.on("block_closed", function(_, ev)
  tracked[ev.block_id] = nil
  pending[ev.block_id] = nil
end)

-- Re-resolve on a timer: a --with-fallback session swaps accounts, and a bare
-- `claude` follows `clauth switch`, without any Rex event.
rex.every(interval, function()
  for block_id, t in pairs(tracked) do
    apply(t.session_id, block_id, t.pid)
  end
  for block_id, p in pairs(pending) do
    p.left = p.left - 1
    if p.left <= 0 then
      apply(p.session_id, block_id, p.pid)
      if tracked[block_id] then
        pending[block_id] = nil
      else
        p.wait = math.min(p.wait * 2, MAX_WAIT)
        p.left = p.wait
      end
    end
  end
end)

-- Panes already running Claude when the watcher starts fire no event.
local sessions = rex.call("session.list") or {}
for _, s in ipairs(sessions.sessions or {}) do
  -- Run in the background there is no session in the environment, and block
  -- calls are refused until the script attaches to the block's session.
  rex.session.attach{ session_id = s.session_id }
  local blocks = rex.session.list_blocks{ session_id = s.session_id } or {}
  for _, b in ipairs(blocks.blocks or {}) do
    if (b.creator_name or ""):find(TERMINAL, 1, true) == 1 then
      local p = rex.block.call(TERMINAL, "process",
        { session_id = s.session_id, block_id = b.block_id })
      if p then consider(s.session_id, b.block_id, p.foreground) end
    end
  end
end
