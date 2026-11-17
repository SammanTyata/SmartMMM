#!/bin/sh
# SmartMMM kiosk watchdog.
# 1. Waits until the SmartMMM server answers on localhost:8080
# 2. Launches Chromium in kiosk mode
# 3. Relaunches it if it ever exits (e.g. renderer/network-service crash),
#    which prevents the "stuck white screen" after a crash.

URL="http://localhost:8080/"

wait_for_server() {
    tries=0
    while [ "$tries" -lt 60 ]; do
        code="$(curl -s -o /dev/null -w '%{http_code}' "$URL" 2>/dev/null)"
        if [ "$code" = "200" ]; then
            return 0
        fi
        tries=$((tries + 1))
        sleep 1
    done
    return 1
}

while true; do
    if wait_for_server; then
        chromium-browser \
            --ozone-platform=wayland \
            --use-angle=gles \
            --kiosk \
            --noerrdialogs \
            --disable-infobars \
            --no-first-run \
            --no-default-browser-check \
            --disable-session-crashed-bullet \
            --hide-scrollbars \
            "$URL"
    fi
    # Chromium exited or server never came up — brief pause, then retry.
    sleep 5
done
