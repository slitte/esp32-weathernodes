//! WiFi STA connection + MQTT publish (Step 4).

use std::{sync::mpsc, time::Duration};

use anyhow::Context as _;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::modem::Modem,
    mqtt::client::{EspMqttClient, EventPayload, MqttClientConfiguration, QoS},
    nvs::EspDefaultNvsPartition,
    wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi},
};

use crate::{config::Config, sensor::SensorData};

const TAG: &str = "network";

// ─── WiFi STA ─────────────────────────────────────────────────────────────────

pub fn connect_wifi(
    cfg:     &Config,
    modem:   Modem,
    sysloop: EspSystemEventLoop,
) -> anyhow::Result<BlockingWifi<EspWifi<'static>>> {
    let no_nvs: Option<EspDefaultNvsPartition> = None;
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(modem, sysloop.clone(), no_nvs)?,
        sysloop,
    )?;

    let ssid: heapless::String<32> = heapless::String::try_from(cfg.wifi_ssid.as_str())
        .map_err(|_| anyhow::anyhow!("WiFi SSID too long (max 32 bytes)"))?;
    let password: heapless::String<64> = heapless::String::try_from(cfg.wifi_password.as_str())
        .map_err(|_| anyhow::anyhow!("WiFi password too long (max 64 bytes)"))?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid,
        password,
        ..Default::default()
    }))?;

    wifi.start().context("WiFi start")?;
    log::info!(target: TAG, "Connecting to «{}»…", cfg.wifi_ssid);

    // Trigger async connect then poll with an explicit 50 s timeout
    // (mirrors the Arduino sketch's WiFi.waitForConnectResult() loop).
    // Note: this busy-loop feeds the FreeRTOS scheduler via sleep(250 ms); the
    // default IDF task watchdog (30 s) is satisfied as long as the loop exits
    // within that window.  If the timeout is ever raised beyond 30 s the watchdog
    // must be fed explicitly or the timeout reduced.
    wifi.wifi_mut().connect().context("WiFi connect trigger")?;
    let start = std::time::Instant::now();
    loop {
        if wifi.is_connected()? { break; }
        if start.elapsed() >= Duration::from_secs(50) {
            return Err(anyhow::anyhow!("WiFi connect timeout (50 s)"));
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    wifi.wait_netif_up().context("DHCP / netif up")?;

    let ip = wifi.wifi().sta_netif().get_ip_info()?;
    log::info!(target: TAG, "WiFi up – IP {}", ip.ip);
    Ok(wifi)
}

// ─── MQTT publish ─────────────────────────────────────────────────────────────

pub fn publish_mqtt(cfg: &Config, data: &SensorData) -> anyhow::Result<()> {
    let broker = format!("mqtt://{}:{}", cfg.mqtt_server, cfg.mqtt_port);
    let conf = MqttClientConfiguration {
        client_id: Some(cfg.device_name.as_str()),
        username:  Some(cfg.mqtt_user.as_str()).filter(|s| !s.is_empty()),
        password:  Some(cfg.mqtt_password.as_str()).filter(|s| !s.is_empty()),
        ..Default::default()
    };

    log::info!(target: TAG, "MQTT connecting → {broker}");
    let (mut client, mut conn) = EspMqttClient::new(&broker, &conf)?;

    // The IDF MQTT task requires its event queue to be drained continuously.
    // Run that in a background thread; report Connected / Published / failure
    // via dedicated channels.
    let (connected_tx, connected_rx) = mpsc::sync_channel::<bool>(1);
    let (published_tx, published_rx) = mpsc::sync_channel::<bool>(1);

    let conn_thread = std::thread::Builder::new()
        .stack_size(6_144)
        .spawn(move || {
            loop {
                match conn.next() {
                    Ok(ev) => match ev.payload() {
                        EventPayload::Connected(_)  => { connected_tx.try_send(true).ok(); }
                        EventPayload::Published(_)  => { published_tx.try_send(true).ok(); break; }
                        EventPayload::Disconnected  => break,
                        _                           => {}
                    },
                    Err(e) => {
                        log::error!(target: TAG, "MQTT event error: {e}");
                        connected_tx.try_send(false).ok();
                        break;
                    }
                }
            }
        })?;

    // Wait up to 10 s for the broker to accept the connection.
    match connected_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(true)  => {}
        Ok(false) => {
            drop(client);
            conn_thread.join().ok();
            return Err(anyhow::anyhow!("MQTT broker refused connection"));
        }
        Err(_) => {
            drop(client);
            conn_thread.join().ok();
            return Err(anyhow::anyhow!("MQTT connection timeout (10 s)"));
        }
    }

    let payload = build_payload(&cfg.device_name, &cfg.room, data);
    log::info!(target: TAG, "→ «{}»: {payload}", cfg.mqtt_topic);
    // QoS 1: broker must acknowledge; we wait for the Published event.
    client
        .publish(&cfg.mqtt_topic, QoS::AtLeastOnce, false, payload.as_bytes())
        .context("MQTT publish")?;

    // Wait up to 5 s for the PUBACK from the broker.
    match published_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(_)  => {}
        Err(_) => log::warn!(target: TAG, "MQTT PUBACK not received within 5 s"),
    }

    drop(client);
    conn_thread.join().ok();
    log::info!(target: TAG, "MQTT done");
    Ok(())
}

