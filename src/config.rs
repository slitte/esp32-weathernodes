//! Runtime configuration and NVS persistence.

use std::{fmt, sync::Mutex};

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

// ---------------------------------------------------------------------------
// Config struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub device_name: String,
    pub room: String,
    pub wifi_ssid: String,
    pub wifi_password: String,
    pub mqtt_server: String,
    pub mqtt_port: u16,
    pub mqtt_user: String,
    pub mqtt_password: String,
    pub mqtt_topic: String,
    pub sleep_minutes: u32,
    /// MQTT QoS level: 0 = AtMostOnce (default, no ACK), 1 = AtLeastOnce (PUBACK).
    pub mqtt_qos: u8,
    /// I²C address of the BME280 (Arduino: 0x76).
    pub bme280_addr: u8,
    /// I²C SDA pin (Arduino: GPIO 21).
    pub sda_pin: u8,
    /// I²C SCL pin (Arduino: GPIO 22).
    pub scl_pin: u8,
    /// ADC pin for battery voltage (Arduino: GPIO 33, ADC1 ch5).
    pub adc_pin: u8,
    // ── per-sensor enable flags ──
    pub send_temperature: bool,
    pub send_pressure: bool,
    pub send_humidity: bool,
    pub send_battery: bool,
}

// ---------------------------------------------------------------------------
// Setup state
// ---------------------------------------------------------------------------

/// Describes how complete the device configuration is.
/// Determines which mode main() enters on boot.
#[derive(Debug, PartialEq)]
pub enum SetupState {
    /// wifi_ssid absent or empty – portal required.
    Incomplete,
    /// wifi_ssid set, but mqtt_server empty – portal with warning.
    WifiOnly,
    /// Both WiFi and MQTT configured – measurement mode.
    Complete,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            device_name: "weathernode".into(),
            room: String::new(),
            wifi_ssid: String::new(),
            wifi_password: String::new(),
            mqtt_server: String::new(),
            mqtt_port: 1883,
            mqtt_user: String::new(),
            mqtt_password: String::new(),
            mqtt_topic: "/data/nodes".into(),
            sleep_minutes: 5,
            mqtt_qos: 0,
            bme280_addr: 0x76,
            sda_pin: 21,
            scl_pin: 22,
            adc_pin: 33,
            send_temperature: true,
            send_pressure: true,
            send_humidity: true,
            send_battery: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ConfigError {
    Storage(String),
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Storage(m) => write!(f, "NVS error: {m}"),
            ConfigError::Validation(m) => write!(f, "invalid config: {m}"),
        }
    }
}

// anyhow's blanket impl covers `From<E: std::error::Error>` automatically.
impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

impl Config {
    /// Validates hardware pin assignments.
    /// Must be called once after loading config, before creating any peripheral drivers.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sda_pin > 33 {
            return Err(ConfigError::Validation(format!(
                "SDA GPIO{} is input-only on ESP32 (valid: 0–33)",
                self.sda_pin
            )));
        }
        if self.scl_pin > 33 {
            return Err(ConfigError::Validation(format!(
                "SCL GPIO{} is input-only on ESP32 (valid: 0–33)",
                self.scl_pin
            )));
        }
        if self.sda_pin == self.scl_pin {
            return Err(ConfigError::Validation(format!(
                "SDA and SCL must be different pins (both GPIO{})",
                self.sda_pin
            )));
        }
        if self.send_battery {
            if !(32..=39).contains(&self.adc_pin) {
                return Err(ConfigError::Validation(format!(
                    "ADC GPIO{} is not an ADC1 channel (valid: 32–39)",
                    self.adc_pin
                )));
            }
            if self.adc_pin == self.sda_pin || self.adc_pin == self.scl_pin {
                return Err(ConfigError::Validation(format!(
                    "ADC GPIO{} conflicts with I²C pins (SDA={} SCL={})",
                    self.adc_pin, self.sda_pin, self.scl_pin
                )));
            }
        }
        Ok(())
    }

    pub fn setup_state(&self) -> SetupState {
        if self.wifi_ssid.is_empty() {
            SetupState::Incomplete
        } else if self.mqtt_server.is_empty() {
            SetupState::WifiOnly
        } else {
            SetupState::Complete
        }
    }
}

