# ClipSync

ClipSync is a source-built macOS and Windows clipboard sync application. It uses a tray/menu bar app, LAN discovery and encrypted peer-to-peer connections. Linux is not an MVP target.

## Install

Download the latest installer from [Releases](https://github.com/IceMan0D/clipsync/releases): `.dmg` for macOS (Apple Silicon or Intel) and `-setup.exe` for Windows. Builds are unsigned.

### macOS (Homebrew, no Gatekeeper warning)

```sh
brew install --cask iceman0d/tap/clipsync
```

Installing from the `.dmg` instead triggers Gatekeeper: right-click the app and choose **Open**, or run `xattr -cr /Applications/ClipSync.app`.

### Windows

SmartScreen shows "unknown publisher": click **More info**, then **Run anyway**.

Then follow [Run and pair](#run-and-pair).

## Build from source

Install Git and a current stable Rust toolchain using [rustup](https://rustup.rs/) on each machine. Clone this repository and run the commands from its root. The project embeds its UI; Node.js and the Tauri CLI are not required for these Cargo commands. The default build is an unbundled executable; signing and installers are outside the MVP.

### macOS

Install the Xcode command line tools (`xcode-select --install`), then:

```sh
git clone https://github.com/IceMan0D/clipsync.git
cd clipsync
cargo build --workspace --locked
cargo test --workspace --locked
./target/debug/clipsync-app
```

### Windows

Install the Microsoft C++ Build Tools with the Desktop development with C++ workload and Windows SDK, and use the stable MSVC Rust toolchain. WebView2 Runtime is required to run the Tauri UI; install it if it is missing. In PowerShell:

```powershell
git clone https://github.com/IceMan0D/clipsync.git
cd clipsync
cargo build --workspace --locked
cargo test --workspace --locked
.\target\debug\clipsync-app.exe
```

For an optimized executable use `cargo build --workspace --release --locked` and substitute `release` for `debug` in the executable path. Keep the executable at a stable path if enabling start at login. Do not run the tray app and CLI daemon simultaneously: they use the same listening port.

## Run and pair

Start the executable on both computers. Open **Settings** from the menu bar or tray, click **Pair New Device** on both, compare the six-digit codes, and confirm on both only if they match. The pairing window lasts two minutes. On a LAN, discovery uses mDNS (`_clipsync._tcp`); both devices must be able to reach each other. Settings lists paired devices and connection status, allows removal and manual addresses, and controls start at login. The tray menu also has a pause toggle and Quit.

For Tailscale, enter the other machine's Tailscale IP or MagicDNS name in the optional pairing address field on one side while both pairing windows are open; add a manual address for the paired peer in Settings for subsequent connections. Default TCP port is **43872**. See [Tailscale instructions](docs/tailscale.md).

The executable also has a terminal interface. For example, with the tray app stopped, use `./target/debug/clipsync-app pair-listen` on one machine and `./target/debug/clipsync-app pair-connect <host>:43872` on the other; compare and confirm the codes on both. Then run `./target/debug/clipsync-app run` on both. On Windows use `.\target\debug\clipsync-app.exe` instead. Other commands are `devices`, `add-address <hex-key> <address>`, and `remove <hex-key>`; `run` accepts an optional port. Manual addresses default to 43872. Avoid placing sensitive clipboard contents in logs or verification records.

## Firewall

Allow inbound TCP **43872** on both machines for the intended network. On Windows, allow the executable through Defender Firewall on the appropriate network profile when prompted. If necessary, an administrator can add a port rule in PowerShell:

```powershell
New-NetFirewallRule -DisplayName "ClipSync TCP 43872" -Direction Inbound -Protocol TCP `
  -LocalPort 43872 -Action Allow -Profile Private
```

Use the profile actually assigned to the network; a Tailscale interface may be Public, so a Private-only rule may not apply. Restrict any additional rule to the intended interface or remote addresses. LAN discovery also requires mDNS multicast UDP 5353; check the OS firewall and network isolation settings if discovery fails. On macOS, allow incoming connections for the executable if the application firewall prompts, and configure any other firewall to permit the same traffic. A custom CLI port needs a matching TCP rule.

## Verification

See [the verification record and manual checklist](docs/verification.md). Automated loopback tests are not a substitute for testing a physical Mac and Windows PC.
