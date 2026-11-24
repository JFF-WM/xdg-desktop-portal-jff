# jff-portal

An xdg-desktop-portal **ScreenCast** backend for jff that can share a **single window** (or a
monitor) with OBS, browsers (WebRTC) and any other app that uses the ScreenCast portal.

`xdg-desktop-portal-wlr` only shares monitors and regions, so OBS offers no window source with
it. jff-portal reads pictures through `ext-image-copy-capture-v1` and
`ext-foreign-toplevel-list-v1`, which jff offers for windows as well as monitors, and publishes
them as a PipeWire stream.

## Build and install

```
tools/install-jff.sh          # builds and installs jff and jff-portal
```

or only the portal:

```
cargo build --release --manifest-path portal/Cargo.toml
sudo install -Dm755 portal/target/release/jff-portal /usr/bin/jff-portal
sudo install -Dm644 portal/data/jff.portal /usr/share/xdg-desktop-portal/portals/jff.portal
sudo install -Dm644 portal/data/org.freedesktop.impl.portal.desktop.jff.service \
    /usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.jff.service
sudo install -Dm644 portal/data/xdg-desktop-portal-jff.service \
    /usr/lib/systemd/user/xdg-desktop-portal-jff.service
```

Building needs the PipeWire development files (`libpipewire`) and clang (for the bindings).

## Enable

jff writes `~/.config/xdg-desktop-portal/jff-portals.conf` routing ScreenCast to `jff;wlr` (jff
first, `xdg-desktop-portal-wlr` if jff-portal is not installed). If you edited that file yourself
(it no longer starts with `# managed by jff`), jff leaves it alone: set

```
org.freedesktop.impl.portal.ScreenCast=jff;wlr
```

in it, then `systemctl --user restart xdg-desktop-portal` and restart the app.

In OBS, **Sources > + > Window Capture (PipeWire)** now exists.

## The picker

There is no GUI toolkit in jff-portal. When an app asks to share something, a command is run:
it gets one line per source on stdin and has to print the chosen line (dmenu style):

```
[0] monitor: DP-1
[1] window: Terminal (foot)
```

The default is `fuzzel --dmenu`. Change it in `$XDG_CONFIG_HOME/jff/portal.conf`:

```
chooser = ["wofi", "--dmenu"]
```

or with `JFF_PORTAL_CHOOSER="rofi -dmenu"` in the portal's environment. Only the lines the portal
offered are accepted: anything else the picker prints is rejected. Printing nothing (Escape) cancels.

## Limits

- Windows and monitors only; no virtual monitors, no input injection, no RemoteDesktop.
- The cursor is not part of a window picture: only "hidden" cursor mode is offered.
- The stream is offered as BGRx (opaque) first and BGRA second. Both are written with a fully opaque alpha byte unless the consumer picked BGRA and the source really has alpha.
- Pictures go through `wl_shm` and a CPU copy: fine for calls and recording, not for 4K at 60.
- jff only sends a picture when the window changed; the last one is repeated for the consumer.
- When a shared window closes, its stream and the session end.
- jff-portal must not run inside a sandbox: jff hides the window list from sandboxed clients.

## Debugging

`RUST_LOG=debug jff-portal`, `journalctl --user -u xdg-desktop-portal -b`,
`pw-cli ls Node | grep -i jff`.
