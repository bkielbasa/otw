#!/bin/sh
# Started by X (~/.xinitrc): the OTW desktop, filling the screen.
xset s off -dpms 2>/dev/null || true
xsetroot -solid '#008080' 2>/dev/null || true
export OTW_DRIVE_C="$HOME/.otw/C"
export OTW_FULLSCREEN=1
export OTW_PARALLEL_THREADS="${OTW_PARALLEL_THREADS:-1}"
export OTW_HOST_CLIPBOARD="${OTW_HOST_CLIPBOARD:-0}"
# Firefox: no crash reporter, no sandbox broker, stubs for what OTW lacks.
export OTW_ENV_MOZ_CRASHREPORTER_DISABLE=1
export OTW_ENV_MOZ_DISABLE_CONTENT_SANDBOX=1
export OTW_STUB_MISSING=1
exec /opt/otw/otw-vm --desktop > "$HOME/otw.log" 2>&1
