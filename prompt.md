# Task: MVP dashboard for a CNG dispenser flowmeter sniffer

Build a minimal, working MVP. Keep it simple and readable — no over-engineering,
no extra features beyond what is listed. Light comments are fine, no heavy
explanatory commenting or defensive scaffolding.

## Context (confirmed facts — do not change these assumptions)

A NodeMCU (ESP8266) passively sniffs the RS485 Modbus RTU bus between a CNG
dispenser controller (master) and an Emerson Micro Motion flowmeter (slave addr 1),
9600 8N1. It never transmits on the bus.

Observed bus behaviour:
- Idle: controller writes `01 06 00 07 00 00 38 0B` once per second (meter echoes it).
- Filling: controller polls `01 03 00 F6 00 0E` (14 regs from 40247) at ~10 Hz.
  Reply is 33 bytes: `01 03 1C` + 28 data bytes + CRC.
- Floats are word-swapped (CDAB).
- Reply data offsets (index into the 33-byte frame):
  - 3  → 40247 mass flow rate (kg/h)
  - 11 → 40251 gas temperature (°C)
  - 27 → 40259 mass total for the current fill (kg; resets to ~0 each fill)
- A fill ends when the idle FC06 frame reappears, or after 10 s of bus silence.
- Units assumed kg and kg/h (still to be confirmed against the dispenser display).

The existing sniffer firmware (below) already decodes this and prints a report
at the end of each fill. Price per kg is 1550.

<PASTE THE CURRENT SNIFFER .ino HERE>

## Deliverables

### 1. Firmware changes (NodeMCU, Arduino IDE, ESP8266 core)

Modify the existing sketch — keep its style: plain globals, straight-line
`loop()`, `Serial.print` output, few helpers. Pins stay as they are.

Add WiFi + HTTP POST (JSON) to the backend:
- **Live status** every 2 s: `device_id`, `filling` (bool), `flow_kg_h`,
  `temp_c`, `current_kg`, `bad_crc_count`, `uptime_ms`.
- **Fill complete** event once per fill: `device_id`, `fill_id`,
  `served_kg`, `amount`, `total_served_kg`, `closed_by` ("idle" or "silence"),
  `age_ms` (millis since the fill closed, so the backend can back-date it).

Hard constraints:
- The sniffer's frame timing must not break. HTTP calls block for hundreds of ms
  and SoftwareSerial's RX buffer is small, so: never POST in the middle of a
  frame; keep live posts short with a short timeout; accept that some frames are
  dropped during a post (the meter total is authoritative, so missed frames do
  not lose kg). Frames that merge fail the CRC and are discarded — that is fine.
- Fill-complete events must not be lost to a WiFi outage: keep a small queue in
  RAM (e.g. 20 events) and retry until the backend returns 200.
- `fill_id` = random boot id + counter, so the backend can de-duplicate retries.
- WiFi SSID/password, backend URL and an API key as constants at the top.

### 2. Backend (Rust)

- `axum` + `tokio` + `serde` + SQLite (`rusqlite` or `sqlx`), single binary.
- `POST /api/status` → keep the latest status per device in memory.
- `POST /api/fill` → insert into SQLite; ignore duplicates by `fill_id`
  (return 200 anyway so the device stops retrying). Event time =
  received time − `age_ms`.
- `GET /api/summary` → per device: latest status, whether it is online
  (status seen in last 10 s), last 10 fills, today's kg and amount,
  all-time kg and amount.
- Both POST routes require header `X-API-Key` matching an env var.
- `GET /` serves the dashboard page (embedded in the binary).
- Config via env vars: `PORT`, `API_KEY`, `DB_PATH`.

### 3. Dashboard (single page)

Plain HTML + CSS + vanilla JS, no framework, no build step, no external CDNs.
Polls `/api/summary` every 2 s. Show per device:
- Online/offline indicator and nozzle state (Idle / Filling).
- Live flow (kg/h), temperature, current fill kg while filling.
- Last served: kg, amount, time, closed_by.
- Today's totals and all-time totals (kg and amount).
- Recent fills table (time, kg, amount, closed_by).
- Bad CRC count (line-quality indicator).
Works on a phone screen.

### 4. Supporting files

- `README.md`: how to build and run, env vars, and `curl` examples that post
  fake status and fill events so the dashboard can be tested without hardware.
- A `Dockerfile` for deploying behind an existing Nginx on a Linux VPS.

## Out of scope for this MVP

Users/login, multiple price tiers, charts, editing records, the dispenser's
host-port protocol (40001+ registers). Mention in the README what the next
steps would be, in two or three lines.