use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::Row;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const DASHBOARD: &str = include_str!("../static/index.html");

/// Latest known state of a device. The sniffer reports flow/temp/kg only while
/// filling and uptime/bad CRC only while idle, so fields are merged, not replaced.
#[derive(Clone, Default, Deserialize, Serialize)]
struct DeviceStatus {
    device_id: String,
    filling: bool,
    flow_kg_h: f64,
    temp_c: f64,
    current_kg: f64,
    bad_crc_count: i64,
    uptime_ms: i64,
}

struct Stored {
    status: DeviceStatus,
    seen_ms: i64,
}

#[derive(Deserialize)]
struct FillPost {
    device_id: String,
    fill_id: String,
    served_kg: f64,
    amount: f64,
    total_served_kg: f64,
    closed_by: String,
    age_ms: i64,
}

struct AppState {
    db: MySqlPool,
    statuses: RwLock<HashMap<String, Stored>>,
    api_key: String,
}

fn now_ms() -> i64 {
    Local::now().timestamp_millis()
}

fn today_start_ms() -> i64 {
    Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .unwrap()
        .timestamp_millis()
}

/// Merge an update into a device's status and mark it as just seen.
fn touch_status(state: &AppState, device_id: &str, f: impl FnOnce(&mut DeviceStatus)) {
    let mut map = state.statuses.write().unwrap();
    let entry = map.entry(device_id.to_string()).or_insert_with(|| Stored {
        status: DeviceStatus {
            device_id: device_id.to_string(),
            ..Default::default()
        },
        seen_ms: 0,
    });
    f(&mut entry.status);
    entry.seen_ms = now_ms();
}

