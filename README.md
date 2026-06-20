# Plexr

A mouse-first terminal session multiplexer with a Herdr-inspired vertical tab bar.

```sh
cargo run --release
```

The current shell's working directory becomes the working directory of the first
tab. Click `+` to create another shell, click a session to switch tabs, click its
`x` to close it, and click the bottom-right chevron to collapse or expand the
sidebar. The sidebar collapses automatically below 72 columns.

Tabs show the last entered command and current directory. While a foreground
command owns the PTY, a spinner appears beside its tab. Right-click a tab to:

- Rename it or clear its custom name
- Create a side-by-side pane group with the active tab
- Add it to an existing pane group
- Tile a group side by side or top and bottom
- Equalize a group's pane sizes
- Remove it from a group or dissolve the full group
- Close it

Clicking a visible terminal pane focuses that tab. A custom name overrides the
last command until the custom name is cleared. Drag a divider between grouped
panes to configure their relative widths or heights.

Keyboard shortcuts:

- `Ctrl-T`: new terminal session
- `Ctrl-B`, then `1` through `9`: select top-level tab/group 1 through 9
- `Ctrl-B`, then `0`: select top-level tab/group 10
- `Ctrl-B`, then `Left`/`Right`: previous/next top-level tab or group
- `Ctrl-B`, then `h`/`j`/`k`/`l`: focus pane left/down/up/right
- `Ctrl-B`, then `T`: create a new terminal tab
- `Ctrl-B`, then `W`: close the active tab
- `Ctrl-B`, then `s`: toggle the sidebar
- `Ctrl-B`, then `Ctrl-B`: send a literal `Ctrl-B` to the active terminal
- `Ctrl-Q`: exit the multiplexer

The prefix remains active for one second and displays an on-screen key hint.
Pane groups count as one item for numbered and left/right navigation.

This project adapts Herdr's layout, styling, and PTY approach and is therefore
licensed under AGPL-3.0-or-later.
