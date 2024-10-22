# iswitch
auto switch input source according your rules. now only support macos.

# Usage

iswitch will read the config file from `XDG_CONFIG_HOME/iswitch/config.toml`.
if config file not exist it will auto create config file with empty rule.
support hotreloading config.

example config file format

```toml
[app]
Alacritty = "com.apple.keylayout.ABC"
WeChat = "com.apple.inputmethod.SCIM.ITABC"
QQ = "com.apple.inputmethod.SCIM.ITABC"
```

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
        api.nvim_err_writeln('Failed to switch input source: ' .. proc.stderr)
      end
    end)
  end,
  desc = 'auto switch to abc input',
})
```

## TODO
- print input source names
- linux support

## LICENSE MIT
