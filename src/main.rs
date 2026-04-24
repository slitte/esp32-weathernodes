//! ESP32 WeatherNode – boot entry point.
//!
//! Boot sequence:
//!   1. Initialise UART logger.
//!   2. Log wakeup reason (mirrors Arduino's print_wakeup_reason).
//!   3. Load config from NVS.
//!      • Config found  → measurement mode  (Step 3+)
//!      • Config absent → AP config portal  (Step 2)

use esp_idf_svc::{eventloop::EspSystemEventLoop, hal::{gpio::AnyIOPin, peripherals::Peripherals}, log::EspLogger};
use esp_idf_sys as _;

mod ap_mode;
mod config;
mod network;
mod sensor;

const TAG: &str = "main";

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    log::info!(target: TAG, "");
    log::info!(target: TAG, "=== ESP32 WeatherNode booting ===");
    log_wakeup_reason();

    let peripherals = Peripherals::take()?;
    let sysloop     = EspSystemEventLoop::take()?;

    match config::load_from_nvs() {
        Ok(Some(cfg)) => {
            log::info!(target: TAG,
                "Config OK: device='{}' wifi='{}' mqtt={}:{}",
                cfg.device_name, cfg.wifi_ssid, cfg.mqtt_server, cfg.mqtt_port);
            if let Err(e) = cfg.validate() {
                log::error!(target: TAG, "Config invalid: {e} – starting config portal");
                ap_mode::run(peripherals.modem, sysloop, peripherals.i2c0)?;
            } else {
                run_measurement_mode(cfg, peripherals, sysloop);
            }
        }
        Ok(None) => {
            log::warn!(target: TAG, "No config in NVS – starting config portal");
            ap_mode::run(peripherals.modem, sysloop, peripherals.i2c0)?;
        }
        Err(e) => {
            log::error!(target: TAG, "Config load error: {e} – starting config portal");
            ap_mode::run(peripherals.modem, sysloop, peripherals.i2c0)?;
        }
    }

    unreachable!()
}

// ---------------------------------------------------------------------------
// Mode stubs
// ---------------------------------------------------------------------------

