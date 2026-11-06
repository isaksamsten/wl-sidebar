# wl-sidebar

Run any regular Wayland application as a slide-out sidebar: a `wlr-layer-shell`
surface anchored to one screen edge that reserves space (exclusive zone) while shown.

It is a Wayland _proxy_, not a compositor: it sits between the app and your compositor
and rewrites `xdg_toplevel` into `zwlr_layer_surface_v1`. Everything else (buffers,
input, popups, clipboard) is passed through untouched. Built on
[wl-proxy](https://github.com/mahkoh/wl-proxy); the translation code is adapted from its
`wl-paper` app (GPL-3.0).

## Usage

Define sidebars as named tables in `$XDG_CONFIG_HOME/wl-sidebar/config.toml` (default
`~/.config/wl-sidebar/config.toml`):

```toml
[term]
command = ["alacritty"]
edge = "left"
size = 400

[notes]
command = "obsidian --ozone-platform=wayland"   # a string is run with `sh -c`
edge = "right"
size = "30%"
focus = true
```

```sh
cargo build --release
wl-sidebar --toggle term   # starts `term` (shown) if not running, otherwise slides it out / in
```

If the config defines only one sidebar, the name can be omitted. `--config
PATH` uses another config file. `wl-sidebar --run [--hidden] NAME` runs a
sidebar in the foreground instead (useful for debugging or autostart).

| Key                      | Default               |                                                                                                                               |
| ------------------------ | --------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `command`                | (required)            | program + args as an array, or a string run with `sh -c`                                                                      |
| `edge`                   | `"left"`              | `left\|right\|top\|bottom`                                                                                                    |
| `size`                   | `400`                 | width (height for top/bottom) in pixels, or a fraction of the output's usable area (`0.5`, `"50%"`; measured once at startup) |
| `peek`                   | `0`                   | pixels left on screen when hidden                                                                                             |
| `output`                 |                       | pin to this output (e.g. `"DP-1"`)                                                                                            |
| `reserve`                | `true`                | reserve space; `false` floats over windows                                                                                    |
| `duration`               | `150`                 | slide animation in ms, `0` = instant                                                                                          |
| `layer`                  | `"top"`               | `background\|bottom\|top\|overlay`                                                                                            |
| `keyboard-interactivity` | `"on-demand"`         | `none\|exclusive\|on-demand` while shown (always `none` while hidden)                                                         |
| `focus`                  | `false`               | take keyboard focus on show: `true`/`"exclusive"` keeps it until hidden, `"remap"` see [Focus](#focus)                        |
| `namespace`              | `"wl-sidebar-<name>"` | layer-shell namespace                                                                                                         |

niri keybinding example:

```kdl
binds {
    Mod+B { spawn "wl-sidebar" "--toggle" "term"; }
}
```

## Focus

With `focus = true`, showing the sidebar switches it to `exclusive` keyboard interactivity, so
it takes keyboard focus right away. Hiding gives focus back to the previously focused
window. While it's shown, other windows can't get keyboard focus, which is the only
reliable way to grab focus: niri gives `on-demand` layer surfaces focus only on
click, not on request.

Without `focus`, the sidebar is `on-demand`: click it to type in it.

`focus = "remap"` (experimental) is the Ghostty quick-terminal approach: the sidebar stays
`on-demand`, but it is unmapped once fully hidden and mapped again on show. niri focuses newly
mapped `on-demand` surfaces, so it gets focus on show and you can still click away from it.
The proxy re-attaches the app's last buffer, so the app doesn't notice. Requires `peek = 0`,
and it hides explicit sync (`wp_linux_drm_syncobj`) from the app.

## How hiding works

Hiding animates a _negative margin_ on the anchored edge, so the app keeps its size and
never re-layouts. The exclusive zone is dropped once fully hidden. Negative-margin
handling is compositor-specific; if your compositor clamps it, use `peek` or
`duration = 0`.
