# terminal-workspace

A mouse-first terminal session multiplexer with a Herdr-inspired vertical tab bar.

```sh
cargo run --release
```

The current shell's working directory becomes the working directory of the first
tab. Click `+` to create another shell, click a session to switch tabs, and click
the bottom-right chevron to collapse or expand the sidebar. The sidebar collapses
automatically below 72 columns.

Keyboard shortcuts:

- `Ctrl-T`: new terminal session
- `Ctrl-B`: toggle the sidebar
- `Ctrl-Q`: exit the multiplexer

This project adapts Herdr's layout, styling, and PTY approach and is therefore
licensed under AGPL-3.0-or-later.
