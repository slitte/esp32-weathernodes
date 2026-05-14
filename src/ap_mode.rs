//! AP+STA config portal (Step 2+).
//!
//! Runs the ESP32 in APSTA mode so the AP stays up the entire time:
//!   - AP  «weathernode-<chipid>» is always reachable on 192.168.71.1
//!   - STA can be tested on-the-fly without disrupting the portal
//!
//! HTTP endpoints
//!   GET  /           → configuration form
//!   POST /save       → persist to NVS + restart
//!   POST /test_wifi  → test STA credentials, return JSON {ok, msg}
//!   POST /test_mqtt  → test MQTT broker,     return JSON {ok, msg}
//!   POST /sensor     → read BME280,          return JSON {ok, temp, pres, humi}

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use embedded_svc::{http::Method, io::Write as _};
use esp_idf_hal::{
    adc::ADC1,
    gpio::AnyIOPin,
    i2c::I2C0,
    peripheral::Peripheral as _, // clone_unchecked
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::modem::Modem,
    http::server::{Configuration as HttpConfig, EspHttpServer},
    mqtt::client::{EspMqttClient, EventPayload, MqttClientConfiguration},
    nvs::EspDefaultNvsPartition,
    wifi::{
        AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration,
        EspWifi,
    },
};

use crate::config::{self, Config};
use crate::sensor;

const TAG: &str = "ap_mode";

// ---------------------------------------------------------------------------
// Internal shared state
// ---------------------------------------------------------------------------

struct WifiState {
    wifi: BlockingWifi<EspWifi<'static>>,
    ap_ssid: heapless::String<32>,
}

