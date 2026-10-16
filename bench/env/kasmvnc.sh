#!/usr/bin/env bash
# KasmVNC (its own X server, Xvnc) on the benchmark display :5, WebSocket on :6922, with
# the settings Tembo's kasmvnc.yaml expands to (kasmvncserver's generated command line):
# 60 fps cap, JPEG/WebP quality 8-9, video mode scaled to at most 1920x1080, client
# overrides ignored. KASMVNC: the kasmvncserver package (nix/packages/kasmvnc in the monorepo).
K=${KASMVNC:?set KASMVNC to the kasmvncserver package}
D=${KASM_DISPLAY:-:5}
exec "$K/bin/Xvnc" $D -geometry 2560x1600 -depth 24 -ac -websocketPort ${KASM_PORT:-6922} -interface 127.0.0.1 \
  -rfbport -1 -SecurityTypes None -DisableBasicAuth 1 -httpd "$K/share/kasmvnc/www" -xkbdir "${XKB_DIR:?set XKB_DIR to xkeyboard_config/share/X11/xkb}" \
  -FrameRate 60 -DynamicQualityMin 8 -DynamicQualityMax 9 -TreatLossless 9 -JpegVideoQuality 9 -WebpVideoQuality 9 \
  -RectThreads 0 -CompareFB 2 -VideoArea 45 -VideoTime 5 -VideoOutTime 3 -VideoScaling 2 -MaxVideoResolution 1920x1080 \
  -WebpEncodingTime 30 -ScrollDetectLimit 25 -IgnoreClientSettingsKasm 1 -ImprovedHextile 1 -MaxIdleTime 0 -IdleTimeout 0 \
  -MaxConnectionTime 0 -MaxDisconnectionTime 0 -DisconnectClients 0 -AcceptSetDesktopSize 1 -SendCutText 1 -AcceptCutText 1 \
  -DLP_RegionAllowClick 1 -DLP_RegionAllowRelease 1 -Log '*:stdout:30'
