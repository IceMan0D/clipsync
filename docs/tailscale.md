# Syncing over Tailscale (manual two-machine test)

Each paired device keeps an ordered list of candidate addresses and tries them in order on
every connection attempt:

1. addresses found by mDNS on the local network,
2. manually entered addresses, in the order they were added (Tailscale `100.x` IP, MagicDNS
   name, or any other host).

If an address fails, the next one is tried. MagicDNS / host names are re-resolved on every
attempt (backoff 1 s up to 30 s), so a changed IP is picked up automatically.

## Address format

`address` or `address:port`. The port defaults to **43872**.

| Input                        | Meaning                              |
| ---------------------------- | ------------------------------------ |
| `100.101.102.103`            | Tailscale IP, port 43872             |
| `100.101.102.103:5000`       | Tailscale IP, port 5000              |
| `laptop.tail1234.ts.net`     | MagicDNS name, port 43872            |
| `laptop:5000`                | short MagicDNS name, port 5000       |
| `[fd7a:115c::1]:5000`        | IPv6 with port (brackets required)   |

## Procedure

Machines A and B on **different** networks (e.g. A on home Wi-Fi, B on a phone hotspot).

1. Install Tailscale on both and sign in to the same tailnet. Confirm with
   `tailscale ping <other-machine>` from each side. Note each IP (`tailscale ip -4`) and
   MagicDNS name.
2. Allow inbound TCP 43872 on the tailnet interface (Windows: see the firewall section of
   the README; the rule must cover the Tailscale network profile, which Windows often
   classes as Public).
3. Pair. Pairing needs a direct connection, so use the Tailscale address:
   - A: `clipsync pair-listen`
   - B: `clipsync pair-connect <A-tailscale-ip-or-name>:43872`
   - Compare the 6-digit codes and answer `y` on both.
4. On each machine, find the other's key with `clipsync devices` and add its address:
   - B: `clipsync add-address <A-hex-key> <A-tailscale-ip-or-magicdns-name>`
   - A: `clipsync add-address <B-hex-key> <B-tailscale-ip-or-magicdns-name>`

   Addresses are stored in `~/.config/clipsync/addresses.txt` (`%APPDATA%\clipsync\addresses.txt`
   on Windows). Only one side needs an address for sync to work, since either side can dial.
5. Start `clipsync run` on both. Expected: connected within a few seconds.

## Checks

- Copy text on A, paste on B within a second; then the reverse.
- Fallback: `add-address` a dead address first (e.g. `100.64.255.254`), then the real one.
  The connection still comes up (the dead one times out after 3 s).
- MagicDNS re-resolve: use a MagicDNS name, stop `clipsync` on B, restart Tailscale on B (it
  may get a new IP), start `clipsync` again. A reconnects without restarting.
- Bare vs ported: repeat with `<ip>` and `<ip>:43872`; both must work. Run B with
  `clipsync run 5000` and add `<ip>:5000` on A to verify a non-default port.
- Disconnect B from the internet, then reconnect: state returns to Connected via backoff.

Headless coverage of parsing, ordering, de-duplication, re-resolution and fallback lives in
`core/src/addresses.rs` and `core/tests/addresses.rs`.
