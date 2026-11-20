# Vision

Saturation, hue, color temperature, brightness, contrast, gamma and RGB gain
controls for Wayland, like the Intel/NVIDIA/AMD control panels on Windows.

Adjustments are applied by the display hardware itself (the CRTC color matrix
and degamma LUT), so they cost nothing in performance, affect everything on
screen including games and video, and stay applied after you close the window.

## Install

```
cargo install --git https://github.com/jeebuscrossaint/vision
vision
```

On first launch Vision asks for your password once to grant its helper
(`visiond`) the `CAP_SYS_PTRACE` capability. It asks again after updates,
because reinstalling replaces the binary. After that it shows up in your app
launcher and re-applies your settings at login.

## How it works

Only the compositor (the DRM master) may change display hardware state on
Wayland. `visiond` borrows the compositor's own DRM file descriptor with
`pidfd_getfd(2)` and sets only the `CTM` and `DEGAMMA_LUT` CRTC properties,
which wlroots does not use. `GAMMA_LUT` is left alone, so night-light tools
(wlsunset, gammastep) keep working alongside it.

- Restricted to processes owned by your own user.
- Never holds the compositor's descriptor longer than one update.
- Re-applies after compositor restarts, hotplug, suspend and VT switches.

Tested on mango (wlroots 0.20) with Intel i915. Should work on any compositor
whose GPU driver exposes `CTM` (Intel, AMD, NVIDIA open/proprietary DRM) and
that leaves it untouched.

## Command line

`vision` with no arguments opens the window; with a command it edits the
same settings, so an open window stays in sync. Fish completions are
installed automatically (`~/.local/share/fish/vendor_completions.d`).

```
vision status                      what is applied where
vision on | off | toggle           all adjustments on/off
vision set saturation 140%         set a value (all displays unless -o)
vision adjust temperature -500     nudge a value, handy for keybindings
vision get [field]                 print values
vision reset                       stock colors
vision preset vivid                apply a preset
vision preset save "My Gaming"     save current settings as a preset
vision preset delete "My Gaming"
vision -o HDMI-A-1 set hue -10     one display only
vision setup                       one-time permission grant from a terminal
```

Fields: `saturation hue temperature brightness contrast gamma red green blue`.

Example mango keybindings:

```
bind=SUPER,F9,spawn,vision toggle
bind=SUPER,F10,spawn,vision preset punchy
bind=SUPER,F11,spawn,vision adjust saturation -10
bind=SUPER,F12,spawn,vision adjust saturation +10
```

The helper itself:

```
visiond --once    apply saved settings once and exit
visiond --reset   restore stock colors on every display and exit
```

Settings live in `~/.config/vision/config.json`.

## Uninstall

```
visiond --reset
systemctl --user disable vision.service
cargo uninstall vision
rm ~/.local/share/applications/vision.desktop ~/.config/systemd/user/vision.service
```