// ---------------------------------------------------------------------------
// NVS key names (ESP-IDF limit: 15 chars each)
// ---------------------------------------------------------------------------

const NVS_NS: &str = "wnode_cfg";
const K_DEVICE_NAME: &str = "device_name"; // 11
const K_ROOM: &str = "room"; //  4
const K_SDA_PIN: &str = "sda_pin"; //  7
const K_SCL_PIN: &str = "scl_pin"; //  7
const K_ADC_PIN: &str = "adc_pin"; //  7
const K_WIFI_SSID: &str = "wifi_ssid"; //  9
const K_WIFI_PASS: &str = "wifi_password"; // 13
const K_MQTT_SERVER: &str = "mqtt_server"; // 11
const K_MQTT_PORT: &str = "mqtt_port"; //  9
const K_MQTT_USER: &str = "mqtt_user"; //  9
const K_MQTT_PASS: &str = "mqtt_password"; // 13
const K_MQTT_TOPIC: &str = "mqtt_topic"; // 10
const K_SLEEP_MINS: &str = "sleep_mins"; // 10
const K_MQTT_QOS:   &str = "mqtt_qos";  //  8
const K_SEND_TEMP:  &str = "send_temp"; //  9
const K_SEND_PRES: &str = "send_pres"; //  9
const K_SEND_HUMI: &str = "send_humi"; //  9
const K_SEND_BAT: &str = "send_bat"; //  8

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// Serialises concurrent save_to_nvs() calls.  EspDefaultNvsPartition::take()
// fails if a second handle is requested while the first is still alive, so only
// one EspNvs instance may exist at a time.
static NVS_LOCK: Mutex<()> = Mutex::new(());

fn open_nvs() -> Result<EspNvs<NvsDefault>, ConfigError> {
    let part = EspDefaultNvsPartition::take().map_err(|e| ConfigError::Storage(e.to_string()))?;
    // rw=true: creates the namespace if absent, needed for first-boot detection.
    EspNvs::new(part, NVS_NS, true).map_err(|e| ConfigError::Storage(e.to_string()))
}

fn get_str(nvs: &EspNvs<NvsDefault>, key: &str) -> Result<Option<String>, ConfigError> {
    let mut buf = [0u8; 256];
    nvs.get_str(key, &mut buf)
        .map(|opt| opt.map(str::to_string))
        .map_err(|e| ConfigError::Storage(e.to_string()))
}

/// Missing key → `true` (all sensors on by default).
fn nvs_bool(nvs: &EspNvs<NvsDefault>, key: &str) -> Result<bool, ConfigError> {
    Ok(get_str(nvs, key)?.is_none_or(|s| s != "0"))
}

