<img src="assets/icon.svg" width="64" height="64" alt="">

# Cross-platform Clipboard Sync

Created in [T3 Code](https://t3.codes).

## Running the LAN sync daemon

```
clipsync pair-listen            # on machine A: opens a 2-minute pairing window
clipsync pair-connect <A-ip>:43872   # on machine B; compare the 6-digit codes, answer y on both
clipsync run [port]             # on both: announce via mDNS, dial paired peers, sync text
```

Peers find each other with mDNS (`_clipsync._tcp`) and connect over TCP port **43872**
(override with `clipsync run <port>`). Link-local (169.254.x.x) and virtual/bridge adapters
(Hyper-V, WSL, VM, Docker, tunnels) are never announced. If both machines dial at once, the
connection initiated by the peer with the lower node ID is kept. Reconnects back off from 1 s
to 30 s and re-resolve the peer's address on every attempt.

### Tailscale and manual addresses

`clipsync add-address <hex-key> <addr>` adds a Tailscale IP, MagicDNS name or other address
(`address[:port]`, default 43872) to a paired device. Candidates are tried in order: mDNS
first, then manual entries. See [docs/tailscale.md](docs/tailscale.md) for the two-machine
test procedure.

### Windows Defender Firewall

The first time `clipsync` listens on Windows, Defender Firewall asks whether to allow it.
Choose **Private networks** (tick Public only if you really sync on a public network). If the
prompt was dismissed, or sync works only one way (Mac to Windows but not back), add the rule
manually from an elevated PowerShell:

```powershell
New-NetFirewallRule -DisplayName "clipsync" -Direction Inbound -Protocol TCP `
  -LocalPort 43872 -Action Allow -Profile Private
```

mDNS discovery uses UDP 5353 multicast, which Windows allows for private networks by default
(rule "mDNS (UDP-In)"). Without the inbound TCP rule the Windows machine can still dial out,
but the Mac cannot connect to it.

## Tray app

Running `clipsync` with no arguments (or `clipsync app`) starts the Tauri tray app: a menu bar
item on macOS (accessory app, no Dock icon) and a system tray icon on Windows. The menu lists
paired devices with their connection status, a **Pause sync** toggle (stops sending and applying
clipboard changes; connections stay up), Settings, and Quit.

Build prerequisites: macOS needs only the Xcode command line tools. Windows needs the MSVC build
tools (as for any Rust build); the WebView2 runtime is only needed at run time (preinstalled on
Windows 10/11). Linux is not a supported target.