// ─── JSON payload ─────────────────────────────────────────────────────────────

/// Builds the JSON payload.
/// Only sensor fields enabled in the config are included.
/// `room` is omitted when empty.
fn build_payload(device_name: &str, room: &str, data: &SensorData) -> String {
    let mut kv: Vec<String> = Vec::with_capacity(6);
    kv.push(format!("\"ESPName\":\"{}\"", json_escape(device_name)));
    if !room.is_empty() { kv.push(format!("\"room\":\"{}\"", json_escape(room))); }
    if let Some(t) = data.temperature     { kv.push(format!("\"temp\":{t:.2}")); }
    if let Some(p) = data.pressure        { kv.push(format!("\"pres\":{p:.2}")); }
    if let Some(h) = data.humidity        { kv.push(format!("\"humi\":{h:.2}")); }
    if let Some(v) = data.battery_voltage { kv.push(format!("\"batvoltvin\":{v:.2}")); }
    format!("{{{}}}", kv.join(","))
}

/// Escape `\` and `"` so the value is safe inside a JSON string literal.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn data_full() -> SensorData {
        SensorData {
            temperature:     Some(21.50),
            pressure:        Some(1013.25),
            humidity:        Some(55.00),
            battery_voltage: Some(3.80),
        }
    }

    #[allow(dead_code)]
    fn data_empty() -> SensorData {
        SensorData {
            temperature:     None,
            pressure:        None,
            humidity:        None,
            battery_voltage: None,
        }
    }

    #[test]
    fn json_escape_plain() {
        assert_eq!(json_escape("hello"), "hello");
    }

    #[test]
    fn json_escape_quotes() {
        assert_eq!(json_escape(r#"say "hi""#), r#"say \"hi\""#);
    }

    #[test]
    fn json_escape_backslash() {
        assert_eq!(json_escape(r"path\to"), r"path\\to");
    }

    #[test]
    fn json_escape_both() {
        assert_eq!(json_escape(r#"a\"b"#), r#"a\\\"b"#);
    }

    #[test]
    fn build_payload_full() {
        let p = build_payload("node1", "living", &data_full());
        assert_eq!(
            p,
            r#"{"ESPName":"node1","room":"living","temp":21.50,"pres":1013.25,"humi":55.00,"batvoltvin":3.80}"#
        );
    }

    #[test]
    fn build_payload_no_room() {
        let p = build_payload("node1", "", &data_empty());
        assert_eq!(p, r#"{"ESPName":"node1"}"#);
    }

    #[test]
    fn build_payload_escapes_injection() {
        let p = build_payload(r#"evil"device"#, "", &data_empty());
        assert!(p.contains(r#"\"device\""#), "name must be escaped: {p}");
    }

    #[test]
    fn build_payload_room_escaped() {
        let p = build_payload("n", r#"room\1"#, &data_empty());
        assert!(p.contains(r#"room\\1"#), "room must be escaped: {p}");
    }
}
