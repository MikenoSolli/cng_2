-- CNG dispenser dashboard - MariaDB schema
--
--   mysql -u root -p < schema.sql
--
-- Safe to re-run: every statement is IF NOT EXISTS.

CREATE DATABASE IF NOT EXISTS cng
  CHARACTER SET utf8mb4
  COLLATE utf8mb4_unicode_ci;

USE cng;

-- One row per completed fill.
--
-- fill_id is the device's own id, "<boot id hex>-<counter>". The sniffer resends
-- a fill every 3 s until the backend ACKs it, so the same id can arrive several
-- times; the primary key turns those retries into a duplicate-key error that the
-- backend treats as "already stored" instead of counting the kg twice.
CREATE TABLE IF NOT EXISTS fills (
  fill_id         VARCHAR(40)  NOT NULL,
  device_id       VARCHAR(32)  NOT NULL,
  ts_ms           BIGINT       NOT NULL,  -- when the fill closed, unix ms, already back-dated by age_ms
  served_kg       DOUBLE       NOT NULL,
  amount          DOUBLE       NOT NULL,
  total_served_kg DOUBLE       NOT NULL,  -- device running total, resets when the NodeMCU reboots
  closed_by       VARCHAR(16)  NOT NULL,  -- 'idle' or 'silence'
  received_at     TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (fill_id),
  KEY idx_device_ts (device_id, ts_ms)
) ENGINE=InnoDB;

-- Application user. Change the password here and in DATABASE_URL before running.
-- The backend only ever reads and inserts, so it gets nothing else. Use '%'
-- instead of 'localhost' if the backend runs in a container or on another host.
CREATE USER IF NOT EXISTS 'cng'@'localhost' IDENTIFIED BY '12345678';
GRANT SELECT, INSERT ON cng.fills TO 'cng'@'localhost';

-- The backend's MySQL driver speaks mysql_native_password, caching_sha2_password
-- and sha256_password, but not MariaDB's ed25519. If it fails to log in with an
-- authentication plugin error, pin the plugin instead of the line above:
--   CREATE OR REPLACE USER 'cng'@'localhost'
--     IDENTIFIED VIA mysql_native_password USING PASSWORD('change-this-password');

-- Handy when checking totals by hand (ts_ms is unix milliseconds):
--   SELECT device_id,
--          FROM_UNIXTIME(ts_ms / 1000) AS closed_at,
--          served_kg, amount, closed_by
--   FROM fills
--   ORDER BY ts_ms DESC
--   LIMIT 20;
