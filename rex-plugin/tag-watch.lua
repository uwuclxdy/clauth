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
local profiles_dir = (os.getenv("HOME") or "") .. "/.clauth/profiles"

-- Zero would hot-spin the timer and a huge value overflows nothing useful;
-- clamp to [1 s, 1 h] like the herdr plugin's tag_watch_secs.
local interval = math.floor(tonumber(os.getenv("CLAUTH_REX_WATCH_SECS") or "") or 10)
interval = math.max(1, math.min(3600, interval))

-- block_id -> { session_id, pid, orig, ours }
local tracked = {}

-- Cheap filter before forking the resolver: every shell command fires
-- process_changed, and only claude, clauth or codex can spend an account.
-- Match program names, not whole paths: everything run from ~/.codex/worktrees
-- has "codex" in its path. resolve-profile.sh makes the real decision.
local AGENTS = { claude = true, clauth = true, codex = true }

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

local function resolve(pid)
  local r = rex.exec({ "sh", resolver, tostring(pid) }, { timeout = 10 })
  if not (r and r.ok) then return nil end
  local profile, how = r.stdout:match("^([^\t\n]+)\t([^\t\n]+)")
  return profile, how
end

local function label_of(session_id, block_id)
  local d = rex.session.describe_block{ session_id = session_id, block_id = block_id }
  return d and d.block and d.block.label or nil
end

local function set_label(session_id, block_id, label)
  rex.session.set_block_label{ session_id = session_id, block_id = block_id, label = label or "" }
end

-- The clauth profile names, so only a tag naming one of them is ever taken for
-- ours: a user's own "deploy · prod" label must survive.
local profiles = {}
local function load_profiles()
  local r = rex.exec({ "ls", "-1", profiles_dir }, { timeout = 5 })
  if not (r and r.ok) then return end
  local names = {}
  for name in r.stdout:gmatch("[^\n]+") do names[name] = true end
  profiles = names
end

-- Strips every account tag this script may have added, so a label read back
-- after a watcher restart ("claude · work") is not tagged twice.
local function base_of(label)
  if not label then return nil end
  while true do
    -- Greedy head: split at the last separator, peeling one tag per pass.
    local head, tag = label:match("^(.*)" .. SEP .. "(.-)$")
    if not head then return label end
    local name = tag:gsub(" %(global%)$", "")
    if not profiles[name] then return label end
    label = head
  end
end

local function apply(session_id, block_id, pid)
  local t = tracked[block_id]
  local profile, how
  if pid then profile, how = resolve(pid) end
  local current = label_of(session_id, block_id)

  if not profile then
    if t then
      -- Claude exited, or the pane runs something else now.
      if t.ours and current == t.ours then set_label(session_id, block_id, t.orig) end
      tracked[block_id] = nil
    end
    return
  end

  if not t then
    t = { orig = base_of(current) }
    tracked[block_id] = t
  elseif t.ours and current ~= t.ours then
    t.orig = base_of(current)
  end
  t.session_id, t.pid = session_id, pid

  local base = t.orig
  if base == nil or base == "" or base == "shell" then
    base = how == "codex" and "codex" or "claude"
  end
  local want = base .. SEP .. profile
  if how == "global" then want = want .. " (global)" end
  if current ~= want then
    set_label(session_id, block_id, want)
  end
  t.ours = want
end

-- A pane that looked like an agent but has no registry row yet stays pending, so
-- the timer picks it up once `clauth start` writes the row.
local pending = {}

local function consider(session_id, block_id, fg)
  if looks_like_agent(fg) then
    apply(session_id, block_id, fg.pid)
    if not tracked[block_id] then
      pending[block_id] = { session_id = session_id, pid = fg.pid }
    else
      pending[block_id] = nil
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
  load_profiles()
  for block_id, t in pairs(tracked) do
    apply(t.session_id, block_id, t.pid)
  end
  for block_id, p in pairs(pending) do
    apply(p.session_id, block_id, p.pid)
    if tracked[block_id] then pending[block_id] = nil end
  end
end)

-- Panes already running Claude when the watcher starts fire no event.
load_profiles()
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