/// Ok(true) stored, Ok(false) this fill_id was already on file (a device retry),
/// Err the database is unhappy and the caller must not confirm the fill.
async fn insert_fill(
    db: &MySqlPool,
    device_id: &str,
    fill_id: &str,
    served_kg: f64,
    amount: f64,
    total_served_kg: f64,
    closed_by: &str,
    age_ms: i64,
) -> Result<bool, sqlx::Error> {
    let ts_ms = now_ms() - age_ms.max(0);
    let result = sqlx::query(
        "INSERT INTO fills
           (fill_id, device_id, ts_ms, served_kg, amount, total_served_kg, closed_by)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(fill_id)
    .bind(device_id)
    .bind(ts_ms)
    .bind(served_kg)
    .bind(amount)
    .bind(total_served_kg)
    .bind(closed_by)
    .execute(db)
    .await;

    match result {
        Ok(_) => Ok(true),
        // a plain INSERT rather than INSERT IGNORE, so only the duplicate is
        // forgiven and a genuine failure still stops the fill being confirmed
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(false),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------- TCP ingest
//
// The sniffer holds one TCP connection open and sends newline terminated lines:
//   S,<device>,FILLING,<flow_kg_h>,<temp_c>,<current_kg>
//   S,<device>,IDLE,<uptime_ms>,<bad_crc_count>
//   F,<device>,<fill_id>,<kg>,<amount>,<total_kg>,<closed_by>,<age_ms>
// A fill line is resent every 3 s until we answer "ACK,<fill_id>".

async fn handle_line(state: &AppState, line: &str) -> Option<String> {
    let f: Vec<&str> = line.split(',').collect();

    match f.first()? {
        &"S" if f.len() >= 5 => {
            let device = f[1];
            match f[2] {
                "FILLING" if f.len() >= 6 => touch_status(state, device, |s| {
                    s.filling = true;
                    s.flow_kg_h = f[3].parse().unwrap_or(0.0);
                    s.temp_c = f[4].parse().unwrap_or(0.0);
                    s.current_kg = f[5].parse().unwrap_or(0.0);
                }),
                "IDLE" => touch_status(state, device, |s| {
                    s.filling = false;
                    s.flow_kg_h = 0.0;
                    s.current_kg = 0.0;
                    s.uptime_ms = f[3].parse().unwrap_or(0);
                    s.bad_crc_count = f[4].parse().unwrap_or(0);
                }),
                _ => return None,
            }
            None
        }
        &"F" if f.len() >= 8 => {
            let fill_id = f[2];
            let stored = insert_fill(
                &state.db,
                f[1],
                fill_id,
                f[3].parse().unwrap_or(0.0),
                f[4].parse().unwrap_or(0.0),
                f[5].parse().unwrap_or(0.0),
                f[6],
                f[7].parse().unwrap_or(0),
            )
            .await;

            touch_status(state, f[1], |_| {});
            match stored {
                Ok(true) => println!("fill stored: {} {}", f[1], fill_id),
                Ok(false) => println!("fill already had: {} {} (retry)", f[1], fill_id),
                // no ACK, so the sniffer keeps it queued and sends it again
                Err(e) => {
                    println!("fill NOT stored: {} {} - {e}", f[1], fill_id);
                    return None;
                }
            }
            Some(format!("ACK,{fill_id}\n"))
        }
        _ => None,
    }
}

async fn handle_conn(state: Arc<AppState>, mut sock: TcpStream, peer: String) {
    println!("device connected: {peer}");
    let (r, mut w) = sock.split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(reply) = handle_line(&state, line.trim()).await {
            if w.write_all(reply.as_bytes()).await.is_err() {
                break;
            }
        }
    }
    println!("device disconnected: {peer}");
}

async fn tcp_ingest(state: Arc<AppState>, addr: String, port: u16) {
    let listener = TcpListener::bind((addr.as_str(), port))
        .await
        .expect("bind tcp port");
    println!("device port listening on {addr}:{port}");
    loop {
        match listener.accept().await {
            Ok((sock, peer)) => {
                let state = state.clone();
                tokio::spawn(handle_conn(state, sock, peer.to_string()));
            }
            Err(e) => println!("accept failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------- HTTP
//
// The device speaks TCP, but these two routes stay so the dashboard can be
// exercised with curl and fake data (see README).

fn check_key(headers: &HeaderMap, state: &AppState) -> Result<(), StatusCode> {
    let given = headers
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if given == state.api_key {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn post_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DeviceStatus>,
) -> Result<StatusCode, StatusCode> {
    check_key(&headers, &state)?;
    let device_id = body.device_id.clone();
    touch_status(&state, &device_id, move |s| *s = body);
    Ok(StatusCode::OK)
}

async fn post_fill(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<FillPost>,
) -> Result<StatusCode, StatusCode> {
    check_key(&headers, &state)?;
    // a duplicate still answers 200 so a retrying client stops
    insert_fill(
        &state.db,
        &body.device_id,
        &body.fill_id,
        body.served_kg,
        body.amount,
        body.total_served_kg,
        &body.closed_by,
        body.age_ms,
    )
    .await
    .map_err(|e| {
        println!("fill NOT stored: {} - {e}", body.fill_id);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::OK)
}

async fn get_summary(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let now = now_ms();
    let today = today_start_ms();

    // snapshot the live statuses so the lock is not held across the queries
    let live: HashMap<String, (DeviceStatus, i64)> = {
        let map = state.statuses.read().unwrap();
        map.iter()
            .map(|(id, s)| (id.clone(), (s.status.clone(), s.seen_ms)))
            .collect()
    };

    let db = |e: sqlx::Error| {
        println!("summary query failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    };

    // every device we have ever heard from, live or from history
    let mut ids: Vec<String> = live.keys().cloned().collect();
    let rows = sqlx::query("SELECT DISTINCT device_id FROM fills")
        .fetch_all(&state.db)
        .await
        .map_err(db)?;
    for row in rows {
        let id: String = row.get(0);
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.sort();

    let mut devices = Vec::new();
    for id in ids {
        let stored = live.get(&id);

        let rows = sqlx::query(
            "SELECT ts_ms, served_kg, amount, closed_by FROM fills
             WHERE device_id = ? ORDER BY ts_ms DESC LIMIT 10",
        )
        .bind(&id)
        .fetch_all(&state.db)
        .await
        .map_err(db)?;

        let fills: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "ts_ms": r.get::<i64, _>(0),
                    "served_kg": r.get::<f64, _>(1),
                    "amount": r.get::<f64, _>(2),
                    "closed_by": r.get::<String, _>(3),
                })
            })
            .collect();

        // SUM() is NULL when nothing matches, so decode as Option and default here
        let mut totals = Vec::new();
        for since in [today, 0] {
            let row = sqlx::query(
                "SELECT SUM(served_kg), SUM(amount) FROM fills
                 WHERE device_id = ? AND ts_ms >= ?",
            )
            .bind(&id)
            .bind(since)
            .fetch_one(&state.db)
            .await
            .map_err(db)?;
            totals.push((
                row.get::<Option<f64>, _>(0).unwrap_or(0.0),
                row.get::<Option<f64>, _>(1).unwrap_or(0.0),
            ));
        }

        devices.push(json!({
            "device_id": id,
            // idle status arrives every 5 s, filling status every 2 s
            "online": stored.map(|(_, seen)| now - seen < 12_000).unwrap_or(false),
            "last_seen_ms": stored.map(|(_, seen)| *seen),
            "status": stored.map(|(s, _)| s),
            "today": { "kg": totals[0].0, "amount": totals[0].1 },
            "all_time": { "kg": totals[1].0, "amount": totals[1].1 },
            "fills": fills,
        }));
    }

    Ok(Json(json!({ "now_ms": now, "devices": devices })))
}

async fn get_dashboard() -> Html<&'static str> {
    Html(DASHBOARD)
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let tcp_port: u16 = std::env::var("TCP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5000);
    let tcp_addr = std::env::var("TCP_ADDR").unwrap_or_else(|_| "192.46.236.241".to_string());
    let api_key = std::env::var("API_KEY").unwrap_or_else(|_| "change-me".to_string());
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://cng:change-this-password@127.0.0.1:3306/cng".to_string());

    let db = MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("connect to MariaDB");

    // fail loudly at boot rather than on the first fill
    sqlx::query("SELECT 1 FROM fills LIMIT 1")
        .fetch_optional(&db)
        .await
        .expect("query the fills table - has schema.sql been run?");
    println!("database ready");

    let state = Arc::new(AppState {
        db,
        statuses: RwLock::new(HashMap::new()),
        api_key,
    });

    tokio::spawn(tcp_ingest(state.clone(), tcp_addr, tcp_port));

    let app = Router::new()
        .route("/", get(get_dashboard))
        .route("/api/status", post(post_status))
        .route("/api/fill", post(post_fill))
        .route("/api/summary", get(get_summary))
        .with_state(state);

    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind http port");
    println!("dashboard on http://0.0.0.0:{port}");
    axum::serve(listener, app).await.unwrap();
}
