-- zeit-claude: the view of what Claude Code runs is the zfloat "claude". Its settings below
-- are the plugin's; init.lua can change any of them (it loads after plugins):
--   z.zfloat('claude', { width = 25, side = 'left' })
local z = require('zeit')
z.zfloat('claude', {
  width = 30,          -- % of the screen
  height = 70,         -- % of the screen
  side = 'top-right',  -- right, left, top-right, bottom-right, top-left, bottom-left, center
  below = true,        -- the keys stay with Claude (prefix Tab moves them up into the view)
  title = 'Claude',
  buttons = true,      -- presets, hide, close on its frame (closed it stays closed this session; prefix C shows it)
})
z.map('<prefix>C', 'zfloat-toggle -n claude', { desc = 'Show or hide what Claude runs' })
-- never shown: touch ~/.config/zeit/claude-view-off (the plugin's tools keep working)
