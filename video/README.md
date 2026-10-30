# Companion Capture — product video

Storyboard for the 16:9 and 9:16 cuts (`storyboard.json`, video-kit 0.35.0). Videos are build output: nothing rendered is committed.

```bash
K=<CI-Common>/packages/video-kit
node --experimental-strip-types $K/bin/ci-video.ts build && ... check && ... render   # from this directory
```

## Footage: honest scope

There is no capture stage. The app is a native egui tray app, so the two real window captures already committed in `docs/assets/` are reused:

- `assets/shots/capture-first-run.png` is `docs/assets/screenshot-first-run.png` cropped to its top 1215 px.
- `assets/shots/capture-settings-connection.png` is `docs/assets/screenshot-settings-connection.png` cropped to its top 1035 px.

Crops (`ffmpeg -vf crop=1680:1215:0:0` and `crop=1800:1035:0:0`) remove regions that are defective in the source captures: text clipped at the window bottom in the first-run shot, a clipped "Test connection" button, a home-directory path in the settings footer, and the red "Not set up" empty state. The real fix is a re-shoot with a taller window; these crops are a stopgap.

Deliberately not used: `check-server.svg` (shows the retired `ci-applet` client id) and `tray-menu.svg` (an illustration, documented as out of date).

Screens: only first-run and Settings, Connection. No tray menu, upload window or capture-in-progress footage exists yet, so the film is short on app footage and says only what those two screens and the README support.
