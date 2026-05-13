# idlemon

A lightweight Mutter `IdleMonitor` D-Bus shim for X11.

idlemon implements the subset of the `org.gnome.Mutter.IdleMonitor` D-Bus interface that desktop applications rely on for idle detection, backed by the X11 MIT-SCREEN-SAVER extension. It exists so that applications built for GNOME, for example 1Password's auto lock, work correctly on standalone window managers such as i3, bspwm, awesome, or dwm.

<img width="1496" height="923" alt="screenshot" src="https://github.com/user-attachments/assets/3fe992dc-6487-421f-b0c7-1dc24fbcb4a4" />

## Background

1Password for Linux uses `org.gnome.Mutter.IdleMonitor` to power its "lock after the computer has been idle for N minutes" feature. The service is normally provided by Mutter, the GNOME compositor, and is therefore available on GNOME and any environment that re-exports it. On a standalone window manager such as i3 it is missing entirely, and 1Password responds with:

> Your current desktop environment does not support Auto-lock. 1Password will not lock when your computer is idle.

The same gap affects any other application that depends on the Mutter idle monitor interface for inactivity detection. idlemon fills that gap.

## Installation

Arch Linux users can install the [`idlemon`](https://aur.archlinux.org/packages/idlemon) package from the AUR:

```bash
yay -S idlemon
```

Otherwise, build and install from source with cargo:

```bash
cargo install --locked --git https://github.com/k4yt3x/idlemon.git
```

## Usage

Start the daemon from your window manager's startup script. For i3, add to `~/.config/i3/config`:

```
exec --no-startup-id /usr/bin/idlemon
```

Logging level is controlled by the `RUST_LOG` environment variable and defaults to `info`.

## Verification

Confirm the daemon is reachable:

```bash
dbus-send --session --print-reply \
  --dest=org.gnome.Mutter.IdleMonitor \
  /org/gnome/Mutter/IdleMonitor/Core \
  org.gnome.Mutter.IdleMonitor.GetIdletime
```

The return value is the time in milliseconds since the last keyboard or pointer input event, and resets to zero on any input.

For 1Password specifically: launch the app and navigate to **Settings → Security**. The "Auto-lock unsupported" banner should be gone and an idle-timeout selector should appear in its place.

## Scope

idlemon implements only what real world consumers actually call: `GetIdletime`, `AddIdleWatch`, `AddUserActiveWatch`, `RemoveWatch`, and the `WatchFired` signal. It deliberately does not implement per device idle monitors (`/org/gnome/Mutter/IdleMonitor/<id>`), nor any Wayland equivalent. Wayland users should rely on the `ext-idle-notify-v1` protocol via a compositor that supports it.

## AI Use Declaration

AI tools were used to assist the design and implementation of this project. All design decisions were made by humans, and every change was reviewed and approved by a human maintainer.