struct TestResult {
    ok: bool,
    msg: String,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(modem: Modem, sysloop: EspSystemEventLoop, i2c0: I2C0, adc1: ADC1) -> anyhow::Result<()> {
    let chip_id = get_chip_id();
    let ap_ssid_str = format!("weathernode-{chip_id}");

    let ap_ssid: heapless::String<32> = heapless::String::try_from(ap_ssid_str.as_str())
        .map_err(|_| anyhow::anyhow!("AP SSID zu lang"))?;

    log::info!(target: TAG, "Starte APSTA-Portal «{ap_ssid}»");

    let wifi = start_apsta(modem, sysloop, &ap_ssid)?;
    log::info!(target: TAG,
        "AP oben – mit «{ap_ssid}» verbinden, dann http://192.168.71.1 öffnen");

    let wifi_state = Arc::new(Mutex::new(WifiState { wifi, ap_ssid }));
    let i2c_shared = Arc::new(Mutex::new(i2c0));
    let adc_shared = Arc::new(Mutex::new(adc1));
    let pending: Arc<Mutex<Option<Config>>> = Arc::new(Mutex::new(None));

    let srv_cfg = HttpConfig {
        stack_size: 12_288,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&srv_cfg)?;

    // ── GET / → Formular ─────────────────────────────────────────────────────
    server.fn_handler("/", Method::Get, |req| {
        req.into_ok_response()?.write_all(HTML_FORM.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /config → current NVS config as JSON (for form pre-population) ──
    server.fn_handler("/config", Method::Get, |req| {
        let json = match config::load_from_nvs() {
            Ok(Some(c)) => {
                let state = match c.setup_state() {
                    config::SetupState::Incomplete => "incomplete",
                    config::SetupState::WifiOnly   => "wifi_only",
                    config::SetupState::Complete    => "complete",
                };
                format!(
                    r#"{{"configured":true,"setup_state":"{state}","device_name":"{dn}","room":"{rm}","wifi_ssid":"{ss}","wifi_password_saved":{wps},"mqtt_server":"{ms}","mqtt_port":{mp},"mqtt_user":"{mu}","mqtt_password_saved":{mps},"mqtt_topic":"{mt}","sleep_minutes":{sm},"mqtt_qos":{qos},"sda_pin":{sda},"scl_pin":{scl},"adc_pin":{adc},"send_temperature":{st},"send_pressure":{sp},"send_humidity":{sh},"send_battery":{sb}}}"#,
                    dn  = json_str(&c.device_name),
                    rm  = json_str(&c.room),
                    ss  = json_str(&c.wifi_ssid),
                    wps = !c.wifi_password.is_empty(),
                    ms  = json_str(&c.mqtt_server),
                    mp  = c.mqtt_port,
                    mu  = json_str(&c.mqtt_user),
                    mps = !c.mqtt_password.is_empty(),
                    mt  = json_str(&c.mqtt_topic),
                    sm  = c.sleep_minutes,
                    qos = c.mqtt_qos,
                    sda = c.sda_pin,
                    scl = c.scl_pin,
                    adc = c.adc_pin,
                    st  = c.send_temperature,
                    sp  = c.send_pressure,
                    sh  = c.send_humidity,
                    sb  = c.send_battery,
                )
            }
            _ => r#"{"configured":false,"setup_state":"incomplete"}"#.to_string(),
        };
        req.into_ok_response()?.write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /save ────────────────────────────────────────────────────────────
    {
        let pending = Arc::clone(&pending);
        server.fn_handler("/save", Method::Post, move |mut req| {
            let body = read_body(&mut req);
            let cfg = parse_form(&body);
            log::info!(target: TAG,
                "Config erhalten: device='{}' ssid='{}'",
                cfg.device_name, cfg.wifi_ssid);
            if let Err(e) = cfg.validate() {
                log::warn!(target: TAG, "Config ungültig: {e}");
                let html = format_save_error(&e.to_string());
                req.into_ok_response()?.write_all(html.as_bytes())?;
                return Ok::<(), anyhow::Error>(());
            }
            // Ignore poison – the polling loop will retry next cycle.
            if let Ok(mut g) = pending.lock() {
                *g = Some(cfg);
            }
            req.into_ok_response()?.write_all(HTML_SAVED.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    // ── POST /save_wifi ───────────────────────────────────────────────────────
    // Verifies the credentials by actually connecting before writing to NVS.
    // This is the ONLY path that may update wifi_ssid / wifi_password.
    {
        let ws = Arc::clone(&wifi_state);
        server.fn_handler("/save_wifi", Method::Post, move |mut req| {
            let body = read_body(&mut req);
            let params = parse_kv(&body);
            let ssid = params.get("wifi_ssid").cloned().unwrap_or_default();
            let pass_form = params.get("wifi_password").cloned().unwrap_or_default();

            if ssid.is_empty() {
                let json = r#"{"ok":false,"msg":"SSID darf nicht leer sein"}"#;
                req.into_ok_response()?.write_all(json.as_bytes())?;
                return Ok::<(), anyhow::Error>(());
            }

            // Empty password field means "keep the saved one" – avoids forcing
            // the user to re-type a password that is already stored.
            let pass = if pass_form.is_empty() {
                config::load_from_nvs()
                    .ok()
                    .flatten()
                    .map(|c| c.wifi_password)
                    .unwrap_or_default()
            } else {
                pass_form
            };

            // Real connection test – only succeeds if SSID+password are correct.
            let r = test_wifi_connection(&ws, &ssid, &pass);
            if !r.ok {
                let json = format!(r#"{{"ok":false,"msg":"{}"}}"#, json_str(&r.msg));
                req.into_ok_response()?.write_all(json.as_bytes())?;
                return Ok::<(), anyhow::Error>(());
            }

            // Connection verified → persist.  Load existing config to keep all
            // other fields intact; fall back to defaults for first-time setup.
            let mut cfg = config::load_from_nvs()
                .ok()
                .flatten()
                .unwrap_or_default();
            cfg.wifi_ssid = ssid;
            cfg.wifi_password = pass;

            let json = match config::save_to_nvs(&cfg) {
                Ok(_) => r#"{"ok":true,"msg":"WLAN gespeichert und verifiziert ✓"}"#.to_string(),
                Err(e) => format!(r#"{{"ok":false,"msg":"{}"}}"#, json_str(&e.to_string())),
            };
            req.into_ok_response()?.write_all(json.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    // ── POST /test_wifi ───────────────────────────────────────────────────────
    {
        let ws = Arc::clone(&wifi_state);
        server.fn_handler("/test_wifi", Method::Post, move |mut req| {
            let body = read_body(&mut req);
            let params = parse_kv(&body);
            let ssid = params.get("wifi_ssid").cloned().unwrap_or_default();
            let pass = params.get("wifi_password").cloned().unwrap_or_default();

            let r = test_wifi_connection(&ws, &ssid, &pass);
            let json = format!(r#"{{"ok":{},"msg":{:?}}}"#, r.ok, r.msg);
            req.into_ok_response()?.write_all(json.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    // ── POST /test_mqtt ───────────────────────────────────────────────────────
    {
        let ws = Arc::clone(&wifi_state);
        server.fn_handler("/test_mqtt", Method::Post, move |mut req| {
            let body = read_body(&mut req);
            let params = parse_kv(&body);
            let srv = params.get("mqtt_server").cloned().unwrap_or_default();
            let port = params
                .get("mqtt_port")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1883u16);
            let user = params.get("mqtt_user").cloned().unwrap_or_default();
            let pass = params.get("mqtt_password").cloned().unwrap_or_default();

            let r = test_mqtt_connection(&ws, &srv, port, &user, &pass);
            let json = format!(r#"{{"ok":{},"msg":{:?}}}"#, r.ok, r.msg);
            req.into_ok_response()?.write_all(json.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    // ── POST /test_bat ────────────────────────────────────────────────────────
    {
        let as_ = Arc::clone(&adc_shared);
        server.fn_handler("/test_bat", Method::Post, move |mut req| {
            let body = read_body(&mut req);
            let params = parse_kv(&body);
            let pin = params
                .get("adc_pin")
                .and_then(|s| s.parse().ok())
                .unwrap_or(33u8);
            let json = read_bat_json(&as_, pin);
            req.into_ok_response()?.write_all(json.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    // ── POST /update ─────────────────────────────────────────────────────────
    // Saves config to NVS immediately without restarting.  Returns JSON.
    // WiFi credentials are intentionally NOT updated here – use /save_wifi.
    server.fn_handler("/update", Method::Post, |mut req| {
        let body = read_body(&mut req);
        let mut cfg = parse_form(&body);
        // Restore credentials from NVS that must not be overwritten here.
        if let Ok(Some(saved)) = config::load_from_nvs() {
            cfg.wifi_ssid     = saved.wifi_ssid;
            cfg.wifi_password = saved.wifi_password;
            // Preserve MQTT password if the user left the field blank.
            if cfg.mqtt_password.is_empty() && !saved.mqtt_password.is_empty() {
                cfg.mqtt_password = saved.mqtt_password;
            }
        }
        let json = match cfg.validate().and_then(|_| config::save_to_nvs(&cfg)) {
            Ok(_) => r#"{"ok":true,"msg":"Gespeichert"}"#.to_string(),
            Err(e) => format!(r#"{{"ok":false,"msg":"{}"}}"#, json_str(&e.to_string())),
        };
        req.into_ok_response()?.write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /sensor ──────────────────────────────────────────────────────────
    {
        let ic = Arc::clone(&i2c_shared);
        server.fn_handler("/sensor", Method::Post, move |mut req| {
            let body = read_body(&mut req);
            let params = parse_kv(&body);
            let sda = params
                .get("sda_pin")
                .and_then(|s| s.parse().ok())
                .unwrap_or(21u8);
            let scl = params
                .get("scl_pin")
                .and_then(|s| s.parse().ok())
                .unwrap_or(22u8);

            let json = read_sensor_json(&ic, sda, scl);
            req.into_ok_response()?.write_all(json.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    // ── Polling-Loop: NVS-Save + Restart wenn /save empfangen ─────────────────
    loop {
        if let Ok(mut guard) = pending.lock()
            && let Some(mut cfg) = guard.take()
        {
            drop(guard); // release before the NVS write
            // Same WiFi-guard as /update: /save path must not overwrite credentials.
            if let Ok(Some(saved)) = config::load_from_nvs() {
                cfg.wifi_ssid     = saved.wifi_ssid;
                cfg.wifi_password = saved.wifi_password;
                if cfg.mqtt_password.is_empty() && !saved.mqtt_password.is_empty() {
                    cfg.mqtt_password = saved.mqtt_password;
                }
            }
            if let Err(e) = cfg.validate() {
                log::error!(target: TAG, "Config ungültig, nicht gespeichert: {e}");
            } else {
                config::save_to_nvs(&cfg).context("NVS write")?;
                log::info!(target: TAG, "Config gespeichert – starte neu…");
                unsafe { esp_idf_sys::esp_restart() };
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---------------------------------------------------------------------------
// WiFi APSTA setup
// ---------------------------------------------------------------------------

fn start_apsta(
    modem: Modem,
    sysloop: EspSystemEventLoop,
    ap_ssid: &heapless::String<32>,
) -> anyhow::Result<BlockingWifi<EspWifi<'static>>> {
    let no_nvs: Option<EspDefaultNvsPartition> = None;
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), no_nvs)?, sysloop)?;

    // Mixed: STA starts idle (no connect() called), AP is immediately active.
    wifi.set_configuration(&Configuration::Mixed(
        ClientConfiguration::default(),
        make_ap_cfg(ap_ssid),
    ))?;
    wifi.start()?;
    Ok(wifi)
}

fn make_ap_cfg(ssid: &heapless::String<32>) -> AccessPointConfiguration {
    AccessPointConfiguration {
        ssid: ssid.clone(),
        ssid_hidden: false,
        channel: 6,
        secondary_channel: None,
        protocols: Default::default(),
        auth_method: AuthMethod::None,
        password: heapless::String::new(),
        max_connections: 4,
    }
}

// ---------------------------------------------------------------------------
// WiFi test
// ---------------------------------------------------------------------------

fn test_wifi_connection(
    wifi_state: &Arc<Mutex<WifiState>>,
    ssid: &str,
    password: &str,
) -> TestResult {
    if ssid.is_empty() {
        return TestResult {
            ok: false,
            msg: "Kein SSID angegeben".into(),
        };
    }
    let ssid_h: heapless::String<32> = match heapless::String::try_from(ssid) {
        Ok(s) => s,
        Err(_) => {
            return TestResult {
                ok: false,
                msg: "SSID zu lang (max 32 Zeichen)".into(),
            };
        }
    };
    let pass_h: heapless::String<64> = match heapless::String::try_from(password) {
        Ok(s) => s,
        Err(_) => {
            return TestResult {
                ok: false,
                msg: "Passwort zu lang (max 64 Zeichen)".into(),
            };
        }
    };

    // try_lock: return immediately if another test is already holding the mutex
    // rather than blocking the HTTP task for up to 15 s.
    let mut guard = match wifi_state.try_lock() {
        Ok(g) => g,
        Err(_) => {
            return TestResult {
                ok: false,
                msg: "Ein Test läuft bereits, bitte warten".into(),
            };
        }
    };

    // Disconnect any existing STA silently before reconfiguring.
    guard.wifi.wifi_mut().disconnect().ok();
    std::thread::sleep(Duration::from_millis(200));

    // Update STA config in-place (APSTA mode stays; AP is unaffected).
    let sta_cfg = ClientConfiguration {
        ssid: ssid_h,
        password: pass_h,
        ..Default::default()
    };
    let ap_cfg = make_ap_cfg(&guard.ap_ssid);
    if let Err(e) = guard
        .wifi
        .set_configuration(&Configuration::Mixed(sta_cfg, ap_cfg))
    {
        return TestResult {
            ok: false,
            msg: format!("Konfigurationsfehler: {e}"),
        };
    }

    // Trigger non-blocking connect, then poll manually.
    if let Err(e) = guard.wifi.wifi_mut().connect() {
        return TestResult {
            ok: false,
            msg: format!("Verbindungsversuch fehlgeschlagen: {e}"),
        };
    }

    let start = Instant::now();
    loop {
        match guard.wifi.is_connected() {
            Ok(true) => {
                // Poll for a valid DHCP address for up to 6 s.
                // is_connected() going true only means layer-2 association
                // is done; the IP lease may still be in flight.
                let dhcp_start = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(300));
                    let ip = guard
                        .wifi
                        .wifi()
                        .sta_netif()
                        .get_ip_info()
                        .ok()
                        .map(|i| i.ip.to_string())
                        .unwrap_or_else(|| "0.0.0.0".into());
                    if ip != "0.0.0.0" {
                        return TestResult {
                            ok: true,
                            msg: format!("Verbunden! IP: {ip}"),
                        };
                    }
                    if dhcp_start.elapsed() > Duration::from_secs(6) {
                        return TestResult {
                            ok: true,
                            msg: "Verbunden – keine DHCP-Antwort (statische IP?)".into(),
                        };
                    }
                }
            }
            Ok(false) => {}
            Err(e) => {
                return TestResult {
                    ok: false,
                    msg: format!("Statusabfrage: {e}"),
                };
            }
        }
        if start.elapsed() > Duration::from_secs(15) {
            return TestResult {
                ok: false,
                msg: "Timeout (15 s) – WLAN nicht erreichbar oder Passwort falsch".into(),
            };
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------
// MQTT test
// ---------------------------------------------------------------------------

fn test_mqtt_connection(
    wifi_state: &Arc<Mutex<WifiState>>,
    server_addr: &str,
    port: u16,
    user: &str,
    password: &str,
) -> TestResult {
    // Brief WiFi check (release lock before doing MQTT).
    // try_lock: avoids blocking if a WiFi test is still running.
    {
        let guard = match wifi_state.try_lock() {
            Ok(g) => g,
            Err(_) => {
                return TestResult {
                    ok: false,
                    msg: "WiFi-Test läuft noch, bitte warten".into(),
                };
            }
        };
        match guard.wifi.is_connected() {
            Ok(true) => {}
            Ok(false) => {
                return TestResult {
                    ok: false,
                    msg: "WLAN nicht verbunden – zuerst WLAN testen".into(),
                };
            }
            Err(e) => {
                return TestResult {
                    ok: false,
                    msg: format!("WLAN-Status: {e}"),
                };
            }
        }
    }

    if server_addr.is_empty() {
        return TestResult {
            ok: false,
            msg: "Kein MQTT-Server angegeben".into(),
        };
    }

    let broker = format!("mqtt://{server_addr}:{port}");
    let conf = MqttClientConfiguration {
        client_id: Some("wnode-test"),
        username: if user.is_empty() { None } else { Some(user) },
        password: if password.is_empty() {
            None
        } else {
            Some(password)
        },
        ..Default::default()
    };

    log::info!(target: TAG, "MQTT-Test → {broker}");
    let (client, mut conn) = match EspMqttClient::new(&broker, &conf) {
        Ok(c) => c,
        Err(e) => {
            return TestResult {
                ok: false,
                msg: format!("Client-Erstellung: {e}"),
            };
        }
    };

    let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
    let conn_thread = std::thread::Builder::new()
        .stack_size(6_144)
        .spawn(move || {
            loop {
                match conn.next() {
                    Ok(ev) => match ev.payload() {
                        EventPayload::Connected(_) => {
                            tx.try_send(true).ok();
                            break;
                        }
                        EventPayload::Disconnected => {
                            tx.try_send(false).ok();
                            break;
                        }
                        EventPayload::Error(_) => {
                            tx.try_send(false).ok();
                            break;
                        }
                        _ => {}
                    },
                    Err(_) => {
                        tx.try_send(false).ok();
                        break;
                    }
                }
            }
        });

    let conn_thread = match conn_thread {
        Ok(t) => t,
        Err(e) => {
            drop(client);
            return TestResult {
                ok: false,
                msg: format!("Thread-Fehler: {e}"),
            };
        }
    };

    let result = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(true) => TestResult {
            ok: true,
            msg: "MQTT-Verbindung erfolgreich!".into(),
        },
        Ok(false) => TestResult {
            ok: false,
            msg: "Broker hat Verbindung abgelehnt".into(),
        },
        Err(_) => TestResult {
            ok: false,
            msg: "Timeout (10 s) – Broker nicht erreichbar".into(),
        },
    };

    drop(client);
    conn_thread.join().ok();
    result
}

// ---------------------------------------------------------------------------
// Sensor test (BME280 only; battery ADC needs concrete pin types unavailable here)
// ---------------------------------------------------------------------------

fn read_sensor_json(i2c_mtx: &Arc<Mutex<I2C0>>, sda_pin: u8, scl_pin: u8) -> String {
    // GPIO 34–39 are input-only on the ESP32 and cannot drive I²C lines.
    if sda_pin > 33 || scl_pin > 33 {
        return r#"{"ok":false,"msg":"Ungültige Pin-Nummern (SDA/SCL max GPIO 33)"}"#.into();
    }

    // try_lock: return an error immediately instead of blocking if another
    // /sensor request is still running (I²C operation ~20 ms).
    let mut guard = match i2c_mtx.try_lock() {
        Ok(g) => g,
        Err(_) => return r#"{"ok":false,"msg":"Sensor wird bereits verwendet"}"#.into(),
    };

    // SAFETY: guard is held for the entire I²C operation.  Releasing it early
    // would allow a concurrent request to clone_unchecked and initialise a
    // second I2cDriver on the same peripheral simultaneously.
    let i2c0 = unsafe { guard.clone_unchecked() };
    let sda = unsafe { AnyIOPin::new(sda_pin as i32) };
    let scl = unsafe { AnyIOPin::new(scl_pin as i32) };

    let bme_cfg = sensor::Bme280Config {
        addr: 0x76,
        send_temperature: true,
        send_pressure: true,
        send_humidity: true,
    };

    let json = match sensor::read_bme280(&bme_cfg, i2c0, sda, scl) {
        Ok((temp, pres, humi)) => {
            let t = temp
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "n/a".into());
            let p = pres
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "n/a".into());
            let h = humi
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "n/a".into());
            format!(r#"{{"ok":true,"temp":"{t}","pres":"{p}","humi":"{h}"}}"#)
        }
        Err(e) => {
            let msg = e.to_string().replace('"', "'");
            format!(r#"{{"ok":false,"msg":"{msg}"}}"#)
        }
    };
    drop(guard); // explicit: I²C complete, peripheral released
    json
}

// ---------------------------------------------------------------------------
// Battery ADC test (portal)
// ---------------------------------------------------------------------------

fn read_bat_json(adc_mtx: &Arc<Mutex<ADC1>>, pin: u8) -> String {
    if !(32..=39).contains(&pin) {
        return r#"{"ok":false,"msg":"Ungültiger ADC-Pin (gültig: 32–39)"}"#.into();
    }

    let mut guard = match adc_mtx.try_lock() {
        Ok(g) => g,
        Err(_) => return r#"{"ok":false,"msg":"ADC wird bereits verwendet"}"#.into(),
    };

    use esp_idf_hal::adc::{
        attenuation::DB_11,
        oneshot::{
            config::{AdcChannelConfig, Calibration},
            AdcChannelDriver, AdcDriver,
        },
    };
    use esp_idf_hal::gpio::{
        Gpio32, Gpio33, Gpio34, Gpio35, Gpio36, Gpio37, Gpio38, Gpio39,
    };

    let result: anyhow::Result<u16> = (|| {
        // SAFETY: clone_unchecked gives a second ADC1 token for this driver scope.
        // GPIO types are ZSTs; measurement mode cannot run concurrently with portal.
        let adc = AdcDriver::new(unsafe { guard.clone_unchecked() })?;
        let ch_cfg = AdcChannelConfig {
            attenuation: DB_11,
            calibration: Calibration::None,
            ..Default::default()
        };
        let raw: u16 = match pin {
            32 => AdcChannelDriver::new(&adc, unsafe { std::mem::zeroed::<Gpio32>() }, &ch_cfg)?.read()?,
            33 => AdcChannelDriver::new(&adc, unsafe { std::mem::zeroed::<Gpio33>() }, &ch_cfg)?.read()?,
            34 => AdcChannelDriver::new(&adc, unsafe { std::mem::zeroed::<Gpio34>() }, &ch_cfg)?.read()?,
            35 => AdcChannelDriver::new(&adc, unsafe { std::mem::zeroed::<Gpio35>() }, &ch_cfg)?.read()?,
            36 => AdcChannelDriver::new(&adc, unsafe { std::mem::zeroed::<Gpio36>() }, &ch_cfg)?.read()?,
            37 => AdcChannelDriver::new(&adc, unsafe { std::mem::zeroed::<Gpio37>() }, &ch_cfg)?.read()?,
            38 => AdcChannelDriver::new(&adc, unsafe { std::mem::zeroed::<Gpio38>() }, &ch_cfg)?.read()?,
            39 => AdcChannelDriver::new(&adc, unsafe { std::mem::zeroed::<Gpio39>() }, &ch_cfg)?.read()?,
            _ => unreachable!(),
        };
        Ok(raw)
    })();

    drop(guard);

    match result {
        Ok(raw) => {
            let v = sensor::bat_voltage_from_raw(raw);
            format!(r#"{{"ok":true,"msg":"{:.2} V (raw {})"}}"#, v, raw)
        }
        Err(e) => format!(r#"{{"ok":false,"msg":"{}"}}"#, json_str(&e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Chip-ID
// ---------------------------------------------------------------------------

fn get_chip_id() -> String {
    let mut mac = [0u8; 6];
    unsafe { esp_idf_sys::esp_efuse_mac_get_default(mac.as_mut_ptr()) };
    format!("{:02x}{:02x}{:02x}", mac[3], mac[4], mac[5])
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Escapes a string for safe embedding inside a JSON string literal.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '"'  => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c    => out.push(c),
        }
    }
    out
}

/// Escapes a string for safe embedding inside HTML text content.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Builds an HTML error page for /save validation failures.
fn format_save_error(msg: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Fehler</title>
<style>
body{{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;background:#eef2f7;padding:1.2rem}}
.card{{background:#fff;border-radius:12px;box-shadow:0 2px 16px rgba(0,0,0,.1);
       max-width:480px;margin:2rem auto;padding:2rem;text-align:center}}
.icon{{font-size:2.8rem;color:#c0392b;margin-bottom:.6rem}}
h2{{color:#c0392b;font-size:1.1rem;margin-bottom:.7rem}}
p{{color:#555;line-height:1.6;font-size:.92rem;word-break:break-word}}
a{{display:inline-block;margin-top:1rem;padding:.4rem 1.2rem;background:#1558a8;
   color:#fff;text-decoration:none;border-radius:6px;font-size:.9rem}}
</style></head>
<body><div class="card">
<div class="icon">&#9888;</div>
<h2>Ung&uuml;ltige Konfiguration</h2>
<p>{}</p>
<a href="/">&#8592; Zur&uuml;ck</a>
</div></body></html>"#,
        html_escape(msg)
    )
}

fn read_body(req: &mut impl embedded_svc::io::Read) -> String {
    // 4 KB cap: the config form is at most ~600 bytes; this prevents heap
    // exhaustion from oversized or malicious POST bodies.
    const MAX_BODY: usize = 4096;
    let mut buf = [0u8; 512];
    let mut out = Vec::with_capacity(512);
    loop {
        let rem = MAX_BODY.saturating_sub(out.len());
        if rem == 0 {
            break;
        }
        let to_read = buf.len().min(rem);
        match req.read(&mut buf[..to_read]) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_kv(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(url_decode(k), url_decode(v));
        }
    }
    map
}

fn parse_form(body: &str) -> Config {
    let map = parse_kv(body);
    let get = |k: &str| map.get(k).cloned().unwrap_or_default();
    let checked = |k: &str| map.contains_key(k);
    Config {
        device_name: {
            let v = get("device_name");
            if v.is_empty() {
                "weathernode".into()
            } else {
                v
            }
        },
        room: get("room"),
        wifi_ssid: get("wifi_ssid"),
        wifi_password: get("wifi_password"),
        mqtt_server: get("mqtt_server"),
        mqtt_port: get("mqtt_port").parse().unwrap_or(1883),
        mqtt_user: get("mqtt_user"),
        mqtt_password: get("mqtt_password"),
        mqtt_topic: {
            let v = get("mqtt_topic");
            if v.is_empty() {
                "/data/nodes".into()
            } else {
                v
            }
        },
        sleep_minutes: get("sleep_minutes").parse().unwrap_or(5),
        mqtt_qos: get("mqtt_qos").parse::<u8>().unwrap_or(0).min(1),
        bme280_addr: 0x76,
        sda_pin: get("sda_pin").parse().unwrap_or(21),
        scl_pin: get("scl_pin").parse().unwrap_or(22),
        adc_pin: get("adc_pin").parse().unwrap_or(33),
        send_temperature: checked("send_temperature"),
        send_pressure: checked("send_pressure"),
        send_humidity: checked("send_humidity"),
        send_battery: checked("send_battery"),
    }
}

fn url_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let b = s.as_bytes();
    // Collect into bytes first so multi-byte UTF-8 sequences (e.g. Umlaute
    // in passwords) are decoded correctly instead of char-by-char.
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            // Both hex digits present: try to decode.
            if let Ok(s2) = std::str::from_utf8(&b[i + 1..i + 3])
                && let Ok(byte) = u8::from_str_radix(s2, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            // Invalid hex digits or non-ASCII: keep the '%' literally so the
            // caller sees the raw sequence rather than a silently mangled string.
            out.push(b'%');
            i += 1;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Static HTML
// ---------------------------------------------------------------------------

const HTML_FORM: &str = r#"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>WeatherNode Setup</title>
<style>
  *{box-sizing:border-box;margin:0;padding:0}
  body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;
       background:#eef2f7;min-height:100vh;padding:1rem}
  h1{color:#1558a8;font-size:1.05rem;text-align:center;
     font-weight:700;margin-bottom:.9rem;letter-spacing:.02em}
  .card{background:#fff;border-radius:12px;
        box-shadow:0 2px 12px rgba(0,0,0,.08);
        max-width:480px;margin:0 auto .9rem;padding:1.2rem 1.3rem}
  .card h2{color:#1558a8;font-size:.92rem;font-weight:700;
           margin-bottom:.85rem;padding-bottom:.5rem;
           border-bottom:2px solid #e2eaf6}
  label{display:block;margin:.65rem 0 .2rem;font-size:.81rem;font-weight:600;color:#444}
  label.dim{color:#bbb}
  input:not([type=checkbox]){
    width:100%;padding:.44rem .6rem;font-size:.92rem;color:#222;
    border:1px solid #c8d0de;border-radius:6px;background:#fafbfc;
    transition:border-color .18s,box-shadow .18s}
  input:not([type=checkbox]):focus{
    outline:none;border-color:#1558a8;background:#fff;
    box-shadow:0 0 0 3px rgba(21,88,168,.13)}
  input:disabled{background:#f2f2f2;color:#bbb;border-color:#e0e0e0;cursor:not-allowed}
  .pw{position:relative}
  .pw input{padding-right:2.4rem}
  .eye{position:absolute;right:.4rem;top:50%;transform:translateY(-50%);
       background:none;border:none;cursor:pointer;font-size:.95rem;
       padding:.15rem .25rem;opacity:.45;transition:opacity .15s;line-height:1}
  .eye:hover{opacity:.9}
  fieldset{border:1px solid #dde3ec;border-radius:8px;
           margin-top:.85rem;padding:.6rem .85rem .8rem}
  legend{font-size:.8rem;font-weight:700;color:#1558a8;padding:0 .35rem}
  .cr{display:flex;align-items:center;gap:.5rem;margin:.4rem 0}
  .cr input{width:15px;height:15px;cursor:pointer;accent-color:#1558a8;flex-shrink:0}
  .cr label{margin:0;font-size:.87rem;font-weight:400;color:#333}
  .note{font-size:.76rem;color:#999;margin-top:.45rem;line-height:1.5}
  .br{display:flex;align-items:center;gap:.45rem;margin-top:.85rem;flex-wrap:wrap}
  .dbtn{padding:.32rem .75rem;font-size:.78rem;cursor:pointer;white-space:nowrap;
        color:#555;background:#f2f2f2;border:1px solid #ccc;border-radius:5px;
        transition:background .15s}
  .dbtn:hover{background:#e4e4e4}
  .tbtn{padding:.32rem .75rem;font-size:.78rem;cursor:pointer;white-space:nowrap;
        color:#1558a8;background:#eef2f7;border:1px solid #b3c4de;border-radius:5px;
        transition:background .15s}
  .tbtn:hover:not(:disabled){background:#dce6f5;border-color:#1558a8}
  .tbtn:disabled{opacity:.38;cursor:default}
  .save{padding:.35rem .85rem;font-size:.82rem;font-weight:600;cursor:pointer;
        color:#fff;background:#1558a8;border:none;border-radius:5px;
        transition:background .15s;margin-left:auto;white-space:nowrap}
  .save:hover{background:#114a90}
  .st{font-size:.79rem;word-break:break-word;color:#888;width:100%;margin-top:.1rem}
  .st.busy{color:#9a5f00}
  .st.ok{color:#1c7a42;font-weight:600}
  .st.fail{color:#c0392b;font-weight:600}
  .confirm-bar{display:none;margin-top:.7rem;padding:.55rem .9rem;background:#e8f5e9;
               border:1px solid #4caf50;border-radius:7px;font-size:.82rem;color:#1c7a42;
               align-items:center;gap:.6rem;flex-wrap:wrap}
  .confirm-bar .cbtn{padding:.3rem .8rem;font-size:.8rem;font-weight:600;cursor:pointer;
                     color:#fff;background:#1c7a42;border:none;border-radius:5px;
                     margin-left:auto;white-space:nowrap;transition:background .15s}
  .confirm-bar .cbtn:hover{background:#155c30}
</style>
<script>
async function ct(url,fields,sid,bid,cid){
  const btn=document.getElementById(bid),st=document.getElementById(sid);
  if(cid){const c=document.getElementById(cid);if(c)c.style.display='none';}
  btn.disabled=true;st.textContent='Bitte warten\u2026';st.className='st busy';
  try{
    const b=new URLSearchParams();
    fields.forEach(n=>{const e=document.querySelector('[name='+n+']');if(e)b.append(n,e.value)});
    const r=await fetch(url,{method:'POST',body:b}),j=await r.json();
    st.textContent=j.temp!==undefined
      ?(j.temp+' \u00b0C \u00b7 '+j.pres+' hPa \u00b7 '+j.humi+' %')
      :(j.msg||(j.ok?'OK':'Fehler'));
    st.className='st '+(j.ok?'ok':'fail');
    if(j.ok&&cid){const c=document.getElementById(cid);if(c)c.style.display='flex';}
  }catch(e){st.textContent='Netzwerkfehler';st.className='st fail';}
  btn.disabled=false;
}
function testWifi(){ct('/test_wifi',['wifi_ssid','wifi_password'],'st-wifi','btn-wifi','confirm-wifi');}
function testMqtt(){ct('/test_mqtt',['mqtt_server','mqtt_port','mqtt_user','mqtt_password'],'st-mqtt','btn-mqtt','confirm-mqtt');}
function testSensor(){ct('/sensor',['sda_pin','scl_pin'],'st-sensor','btn-sensor');}
function testBat(){ct('/test_bat',['adc_pin'],'st-bat','btn-bat');}
async function saveWifi(){
  document.getElementById('confirm-wifi').style.display='none';
  const btn=document.getElementById('sv-wifi'),st=document.getElementById('st-wifi');
  btn.disabled=true;st.textContent='Verbindung wird getestet…';st.className='st busy';
  try{
    const b=new URLSearchParams();
    b.append('wifi_ssid',document.querySelector('[name=wifi_ssid]').value);
    b.append('wifi_password',document.getElementById('pw1').value);
    const r=await fetch('/save_wifi',{method:'POST',body:b}),j=await r.json();
    st.textContent=j.msg||(j.ok?'Gespeichert':'Fehler');
    st.className='st '+(j.ok?'ok':'fail');
    if(j.ok){
      document.getElementById('pw1').value='';
      document.getElementById('pw1').placeholder='• gespeichert – leer lassen zum Beibehalten';
      document.getElementById('mqtt-warning').style.display='none';
    }
  }catch(e){st.textContent='Netzwerkfehler';st.className='st fail';}
  btn.disabled=false;
}
window.addEventListener('load',async()=>{
  try{
    const j=await fetch('/config').then(r=>r.json());
    if(!j.configured)return;
    ['device_name','room','wifi_ssid','mqtt_server','mqtt_port','mqtt_user',
     'mqtt_topic','sleep_minutes','sda_pin','scl_pin','adc_pin'].forEach(n=>{
      const e=document.querySelector('[name='+n+']');
      if(e&&j[n]!==undefined)e.value=j[n];
    });
    const qs=document.querySelector('[name=mqtt_qos]');
    if(qs&&j.mqtt_qos!==undefined)qs.value=j.mqtt_qos;
    ['send_temperature','send_pressure','send_humidity','send_battery'].forEach(n=>{
      const e=document.querySelector('[name='+n+']');
      if(e&&j[n]!==undefined)e.checked=j[n];
    });
    if(j.wifi_password_saved)
      document.getElementById('pw1').placeholder='• gespeichert – leer lassen zum Beibehalten';
    if(j.mqtt_password_saved)
      document.getElementById('pw2').placeholder='• gespeichert – leer lassen zum Beibehalten';
    if(j.setup_state==='wifi_only')
      document.getElementById('mqtt-warning').style.display='block';
    syncBat();
  }catch(e){}
});
function togglePw(id){const e=document.getElementById(id);e.type=e.type==='password'?'text':'password';}
function setDef(fields){
  fields.forEach(([n,v])=>{
    const e=document.querySelector('[name='+n+']');
    if(!e)return;
    e.type==='checkbox'?e.checked=v:e.value=v;
  });
  syncBat();
}
function syncBat(){
  const on=document.querySelector('[name=send_battery]').checked;
  document.getElementById('adc-pin').disabled=!on;
  document.getElementById('lbl-adc').className=on?'':'dim';
  document.getElementById('btn-bat').disabled=!on;
}
async function save(bid,sid){
  const btn=document.getElementById(bid),st=document.getElementById(sid);
  btn.disabled=true;st.textContent='Speichern\u2026';st.className='st busy';
  try{
    const b=new URLSearchParams(new FormData(document.getElementById('frm')));
    const r=await fetch('/update',{method:'POST',body:b}),j=await r.json();
    st.textContent=j.msg||(j.ok?'Gespeichert':'Fehler');
    st.className='st '+(j.ok?'ok':'fail');
  }catch(e){st.textContent='Netzwerkfehler';st.className='st fail';}
  btn.disabled=false;
}
</script>
</head>
<body>
<h1>&#9729; WeatherNode Konfiguration</h1>
<div id="mqtt-warning" style="display:none;background:#fff3cd;border:2px solid #e67e22;border-radius:12px;max-width:480px;margin:0 auto .9rem;padding:1rem 1.3rem">
  <strong style="color:#c0392b">&#9888; MQTT nicht konfiguriert</strong>
  <p style="font-size:.85rem;color:#555;margin-top:.35rem">Das Ger&auml;t verbindet sich mit dem WLAN, sendet aber keine Messwerte. Bitte MQTT-Server eintragen und speichern, dann Ger&auml;t neu starten.</p>
</div>
<form method="POST" action="/save" id="frm">

<div class="card">
  <h2>&#9881; Ger&auml;t</h2>
  <label>Ger&auml;tename</label>
  <input name="device_name" value="weathernode" required>
  <label>Raum <span style="font-weight:400;color:#aaa;font-size:.76rem">(optional &ndash; erscheint im MQTT-Payload)</span></label>
  <input name="room" placeholder="z.B. keller">
  <div class="br">
    <button type="button" class="dbtn" onclick="setDef([['device_name','weathernode'],['room','']])">Standard</button>
    <button type="button" id="sv-dev" class="save" onclick="save('sv-dev','st-dev')">Speichern</button>
  </div>
  <span id="st-dev" class="st"></span>
</div>

<div class="card">
  <h2>&#128246; WLAN</h2>
  <label>SSID</label>
  <input name="wifi_ssid" autocomplete="off">
  <label>Passwort</label>
  <div class="pw">
    <input type="password" id="pw1" name="wifi_password" autocomplete="off">
    <button type="button" class="eye" onclick="togglePw('pw1')" title="Passwort anzeigen">&#128065;</button>
  </div>
  <p class="note" style="margin-top:.5rem">Leer lassen um gespeichertes Passwort zu &uuml;bernehmen. WLAN wird vor dem Speichern getestet.</p>
  <div class="br">
    <button type="button" class="dbtn" onclick="setDef([['wifi_ssid',''],['wifi_password','']])">Standard</button>
    <button type="button" id="btn-wifi" class="tbtn" onclick="testWifi()">Verbindung testen</button>
  </div>
  <span id="st-wifi" class="st"></span>
  <div id="confirm-wifi" class="confirm-bar">
    <span>&#10003; Verbindung ok &ndash; Einstellungen jetzt speichern?</span>
    <button type="button" id="sv-wifi" class="cbtn" onclick="saveWifi()">Speichern &amp; weiter</button>
  </div>
</div>

<div class="card">
  <h2>&#128241; MQTT</h2>
  <label>Server</label>
  <input name="mqtt_server" placeholder="192.168.1.x">
  <label>Port</label>
  <input name="mqtt_port" type="number" value="1883" min="1" max="65535">
  <label>Benutzer</label>
  <input name="mqtt_user" autocomplete="off">
  <label>Passwort</label>
  <div class="pw">
    <input type="password" id="pw2" name="mqtt_password" autocomplete="off">
    <button type="button" class="eye" onclick="togglePw('pw2')" title="Passwort anzeigen">&#128065;</button>
  </div>
  <label>Topic</label>
  <input name="mqtt_topic" value="/data/nodes">
  <label>Sleep-Intervall (Minuten)</label>
  <input name="sleep_minutes" type="number" value="5" min="1" max="60">
  <label>MQTT QoS</label>
  <select name="mqtt_qos" style="width:100%;padding:.44rem .6rem;font-size:.92rem;color:#222;border:1px solid #c8d0de;border-radius:6px;background:#fafbfc">
    <option value="0" selected>0 &ndash; AtMostOnce (empfohlen, kein ACK)</option>
    <option value="1">1 &ndash; AtLeastOnce (ACK, l&auml;ngere Wachzeit)</option>
  </select>
  <div class="br">
    <button type="button" class="dbtn" onclick="setDef([['mqtt_server',''],['mqtt_port','1883'],['mqtt_user',''],['mqtt_password',''],['mqtt_topic','/data/nodes'],['sleep_minutes','5'],['mqtt_qos','0']])">Standard</button>
    <button type="button" id="btn-mqtt" class="tbtn" onclick="testMqtt()">Verbindung testen</button>
    <button type="button" id="sv-mqtt" class="save" onclick="save('sv-mqtt','st-mqtt-s')">Speichern</button>
  </div>
  <span id="st-mqtt" class="st"></span>
  <div id="confirm-mqtt" class="confirm-bar">
    <span>&#10003; Broker erreichbar &ndash; MQTT-Einstellungen jetzt speichern?</span>
    <button type="button" class="cbtn" onclick="this.closest('.confirm-bar').style.display='none';save('sv-mqtt','st-mqtt-s')">Speichern &amp; weiter</button>
  </div>
  <span id="st-mqtt-s" class="st"></span>
</div>

<div class="card">
  <h2>&#127777; Sensor / Hardware</h2>
  <label>I&sup2;C SDA (GPIO 0&ndash;33)</label>
  <input name="sda_pin" type="number" value="21" min="0" max="33">
  <label>I&sup2;C SCL (GPIO 0&ndash;33)</label>
  <input name="scl_pin" type="number" value="22" min="0" max="33">
  <fieldset>
    <legend>Messwerte senden</legend>
    <div class="cr"><input type="checkbox" id="ct" name="send_temperature" value="1" checked><label for="ct">Temperatur</label></div>
    <div class="cr"><input type="checkbox" id="cp" name="send_pressure"    value="1" checked><label for="cp">Luftdruck</label></div>
    <div class="cr"><input type="checkbox" id="ch" name="send_humidity"    value="1" checked><label for="ch">Luftfeuchtigkeit</label></div>
  </fieldset>
  <div class="br">
    <button type="button" class="dbtn" onclick="setDef([['sda_pin','21'],['scl_pin','22'],['send_temperature',true],['send_pressure',true],['send_humidity',true]])">Standard</button>
    <button type="button" id="btn-sensor" class="tbtn" onclick="testSensor()">Sensor lesen</button>
    <button type="button" id="sv-sens" class="save" onclick="save('sv-sens','st-sens-s')">Speichern</button>
  </div>
  <span id="st-sensor" class="st"></span>
  <span id="st-sens-s" class="st"></span>
</div>

<div class="card">
  <h2>&#128267; Batterie</h2>
  <div class="cr">
    <input type="checkbox" id="cb" name="send_battery" value="1" onchange="syncBat()">
    <label for="cb">Batteriespannung messen und senden</label>
  </div>
  <p class="note">Steckdosen-Node: deaktiviert lassen &ndash; ADC wird dann nicht initialisiert.<br>Batterie-Node: aktivieren und ADC-Pin w&auml;hlen.</p>
  <label id="lbl-adc" class="dim">ADC-Pin (GPIO 32&ndash;39)</label>
  <input id="adc-pin" name="adc_pin" type="number" value="33" min="32" max="39" disabled>
  <div class="br">
    <button type="button" class="dbtn" onclick="setDef([['send_battery',false],['adc_pin','33']])">Standard</button>
    <button type="button" id="btn-bat" class="tbtn" onclick="testBat()" disabled>ADC messen</button>
    <button type="button" id="sv-bat" class="save" onclick="save('sv-bat','st-bat-s')">Speichern</button>
  </div>
  <span id="st-bat" class="st"></span>
  <span id="st-bat-s" class="st"></span>
</div>

<div class="card" style="text-align:center">
  <p class="note" style="margin-bottom:.8rem">Alle &Auml;nderungen gespeichert? Dann Ger&auml;t neu starten:</p>
  <button type="submit" style="padding:.55rem 2rem;font-size:.95rem;font-weight:700;cursor:pointer;color:#fff;background:#1558a8;border:none;border-radius:8px;transition:background .15s" onmouseover="this.style.background='#114a90'" onmouseout="this.style.background='#1558a8'">Speichern &amp; Neustart</button>
</div>

</form>
<script>syncBat();</script>
</body></html>"#;

const HTML_SAVED: &str = r#"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Gespeichert</title>
<style>
  body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;
       background:#eef2f7;min-height:100vh;padding:1.2rem}
  .card{background:#fff;border-radius:12px;
        box-shadow:0 2px 16px rgba(0,0,0,.1);
        max-width:480px;margin:2rem auto;padding:2rem;text-align:center}
  .icon{font-size:2.8rem;color:#1c7a42;margin-bottom:.6rem}
  h2{color:#1c7a42;font-size:1.2rem;margin-bottom:.7rem}
  p{color:#555;line-height:1.6;font-size:.95rem}
</style>
</head>
<body>
<div class="card">
  <div class="icon">&#10003;</div>
  <h2>Konfiguration gespeichert</h2>
  <p>Das Ger&auml;t startet jetzt neu und verbindet sich mit dem konfigurierten WLAN.</p>
</div>
</body></html>"#;

// ---------------------------------------------------------------------------
// Tests (pure logic, no hardware)
// Run with: cargo test --target <host-target>  (requires host-compatible build)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn url_decode_plain() {
        assert_eq!(url_decode("hello"), "hello");
    }

    #[test]
    fn url_decode_plus_as_space() {
        assert_eq!(url_decode("hello+world"), "hello world");
    }

    #[test]
    fn url_decode_ascii_percent() {
        assert_eq!(url_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn url_decode_umlaut_password() {
        // ä = U+00E4 = UTF-8 0xC3 0xA4
        assert_eq!(url_decode("P%C3%A4ssword"), "Pässword");
    }

    #[test]
    fn parse_kv_basic() {
        let m = parse_kv("a=1&b=2");
        assert_eq!(m.get("a").map(String::as_str), Some("1"));
        assert_eq!(m.get("b").map(String::as_str), Some("2"));
    }

    #[test]
    fn parse_kv_missing_value() {
        let m = parse_kv("key=");
        assert_eq!(m.get("key").map(String::as_str), Some(""));
    }

    #[test]
    fn parse_kv_encoded_value() {
        let m = parse_kv("pw=My%20Pass");
        assert_eq!(m.get("pw").map(String::as_str), Some("My Pass"));
    }

    #[test]
    fn url_decode_invalid_hex_kept() {
        // %GG is not valid hex; the '%' should be preserved literally.
        assert_eq!(url_decode("%GGabc"), "%GGabc");
    }

    #[test]
    fn url_decode_truncated_at_end() {
        // Bare '%' at end (no following digits) should not be eaten.
        assert_eq!(url_decode("abc%"), "abc%");
    }

    #[test]
    fn url_decode_truncated_one_digit() {
        // '%A' with no second digit: keep '%' literally (no phantom byte).
        assert_eq!(url_decode("abc%A"), "abc%A");
    }

    #[test]
    fn json_str_escapes_special() {
        assert_eq!(json_str(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn json_str_escapes_newline() {
        assert_eq!(json_str("a\nb"), r"a\nb");
    }

    #[test]
    fn html_escape_entities() {
        assert_eq!(html_escape("<b>&\"x\"</b>"), "&lt;b>&amp;\"x\"&lt;/b>");
    }
}
