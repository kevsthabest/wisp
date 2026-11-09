# Changelog

User-facing changes in each release. Each release on GitHub lists these notes and its pull requests.

## Unreleased

### New

- **Preview an app's windows.** Hovering an app in the dock shows a live preview of each of its windows over the window's title, instead of titles alone. Click a preview to switch to that window, or middle-click to close it. With `window_list = "always"`, previews appear for single-window apps too.

## 0.6.1 - 2026-11-09

### Fixes

- The release zip has Wisp's files at its top level, so the install path stays the same across updates. (#50)
- Wisp installed with winget updates through `winget upgrade` instead of the Update button in Settings. (#50)

## 0.6.0 - 2026-11-09

### New

- **See an app's windows.** Hover an app with several windows to list them. Click one to switch to it, or middle-click to close it. (#21, by @JulienMaille)
- **Choose the dock's size.** Set the app icon size in Settings, from 24 to 40. The dock's height, spacing and status area scale with it. (#39, by @JulienMaille)

### Fixes

- Wisp hides the taskbar reliably when it starts. Before, the taskbar sometimes stayed visible for several seconds, especially with a second screen. (#30, by @RayTrunk, fixes #28)
- The Windows taskbar no longer flashes when you switch the dock between autohide and Keep the dock visible. (#24, by @RayTrunk, fixes #18)
- Notification icons keep reaching Wisp when Explorer shows its taskbar. (#38, fixes #19)
- Right-clicking the dock no longer shows a "Wisp" title strip across its top. (#34, by @RayTrunk, fixes #33)
- The best match stays selected while typing in the launcher when the mouse rests over the results. Move the mouse to highlight another result. (#36, fixes #35)
- Time searches now find English city names on non-English Windows, while still finding names in the Windows display language. (#37, fixes #26)
- The settings app is a third smaller to download (15.2 MB → 10.0 MB). (#46)
- Updates download the release zip, and Wisp tells you where to get Settings if it is missing. (#47)

## 0.5.0 - 2026-11-07

### New

- **Keep the dock visible.** A new option in Settings keeps the dock on screen and reserves its space, so maximized windows stop above it instead of going behind it. Full-screen apps still hide it, including apps that go full screen from a maximized window. (#9)
- **Click the app in front to minimize it**, like a taskbar button. Click it again to bring it back. (#14, by @JulienMaille)

### Fixes

- Wisp now hides the taskbar when you start it from the Start menu. Before, it could fail with "Could not find primary taskbar". (#8)
- Clicking an app in the dock now switches to it while Action Center, Start or search is open. (#13, by @JulienMaille, fixes #11)
- Dock icons keep their place when clicked instead of moving to the front. (#5, by @JulienMaille)
- Tray icons are sharp instead of pixelated. (#3, by @JulienMaille, fixes #2)
- Windows 10: icons and text use the Windows 10 fonts instead of drawing as boxes. (#10, for #1)
- The tray no longer comes up empty after starting Wisp from the Start menu. (#9)
- Turning "Keep the dock visible" on or off no longer freezes the dock and launcher for up to two seconds. (#9)
- Taskbar hiding retries reliably after Explorer restarts and display changes. (#8)
- An icon no longer stays raised after hovering the hidden-icons arrow. (#16)
- The "Show hidden icons" tooltip no longer covers its own flyout. (#17)
- Configs from older versions pick up new settings from the defaults. Configs from Wisp 0.3 keep the taskbar as before. (#8)

### Thanks

@JulienMaille contributed their first pull requests to Wisp in this release: #3, #5, #13 and #14.

## Earlier releases

See the [releases page](https://github.com/SumirSeth/wisp/releases) for 0.4.2 and earlier.
