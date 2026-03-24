# iwd-tui Design Spec

A terminal UI for managing iwd wireless networks. Launched from waybar, works in
a standard Linux TTY.

## Design Philosophy

SUPERHOT piOS aesthetic meets lf/ncdu functionality. Three conditions (from the
Shoulder Surfing Funnel principle) applied in order:

1. **Information dense** -- every visual element conveys real data. The signal bar
   IS the signal strength. The dBm number IS the measurement. No decoration.
2. **Performant** -- D-Bus queries over system bus, ratatui rendering. Instant.
3. **Then aesthetic** -- monochrome base, block element gradient bars, ALL CAPS
   status words, colored dBm. The layer that makes someone go "what is that?"

## Visual Constraints

- CP437-compatible block elements for signal bars: `░▒▓█`
- ASCII box drawing for borders: `+`, `-`, `|` (no unicode box chars)
- TTY-safe ANSI colors only (basic 8/16 palette)
- Must work at 80x24 minimum
- White on black base, reverse video for selection highlight

## Layout

### Adaptive: Narrow (< 100 cols)

```
 iwd -- wlp0s20f3 -- CONNECTED                          esc:quit
+-------------------------------------------------------------+
|>  MyHomeWifi        ██████████▓▒░  -42 dBm   psk  CONNECTED |
|   CoffeeShop        ██████▓▒░      -58 dBm   psk  known     |
|   Neighbor5G         ████▓▒░        -71 dBm   psk            |
|   NETGEAR-Guest      ██▓▒░          -78 dBm   open           |
|                                                              |
+-------------------------------------------------------------+
 j/k:move  enter:connect  d:disconnect  f:forget  s:scan  a:autoconnect
```

### Adaptive: Wide (100+ cols)

```
 iwd -- wlp0s20f3 -- CONNECTED                                    esc:quit
+--------------------------------------+---------------------------+
|>  MyHomeWifi   ██████████▓▒░ -42 dBm | NETWORK: MyHomeWifi      |
|   CoffeeShop   ██████▓▒░    -58 dBm | Status:  CONNECTED       |
|   Neighbor5G    ████▓▒░      -71 dBm | Type:    psk             |
|   NETGEAR-Guest ██▓▒░        -78 dBm | Channel: 36 (5 GHz)     |
|                                      | RSSI:    -42 dBm         |
|                                      | Tx:      866 Mbit/s      |
|                                      | Rx:      650 Mbit/s      |
|                                      | Cipher:  CCMP            |
|                                      | Uptime:  2h 34m          |
|                                      | AutoConnect: ON          |
|                                      |                          |
+--------------------------------------+---------------------------+
 j/k:move  enter:connect  d:disconnect  f:forget  s:scan  a:autoconnect
```

The detail panel updates live as the cursor moves. For non-connected networks it
shows a reduced set (name, type, known status, signal, AutoConnect) since
diagnostic data is only available for the active connection.

## Components

### Header

```
 iwd -- wlp0s20f3 -- CONNECTED                          esc:quit
```

- Interface name auto-detected from D-Bus Station objects
- State: CONNECTED, DISCONNECTED, CONNECTING, SCANNING
- SCANNING animates: `SCANNING.` `SCANNING..` `SCANNING...` cycling at ~300ms
- Error flash: `FAILED: auth error` displays for ~3 seconds then reverts to
  actual state

### Multiple Adapters

Stations are enumerated from D-Bus (laptop-agnostic, no hardcoded interface name).

- Single adapter: no extra UI, just shown in header
- Multiple adapters: `tab` cycles between them, brackets show active:

```
 iwd -- [wlp0s20f3] / wlp1s0 -- CONNECTED           esc:quit
```

### Network List

Single unified list. Sort order:

1. Currently connected network (always first)
2. Known networks (by signal strength descending)
3. Unknown/new networks (by signal strength descending)

### Row Format

```
{cursor}  {ssid:<20}  {signal_bar} {dbm:>4} dBm   {type:<4}  {status}
```

- `>` cursor for selected row, space otherwise
- SSID left-aligned, truncated at 20 chars
- Signal bar: `█` fill proportional to signal, always ends with `▓▒░` gradient tail
- dBm: space before "dBm" (`-42 dBm` not `-42dBm`)
- Type: `psk`, `open`, `8021x`
- Status: `CONNECTED` (caps), `known`, or blank

