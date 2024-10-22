# iswitch
auto switch input source to ABC when forcus on terminal.
auto switch to PinYin when focus on wechat or qq. only support macos-arm now.

# Usage

iswitch will read the config file from `XDG_CONFIG_HOME/iswitch/config.toml`.
if config file not exist it will auto create config file with empty rule.

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
au('InsertLeave', {
  callback = function()
    if vim.fn.executable('iswitch') == 0 then
      return
    end
    vim.system({ 'iswitch', '--check-and-switch' }, nil, function(proc)
      if proc.code ~= 0 then
        api.nvim_err_writeln('Failed to switch input source: ' .. proc.stderr)
      end
    end)
  end,
  desc = 'auto switch to abc input',
})
```

## TODO
- linux support

## LICENSE MIT
