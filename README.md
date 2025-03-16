# iswitch
auto switch input source according your rules. now only support macos.

# Install

- With homebrew

```
brew tap glepnir/iswitch
brew install iswitch
```
uninstall `brew untap glepnir/iswitch`

# Usage

iswitch will read the config file from `XDG_CONFIG_HOME/iswitch/config.toml`.
if config file not exist it will auto create config file with empty rule.
support hotreloading config.

example config file format

```toml
[app]
# Examples of application-to-input-source mappings
# Format: "Application Name" = "InputSourceID"
# 
# "Terminal" = "com.apple.keylayout.US"
# "Safari" = "com.apple.inputmethod.TCIM.Pinyin"

[debug]
# Enable debugging
enabled = false
# Log level: trace, debug, info, warn, error
level = "info"
# Log file path
file = "~/Library/Logs/iswitch.log"
```

### Command Line Argument

- `-s` with switch input source id
- `-p` print all available input source list with localized name if found.

# Integration with editor

## neovim

Asynchronously auto-switch the input source after leaving insert mode. After
the switch is complete, regardless of whether the switch succeeds or fails,
iswitch will check if an iswitch process is already running. If no iswitch
process is found, iswitch will not exit; it will remain running.

```lua
vim.api.nvim_create_autocmd('InsertLeave', {
  callback = function()
    if vim.fn.executable('iswitch') == 0 then
      return
    end
    vim.system({ 'iswitch', '-s', 'com.apple.keylayout.ABC' }, nil, function(proc)
      if proc.code ~= 0 then
        vim.notify('Failed to switch input source: ' .. proc.stderr, vim.log.levels.Warn)
      end
    end)
  end,
  desc = 'auto switch to abc input',
})
```

## TODO
- linux support

## LICENSE MIT