fn run_measurement_mode(
    cfg:     config::Config,
    p:       Peripherals,
    sysloop: EspSystemEventLoop,
) -> ! {
    let sleep_min = cfg.sleep_minutes;
    log::info!(target: TAG, "--- Measurement mode (sleep: {sleep_min} min) ---");

    // ── Battery ADC (configurable, default GPIO 33 = ADC1 channel 5) ────────────
    // ADC failure is non-fatal: log the error and continue without battery data.
    // Only GPIO 32–39 are valid ADC1 channels on the ESP32.
    let bat_v: Option<f32> = if cfg.send_battery {
        use esp_idf_svc::hal::adc::oneshot::{
            config::{AdcChannelConfig, Calibration},
            AdcChannelDriver, AdcDriver,
        };
        use esp_idf_svc::hal::adc::attenuation::DB_11;
        (|| -> anyhow::Result<f32> {
            let adc    = AdcDriver::new(p.adc1)?;
            // Calibration::None → read() returns raw 12-bit counts (0–4095).
            // bat_voltage_from_raw() expects this range; Curve/Line would return mV.
            let ch_cfg = AdcChannelConfig { attenuation: DB_11, calibration: Calibration::None, ..Default::default() };
            // Peripheral tokens are distinct zero-sized types in esp-idf-hal –
            // the type system prevents dynamic dispatch without unsafe.
            // Each arm is identical code but a different compile-time pin type.
            let raw: u16 = match cfg.adc_pin {
                32 => AdcChannelDriver::new(&adc, p.pins.gpio32, &ch_cfg)?.read()?,
                33 => AdcChannelDriver::new(&adc, p.pins.gpio33, &ch_cfg)?.read()?,
                34 => AdcChannelDriver::new(&adc, p.pins.gpio34, &ch_cfg)?.read()?,
                35 => AdcChannelDriver::new(&adc, p.pins.gpio35, &ch_cfg)?.read()?,
                36 => AdcChannelDriver::new(&adc, p.pins.gpio36, &ch_cfg)?.read()?,
                37 => AdcChannelDriver::new(&adc, p.pins.gpio37, &ch_cfg)?.read()?,
                38 => AdcChannelDriver::new(&adc, p.pins.gpio38, &ch_cfg)?.read()?,
                39 => AdcChannelDriver::new(&adc, p.pins.gpio39, &ch_cfg)?.read()?,
                n  => anyhow::bail!("GPIO{n} is not an ADC1 channel (valid: 32–39)"),
            };
            Ok(sensor::bat_voltage_from_raw(raw))
        })()
        .map_err(|e| log::error!(target: TAG, "Battery ADC: {e}"))
        .ok()
    } else {
        None
    };

    // ── BME280 via I²C (configurable, default SDA=GPIO21, SCL=GPIO22) ────────
    // SAFETY: Config::validate() is called in main() before entering this function.
    // It guarantees: sda_pin ≤ 33, scl_pin ≤ 33, sda_pin ≠ scl_pin, and
    // (when send_battery) adc_pin ∉ {sda_pin, scl_pin}.
    // The ADC closure above has already dropped its AdcChannelDriver, so no other
    // driver holds these pins at this point.
    let sda = unsafe { AnyIOPin::new(cfg.sda_pin as i32) };
    let scl = unsafe { AnyIOPin::new(cfg.scl_pin as i32) };
    let bme_cfg = sensor::Bme280Config {
        addr:             cfg.bme280_addr,
        send_temperature: cfg.send_temperature,
        send_pressure:    cfg.send_pressure,
        send_humidity:    cfg.send_humidity,
    };
    let (temp, pres, humi) = match sensor::read_bme280(
        &bme_cfg,
        p.i2c0,
        sda,
        scl,
    ) {
        Ok(v)  => v,
        Err(e) => {
            log::error!(target: TAG, "BME280: {e}");
            go_to_sleep(sleep_min);
        }
    };

    let data = sensor::SensorData {
        temperature:     temp,
        pressure:        pres,
        humidity:        humi,
        battery_voltage: bat_v,
    };

    // ── Log sensor readings ───────────────────────────────────────────────────
    if let Some(t) = data.temperature     { log::info!(target: TAG, "Temperature: {t:.2} °C"); }
    if let Some(p) = data.pressure        { log::info!(target: TAG, "Pressure:    {p:.2} hPa"); }
    if let Some(h) = data.humidity        { log::info!(target: TAG, "Humidity:    {h:.2} %"); }
    if let Some(v) = data.battery_voltage { log::info!(target: TAG, "Battery:     {v:.2} V"); }

    // ── WiFi ──────────────────────────────────────────────────────────────────
    let wifi = match network::connect_wifi(&cfg, p.modem, sysloop) {
        Ok(w)  => w,
        Err(e) => {
            log::error!(target: TAG, "WiFi: {e}");
            go_to_sleep(sleep_min);
        }
    };

    // ── MQTT ──────────────────────────────────────────────────────────────────
    if let Err(e) = network::publish_mqtt(&cfg, &data) {
        log::error!(target: TAG, "MQTT: {e}");
        // Still sleep on MQTT error; WiFi drops with `wifi` below.
    }
    drop(wifi); // WiFi stack shuts down before sleep.

    go_to_sleep(sleep_min);
}

// ---------------------------------------------------------------------------
// Deep sleep
// ---------------------------------------------------------------------------

fn go_to_sleep(minutes: u32) -> ! {
    let us = minutes as u64 * 60 * 1_000_000;
    log::info!(target: TAG, "Deep sleep for {minutes} min…");
    // Let the UART TX FIFO drain before cutting power to the RF/CPU domain.
    std::thread::sleep(std::time::Duration::from_millis(20));
    unsafe {
        esp_idf_sys::esp_sleep_enable_timer_wakeup(us);
        esp_idf_sys::esp_deep_sleep_start();
    }
}

// ---------------------------------------------------------------------------
// Wakeup reason (mirrors Arduino's print_wakeup_reason)
// ---------------------------------------------------------------------------

fn log_wakeup_reason() {
    use esp_idf_sys::{
        esp_sleep_get_wakeup_cause,
        esp_sleep_source_t_ESP_SLEEP_WAKEUP_EXT0     as EXT0,
        esp_sleep_source_t_ESP_SLEEP_WAKEUP_EXT1     as EXT1,
        esp_sleep_source_t_ESP_SLEEP_WAKEUP_TIMER    as TIMER,
        esp_sleep_source_t_ESP_SLEEP_WAKEUP_TOUCHPAD as TOUCH,
        esp_sleep_source_t_ESP_SLEEP_WAKEUP_ULP      as ULP,
    };
    let reason = unsafe { esp_sleep_get_wakeup_cause() };
    let msg = match reason {
        EXT0  => "EXT0 (RTC_IO)",
        EXT1  => "EXT1 (RTC_CNTL)",
        TIMER => "deep-sleep timer",
        TOUCH => "touchpad",
        ULP   => "ULP co-processor",
        _     => "power-on / undefined",
    };
    log::info!(target: TAG, "Wakeup: {msg}");
}
