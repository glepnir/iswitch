# iswitch
auto switch input source to ABC when forcus on terminal (Alacritty) on macos-arm

# Integration with editor

## neovim

Asynchronously auto-switch the input source after leaving insert mode. After
the switch is complete, regardless of whether the switch succeeds or fails,
iswitch will check if an iswitch process is already running. If no iswitch
process is found, iswitch will not exit; it will remain running.

```lua
au('InsertLeave', {
  callback = function()
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