### Signal Bar

Block element gradient using `░▒▓█`. The bar is always white. The `▓▒░` tail
always appears to give a smooth SUPERHOT-style falloff.

```
Strong (-30 dBm):  ██████████▓▒░
Good   (-50 dBm):  ████████▓▒░
Fair   (-65 dBm):  █████▓▒░
Weak   (-80 dBm):  ██▓▒░
```

### Signal Color (dBm number only)

The bar stays white. Only the dBm text gets colored:

| Range            | Color          | ANSI |
|------------------|----------------|------|
| -30 to -55 dBm  | Green (strong) | 32   |
| -55 to -70 dBm  | Yellow (fair)  | 33   |
| -70+ dBm        | Red (weak)     | 31   |

### Detail Panel (wide mode only)

Right panel appears when terminal width >= 100 columns.

Connected network shows full diagnostics from `StationDiagnostic.GetDiagnostics()`:

```
NETWORK: MyHomeWifi
Status:  CONNECTED
Type:    psk
Channel: 36 (5 GHz)
RSSI:    -42 dBm
Tx:      866 Mbit/s
Rx:      650 Mbit/s
Cipher:  CCMP
Uptime:  2h 34m
AutoConnect: ON
```

Non-connected networks show reduced info:

```
NETWORK: CoffeeShop
Status:  known
Type:    psk
RSSI:    -58 dBm
AutoConnect: ON
```

### Footer

Keybind hints, ncdu-style:

```
 j/k:move  enter:connect  d:disconnect  f:forget  s:scan  a:autoconnect
```

## Overlays

### Password Entry

Centered overlay box for connecting to new PSK networks:

```
+----------------------------+
| CONNECT: Neighbor5G        |
|                            |
| Password: **************** |
| [v] show password          |
|                            |
| enter:connect  esc:cancel  |
+----------------------------+
```

- Password masked with `*` by default
- `v` toggles show/hide
- `enter` submits, `esc` cancels
- Network name in overlay title

### Forget Confirmation

Small centered overlay:

```
+------------------------+
| FORGET: CoffeeShop?    |
|                        |
| y:confirm  n:cancel    |
+------------------------+
```

## Keybindings

| Key     | Action                                      |
|---------|---------------------------------------------|
| `j`/`k` | Move cursor down/up                        |
| `enter` | Connect to selected network                 |
| `d`     | Disconnect from current network             |
| `f`     | Forget selected known network (with confirm)|
| `s`     | Trigger scan                                |
| `a`     | Toggle AutoConnect on selected known network|
| `tab`   | Cycle adapter (if multiple)                 |
| `v`     | Toggle password visibility (in overlay)     |
| `esc`   | Cancel overlay / quit TUI                   |
| `q`     | Quit TUI                                    |

## Data Sources (iwd D-Bus API)

Service: `net.connman.iwd` on the **system bus**.

| Data                  | D-Bus Call                                     |
|-----------------------|------------------------------------------------|
| List networks         | `Station.GetOrderedNetworks()` -> (path, dBm*100) |
| Network name/type     | `Network.Name`, `Network.Type` properties      |
| Connected status      | `Network.Connected` property                   |
| Known status          | `Network.KnownNetwork` property (non-empty = known) |
| Connect               | `Network.Connect()` method                     |
| Disconnect            | `Station.Disconnect()` method                  |
| Scan                  | `Station.Scan()` method                        |
| Scan status           | `Station.Scanning` property + PropertiesChanged |
| Station state         | `Station.State` property                       |
| Diagnostics           | `StationDiagnostic.GetDiagnostics()` method    |
| Forget network        | `KnownNetwork.Forget()` method                 |
| Toggle AutoConnect    | `KnownNetwork.AutoConnect` property (rw)       |
| Enumerate adapters    | Enumerate objects with `Station` interface      |
| Adapter info          | `Device.Name`, `Device.Address` properties     |

Signal strength from `GetOrderedNetworks()` is `int16` in dBm * 100 (e.g.,
-6000 = -60 dBm). Divide by 100.

Password entry requires registering an `Agent` via `AgentManager.RegisterAgent()`
and implementing `RequestPassphrase` callback on the registered object path.

## Tech Stack

- **Language:** Rust
- **TUI framework:** ratatui
- **D-Bus:** zbus (async, system bus)
- **Async runtime:** tokio
