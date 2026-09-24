# CNG dispenser flowmeter dashboard

A NodeMCU passively sniffs the RS485 Modbus RTU bus between the dispenser
controller and the Micro Motion flowmeter, and reports over a plain TCP socket
to a small Rust backend that stores fills in SQLite and serves a live dashboard.

```
firmware/cng_sniffer/cng_sniffer.ino   sniffer + TCP reporting
backend/                               axum + rusqlite server, dashboard embedded
```

## Firmware

Arduino IDE with the ESP8266 core. Edit the constants at the top of the sketch:

```cpp
const char *WIFI_SSID   = "your-ssid";
const char *WIFI_PASS   = "your-password";
const char *SERVER_IP   = "192.168.1.10";
const uint16_t SERVER_PORT = 5000;      // the backend's TCP_PORT
const char *DEVICE_ID   = "D01";
```

Pins are unchanged: `SoftwareSerial mod(0, 2)` and `RE` on GPIO14 held low.

The sketch keeps one TCP connection open and never blocks the bus for long:
reconnects are attempted only while idle, and nothing is written in the middle
of a frame. Fill reports sit in a 20-slot queue and are resent every 3 s until
the backend ACKs them, so a WiFi outage cannot lose a fill. `fill_id` is a
random boot id plus a counter, so a resend is de-duplicated server side.

## Device protocol

Newline terminated CSV, device to server:

| Line | Meaning |
|------|---------|
| `S,<dev>,IDLE,<uptime_ms>,<bad_crc>` | status every 5 s while idle |
| `S,<dev>,FILLING,<flow_kg_h>,<temp_c>,<kg_so_far>` | status every 2 s while filling |
| `F,<dev>,<fill_id>,<kg>,<amount>,<total_kg>,<closed_by>,<age_ms>` | one completed fill |

Server to device, and nothing else — the sketch reads with a 1 s timeout, so the
server must never send unsolicited traffic:

```
ACK,<fill_id>
```

The fill is written to SQLite *before* the ACK, and a `fill_id` already on file
is ACKed again without being stored twice. `age_ms` is how long ago the fill
closed, so the backend back-dates the event time to when it actually happened.

Because flow/temp only appear in `FILLING` lines and uptime/bad CRC only in
`IDLE` lines, the backend merges fields into one record per device rather than
replacing it.

## Database

MariaDB. Create the schema once, as an admin user:

```sh
mysql -u root -p < backend/schema.sql
```

That makes the `cng` database, the `fills` table, and a `cng` user with only
`SELECT` and `INSERT` on that table — edit the password in the script first, and
change `'cng'@'localhost'` to `'cng'@'%'` if the backend runs on another host or
in a container. The script is safe to re-run.

There is only one table. Live status is held in memory and is not persisted:
it is worthless after a restart, since the device resends it within 5 s.

Two things to know if the backend cannot log in. The driver does not support
MariaDB's `ed25519` authentication plugin — `schema.sql` has a commented line
that pins `mysql_native_password` instead. And it is built without TLS, so the
connection must be on localhost or a trusted network; add the `tls-rustls`
feature to `sqlx` in `Cargo.toml` if the database is remote.

## Backend

Needs Rust (stable) to build.

```sh
cd backend
cargo build --release
DATABASE_URL='mysql://cng:change-this-password@127.0.0.1:3306/cng' \
  PORT=8080 TCP_PORT=5000 API_KEY=change-me ./target/release/cng-dashboard
```

Then open `http://localhost:8080/`.

| Env var        | Default                  | Meaning                                       |
|----------------|--------------------------|-----------------------------------------------|
| `DATABASE_URL` | `mysql://cng:change-this-password@127.0.0.1:3306/cng` | MariaDB connection |
| `PORT`         | `8080`                   | HTTP port (dashboard + API)                   |
| `TCP_PORT`     | `5000`                   | device port, the sketch's `SERVER_PORT`       |
| `TCP_ADDR`     | `192.46.236.241`         | address the device port binds to              |
| `API_KEY`      | `change-me`              | required in `X-API-Key` on the two POST routes|

`TCP_ADDR` must be an address the machine actually owns, or the process exits at
startup. Use `0.0.0.0` to listen on every interface, which is what the container
needs since it cannot bind the host's public IP.

The backend refuses to start if it cannot reach the database or the `fills`
table is missing, rather than failing later on the first fill.

Routes:

- `GET /` — the dashboard, polls the summary every 2 s.
- `GET /api/summary` — per device: status, online flag (a line within 12 s, i.e.
  just over two idle intervals), last 10 fills, today's kg/amount, all-time kg/amount.
- `POST /api/status`, `POST /api/fill` — JSON equivalents of the two device
  lines, kept only so the dashboard can be driven with fake data (below). The
  hardware does not use them. Both need `X-API-Key`.

### Docker

```sh
cd backend
docker build -t cng-dashboard .
docker run -d --name cng-dashboard --restart unless-stopped \
  -p 127.0.0.1:8080:8080 -p 5000:5000 \
  --add-host host.docker.internal:host-gateway \
  -e DATABASE_URL='mysql://cng:real-password@host.docker.internal:3306/cng' \
  -e API_KEY=pick-a-real-key \
  cng-dashboard
```

MariaDB stays on the host, so the container reaches it through
`host.docker.internal` and the `cng` user must be granted at `'cng'@'%'`.
No volume is needed any more; the data lives in MariaDB.

The dashboard goes behind the existing Nginx; the device port is plain TCP and
must be published directly (Nginx is not in that path).

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
}
```

## Testing without hardware

Pretend to be the sniffer over TCP — this exercises the real path, ACKs included.
Note the default `TCP_ADDR` binds one public IP, so `localhost` will refuse the
connection; either use that address or start the backend with `TCP_ADDR=0.0.0.0`.

```sh
# needs netcat; -q1 keeps the socket open long enough to read the ACK back
printf 'S,D01,IDLE,845000,3\n' | nc -q1 localhost 5000
printf 'S,D01,FILLING,214.5,31.2,4.317\n' | nc -q1 localhost 5000
printf 'F,D01,A1B2C3D4-7,8.420,13051.00,120.500,idle,1500\n' | nc -q1 localhost 5000
# -> ACK,A1B2C3D4-7 ; sending the same line again ACKs but does not double count
```

Or over HTTP, with the server on port 8080 and `API_KEY=change-me`:

```sh
curl -X POST http://localhost:8080/api/status \
  -H 'Content-Type: application/json' -H 'X-API-Key: change-me' \
  -d '{"device_id":"D01","filling":true,"flow_kg_h":214.5,"temp_c":31.2,
       "current_kg":4.317,"bad_crc_count":3,"uptime_ms":845000}'

curl -X POST http://localhost:8080/api/fill \
  -H 'Content-Type: application/json' -H 'X-API-Key: change-me' \
  -d '{"device_id":"D01","fill_id":"A1B2C3D4-7","served_kg":8.42,
       "amount":13051,"total_served_kg":120.5,"closed_by":"idle","age_ms":1500}'

curl http://localhost:8080/api/summary
```

Repeat a status line every few seconds to keep the device showing as online; stop
and it flips to offline after 12 s.

## Next steps

- Confirm the units and the 40259 register against the dispenser's own display
  before trusting the totals commercially.
- Read the dispenser's host port (40001+) to get the price and nozzle state from
  the controller itself instead of hard-coding 1550 in the sketch.
- Then: login, per-device price history, and charts over the stored fills.