fn set_str(nvs: &mut EspNvs<NvsDefault>, key: &str, val: &str) -> Result<(), ConfigError> {
    nvs.set_str(key, val)
        .map(|_| ())
        .map_err(|e| ConfigError::Storage(e.to_string()))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load config from NVS.  Returns `Ok(None)` on first boot (no wifi_ssid stored).
pub fn load_from_nvs() -> Result<Option<Config>, ConfigError> {
    let nvs = open_nvs()?;

    let wifi_ssid = match get_str(&nvs, K_WIFI_SSID)? {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };

    Ok(Some(Config {
        device_name: get_str(&nvs, K_DEVICE_NAME)?.unwrap_or_else(|| "weathernode".into()),
        room: get_str(&nvs, K_ROOM)?.unwrap_or_default(),
        wifi_ssid,
        wifi_password: get_str(&nvs, K_WIFI_PASS)?.unwrap_or_default(),
        mqtt_server: get_str(&nvs, K_MQTT_SERVER)?.unwrap_or_default(),
        mqtt_port: get_str(&nvs, K_MQTT_PORT)?
            .and_then(|s| s.parse().ok())
            .unwrap_or(1883),
        mqtt_user: get_str(&nvs, K_MQTT_USER)?.unwrap_or_default(),
        mqtt_password: get_str(&nvs, K_MQTT_PASS)?.unwrap_or_default(),
        mqtt_topic: get_str(&nvs, K_MQTT_TOPIC)?.unwrap_or_else(|| "/data/nodes".into()),
        sleep_minutes: get_str(&nvs, K_SLEEP_MINS)?
            .and_then(|s| s.parse().ok())
            .unwrap_or(5),
        mqtt_qos: get_str(&nvs, K_MQTT_QOS)?
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(0),
        bme280_addr: 0x76,
        sda_pin: get_str(&nvs, K_SDA_PIN)?
            .and_then(|s| s.parse().ok())
            .unwrap_or(21),
        scl_pin: get_str(&nvs, K_SCL_PIN)?
            .and_then(|s| s.parse().ok())
            .unwrap_or(22),
        adc_pin: get_str(&nvs, K_ADC_PIN)?
            .and_then(|s| s.parse().ok())
            .unwrap_or(33),
        send_temperature: nvs_bool(&nvs, K_SEND_TEMP)?,
        send_pressure: nvs_bool(&nvs, K_SEND_PRES)?,
        send_humidity: nvs_bool(&nvs, K_SEND_HUMI)?,
        send_battery: nvs_bool(&nvs, K_SEND_BAT)?,
    }))
}

/// Persist config to NVS (overwrites any previous values).
pub fn save_to_nvs(c: &Config) -> Result<(), ConfigError> {
    let _lock = NVS_LOCK
        .lock()
        .map_err(|_| ConfigError::Storage("NVS mutex poisoned".into()))?;
    let mut nvs = open_nvs()?;
    set_str(&mut nvs, K_DEVICE_NAME, &c.device_name)?;
    set_str(&mut nvs, K_ROOM, &c.room)?;
    set_str(&mut nvs, K_WIFI_SSID, &c.wifi_ssid)?;
    set_str(&mut nvs, K_WIFI_PASS, &c.wifi_password)?;
    set_str(&mut nvs, K_MQTT_SERVER, &c.mqtt_server)?;
    set_str(&mut nvs, K_MQTT_PORT, &c.mqtt_port.to_string())?;
    set_str(&mut nvs, K_MQTT_USER, &c.mqtt_user)?;
    set_str(&mut nvs, K_MQTT_PASS, &c.mqtt_password)?;
    set_str(&mut nvs, K_MQTT_TOPIC, &c.mqtt_topic)?;
    set_str(&mut nvs, K_SLEEP_MINS, &c.sleep_minutes.to_string())?;
    set_str(&mut nvs, K_MQTT_QOS,  &c.mqtt_qos.to_string())?;
    set_str(&mut nvs, K_SDA_PIN, &c.sda_pin.to_string())?;
    set_str(&mut nvs, K_SCL_PIN, &c.scl_pin.to_string())?;
    set_str(&mut nvs, K_ADC_PIN, &c.adc_pin.to_string())?;
    set_str(
        &mut nvs,
        K_SEND_TEMP,
        if c.send_temperature { "1" } else { "0" },
    )?;
    set_str(
        &mut nvs,
        K_SEND_PRES,
        if c.send_pressure { "1" } else { "0" },
    )?;
    set_str(
        &mut nvs,
        K_SEND_HUMI,
        if c.send_humidity { "1" } else { "0" },
    )?;
    set_str(&mut nvs, K_SEND_BAT, if c.send_battery { "1" } else { "0" })?;
    Ok(())
}
