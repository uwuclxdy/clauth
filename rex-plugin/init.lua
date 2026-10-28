-- clauth for Rex: launch Claude Code through clauth, open the clauth dashboard
-- in a floating pane, and label each pane with the account it spends.
--
-- Install it beside Rex's config as ~/.config/rex/clauth/ and load it from
-- ~/.config/rex/init.lua:
--
--   require("clauth").setup{}
--
-- Every option is optional:
--
--   require("clauth").setup{
--     -- clauth start flags when no profile is named
--     select = { "--auto", "--with-fallback" },
--     -- handed to claude after `--`
--     claude_args = { "--dangerously-skip-permissions" },
--     env = { CLAUDE_CODE_NO_FLICKER = "1" },
--     keys = {
--       claude = "alt+cmd+c",
--       claude_down = "shift+alt+cmd+c",
--       dashboard = "shift+cmd+c",
--     },
--     tag = true, -- run the pane-label watcher
--   }
--
-- Set a key to false to leave it unbound. The actions are in the command
-- palette either way, and `rex do clauth_claude direction=tab profile=work`
-- runs them from a shell. direction is right (default), down, or tab.

local M = {}

local TERMINAL = "com.superlogical.terminal"
local SHELL = "com.superlogical.terminal.shell"

local root = debug.getinfo(1, "S").source:gsub("^@", ""):match("(.*)/[^/]*$") or "."

local defaults = {
  select = { "--auto", "--with-fallback" },
  claude_args = {},
  env = {},
  keys = {
    claude = "alt+cmd+c",
    claude_down = "shift+alt+cmd+c",
    dashboard = "shift+cmd+c",
  },
  tag = true,
}

local opts = defaults

local function merge(user)
  user = user or {}
  local out = {}
  for k, v in pairs(defaults) do out[k] = v end
  for k, v in pairs(user) do out[k] = v end
  -- Keys merge one by one, so rebinding one keeps the other defaults.
  out.keys = {}
  for k, v in pairs(defaults.keys) do out.keys[k] = v end
  for k, v in pairs(user.keys or {}) do out.keys[k] = v end
  return out
end

-- The argv for one Claude session. `env -u CLAUDE_CONFIG_DIR` keeps a profile
-- dir leaking in from the server's environment from pinning the account.
local function command(profile)
  local argv = { "env", "-u", "CLAUDE_CONFIG_DIR" }
  local names = {}
  for k in pairs(opts.env) do names[#names + 1] = k end
  table.sort(names)
  for _, k in ipairs(names) do argv[#argv + 1] = k .. "=" .. tostring(opts.env[k]) end
  argv[#argv + 1] = "clauth"
  argv[#argv + 1] = "start"
  if profile and profile ~= "" then
    argv[#argv + 1] = profile
  else
    for _, a in ipairs(opts.select) do argv[#argv + 1] = a end
  end
  if #opts.claude_args > 0 then
    -- `--auto` needs `--` before claude's own flags; it is harmless after a profile.
    argv[#argv + 1] = "--"
    for _, a in ipairs(opts.claude_args) do argv[#argv + 1] = a end
  end
  return argv
end

-- The working directory of the pane an action ran from.
local function cwd_of(block_id)
  if not block_id then return nil end
  local p = rex.block.call(TERMINAL, "process", { block_id = block_id })
  return p and p.foreground and p.foreground.cwd or nil
end

local function window_of(block_id)
  if not block_id then return nil end
  local d = rex.session.describe_block{ block_id = block_id }
  return d and d.block and d.block.window_id or nil
end

local function claude(ctx, args)
  args = args or {}
  local block = rex.layout.block{
    flavor = SHELL,
    options = { command = command(args.profile), cwd = cwd_of(ctx.block_id) },
  }
  block.block.label = "claude"
  local direction = args.direction or "right"
  if direction == "tab" then
    return rex.session.new_window{ window_label = "claude", layout = block, focus = true }
  end
  return rex.session.new_split{
    anchor_id = ctx.block_id,
    direction = direction == "down" and "vertical" or "horizontal",
    side = "after",
    layout = block,
    focus = true,
  }
end

local dashboard_layer

local function dashboard(ctx)
  -- A second press closes the dashboard instead of stacking another one.
  if dashboard_layer then
    local closed = rex.session.close_layer{ layer_id = dashboard_layer }
    dashboard_layer = nil
    if closed then return closed end
  end
  local block = rex.layout.block{
    flavor = SHELL,
    options = { command = { "clauth" }, cwd = os.getenv("HOME") },
  }
  block.block.label = "clauth"
  local result, err = rex.session.new_layer{
    window_id = window_of(ctx.block_id),
    bounds = { 0.08, 0.06, 0.84, 0.88 },
    layout = block,
    focus = true,
  }
  if result then dashboard_layer = result.layer_id end
  return result, err
end

function M.setup(user)
  opts = merge(user)

  rex.action{
    name = "clauth_claude",
    title = "New Claude Code pane (clauth)",
    description = "Split a pane running Claude Code through clauth, "
      .. "on the account with the most headroom.",
    category = "clauth",
    keywords = "claude agent account subscription",
    args = { direction = "string?", profile = "string?" },
    run = claude,
  }
  rex.action{
    name = "clauth_claude_tab",
    title = "New Claude Code tab (clauth)",
    category = "clauth",
    keywords = "claude agent account subscription window",
    args = { profile = "string?" },
    run = function(ctx, args)
      args = args or {}
      args.direction = "tab"
      return claude(ctx, args)
    end,
  }
  rex.action{
    name = "clauth_dashboard",
    title = "clauth accounts",
    description = "Open the clauth dashboard: accounts, usage windows, and the fallback chain.",
    category = "clauth",
    keywords = "usage quota accounts subscription",
    run = dashboard,
  }

  local keys = opts.keys or {}
  if keys.claude then rex.bind(keys.claude, "clauth_claude", { direction = "right" }) end
  if keys.claude_down then rex.bind(keys.claude_down, "clauth_claude", { direction = "down" }) end
  if keys.dashboard then rex.bind(keys.dashboard, "clauth_dashboard") end

  if opts.tag then
    rex.exec.start({ "sh", root .. "/start-watch.sh" },
      { detach = true, stdout = false, stderr = false })
  end
  return M
end

return M
