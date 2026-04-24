//! BME280 (I²C) sensor driver + battery voltage helper.
//!
//! The BME280 is driven in *forced mode* (one-shot measurement) via a minimal
//! direct I²C implementation.  No external sensor crate required – avoids
//! embedded-hal version mismatches.
//!
//! Compensation math follows the Bosch BME280 datasheet §4.2.3 (float variant).

use anyhow::Context as _;
use esp_idf_hal::{
    delay::FreeRtos,
    gpio::{InputPin, OutputPin},
    i2c::{I2cConfig, I2cDriver},
    peripheral::Peripheral,
    prelude::*,
};

const TAG: &str = "sensor";

// ─── Public output ────────────────────────────────────────────────────────────

pub struct SensorData {
    pub temperature:     Option<f32>,   // °C
    pub pressure:        Option<f32>,   // hPa
    pub humidity:        Option<f32>,   // %RH
    pub battery_voltage: Option<f32>,   // V
}

/// Minimal BME280 configuration, decoupled from the runtime `Config` struct.
/// Passed to [`read_bme280`] so `sensor.rs` has no dependency on the
/// configuration layer.
pub struct Bme280Config {
    /// I²C address (0x76 or 0x77).
    pub addr:             u8,
    pub send_temperature: bool,
    pub send_pressure:    bool,
    pub send_humidity:    bool,
}

// ─── BME280 entry point ───────────────────────────────────────────────────────

/// Read temperature, pressure and humidity from a BME280 via I²C.
/// Returns `(temp, pres, humi)` as `Option<f32>` according to `cfg.send_*`.
pub fn read_bme280<'d>(
    cfg: &Bme280Config,
    i2c: impl Peripheral<P = impl esp_idf_hal::i2c::I2c> + 'd,
    sda: impl Peripheral<P = impl InputPin + OutputPin> + 'd,
    scl: impl Peripheral<P = impl InputPin + OutputPin> + 'd,
) -> anyhow::Result<(Option<f32>, Option<f32>, Option<f32>)> {
    // No BME280 value requested → skip I²C entirely.
    let any_bme = cfg.send_temperature || cfg.send_pressure || cfg.send_humidity;
    if !any_bme {
        log::info!(target: TAG, "BME280 skipped (all disabled)");
        return Ok((None, None, None));
    }

    let i2c_cfg = I2cConfig::new().baudrate(400_u32.kHz().into());
    let mut bus = I2cDriver::new(i2c, sda, scl, &i2c_cfg)
        .context("I²C init failed")?;

    let addr = cfg.addr;

    let calib = read_calibration(&mut bus, addr).context("BME280 calibration read")?;
    // Pass cfg so only requested channels are measured.
    trigger_forced(&mut bus, addr, cfg).context("BME280 trigger forced mode")?;
    // Worst-case measurement time: 2 ms + 3×2.3 ms ≈ 9 ms; 15 ms is safe.
    FreeRtos::delay_ms(15);
    let (temp, pres, humi) = raw_and_compensate(&mut bus, addr, &calib, cfg)
        .context("BME280 raw read")?;

    log::info!(target: TAG, "BME280: {temp:.2} °C  {pres:.2} hPa  {humi:.2} %");

    Ok((
        cfg.send_temperature.then(|| round2(temp)),
        cfg.send_pressure   .then(|| round2(pres)),
        cfg.send_humidity   .then(|| round2(humi)),
    ))
}

// ─── Battery voltage ──────────────────────────────────────────────────────────

/// Convert raw 12-bit ADC reading (0–4095) to battery input voltage [V].
///
/// Voltage divider used in the reference hardware: 127 kΩ / 100 kΩ
/// → same formula as the original Arduino sketch.
pub fn bat_voltage_from_raw(raw: u16) -> f32 {
    raw as f32 / 4095.0 * 3.3 * (127_000.0 / 100_000.0)
}

// ─── Rounding helper ─────────────────────────────────────────────────────────

fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

// ─── Low-level I²C helpers ────────────────────────────────────────────────────

fn write_reg(bus: &mut I2cDriver<'_>, addr: u8, reg: u8, val: u8) -> anyhow::Result<()> {
    bus.write(addr, &[reg, val], 1_000)
        .with_context(|| format!("I²C write reg 0x{reg:02x}"))?;
    Ok(())
}

fn read_regs(bus: &mut I2cDriver<'_>, addr: u8, reg: u8, buf: &mut [u8]) -> anyhow::Result<()> {
    bus.write_read(addr, &[reg], buf, 1_000)
        .with_context(|| format!("I²C read reg 0x{reg:02x} ×{}", buf.len()))?;
    Ok(())
}

// ─── Calibration data ─────────────────────────────────────────────────────────

struct Calib {
    t1: u16, t2: i16, t3: i16,
    p1: u16, p2: i16, p3: i16, p4: i16, p5: i16,
    p6: i16, p7: i16, p8: i16, p9: i16,
    h1: u8,  h2: i16, h3: u8,  h4: i16, h5: i16, h6: i8,
}

fn read_calibration(bus: &mut I2cDriver<'_>, addr: u8) -> anyhow::Result<Calib> {
    let mut b1 = [0u8; 24]; // 0x88–0x9F: T1..T3, P1..P9
    let mut h1 = [0u8; 1];  // 0xA1:      H1
    let mut b2 = [0u8; 7];  // 0xE1–0xE7: H2..H6

    read_regs(bus, addr, 0x88, &mut b1)?;
    read_regs(bus, addr, 0xA1, &mut h1)?;
    read_regs(bus, addr, 0xE1, &mut b2)?;

    let u16le = |lo: u8, hi: u8| u16::from_le_bytes([lo, hi]);
    let i16le = |lo: u8, hi: u8| i16::from_le_bytes([lo, hi]);

    // H4: 0xE4[7:4]‖0xE5[3:0]  (sign-extended 12-bit)
    let h4 = ((b2[3] as i16) << 4) | ((b2[4] as i16) & 0x0F);
    // H5: 0xE6[7:4]‖0xE5[7:4]  (sign-extended 12-bit)
    let h5 = ((b2[5] as i16) << 4) | ((b2[4] as i16) >> 4);

    Ok(Calib {
        t1: u16le(b1[0],  b1[1]),
        t2: i16le(b1[2],  b1[3]),
        t3: i16le(b1[4],  b1[5]),
        p1: u16le(b1[6],  b1[7]),
        p2: i16le(b1[8],  b1[9]),
        p3: i16le(b1[10], b1[11]),
        p4: i16le(b1[12], b1[13]),
        p5: i16le(b1[14], b1[15]),
        p6: i16le(b1[16], b1[17]),
        p7: i16le(b1[18], b1[19]),
        p8: i16le(b1[20], b1[21]),
        p9: i16le(b1[22], b1[23]),
        h1: h1[0],
        h2: i16le(b2[0], b2[1]),
        h3: b2[2],
        h4,
        h5,
        h6: b2[6] as i8,
    })
}

// ─── Forced-mode trigger ──────────────────────────────────────────────────────

fn trigger_forced(bus: &mut I2cDriver<'_>, addr: u8, cfg: &Bme280Config) -> anyhow::Result<()> {
    // Temperature must be measured whenever pressure or humidity compensation is needed
    // (the compensation formulas all depend on t_fine from the temperature channel).
    let need_t = cfg.send_temperature || cfg.send_pressure || cfg.send_humidity;
    let osrs_t: u8 = if need_t            { 0b001 } else { 0 };
    let osrs_p: u8 = if cfg.send_pressure { 0b001 } else { 0 };
    let osrs_h: u8 = if cfg.send_humidity { 0b001 } else { 0 };

    // ctrl_hum (0xF2) MUST be written before ctrl_meas takes effect.
    write_reg(bus, addr, 0xF2, osrs_h)?;
    write_reg(bus, addr, 0xF4, (osrs_t << 5) | (osrs_p << 2) | 0b01)?; // forced mode
    Ok(())
}

// ─── Raw read + compensation ──────────────────────────────────────────────────

// BME280 skipped-channel sentinel values (datasheet §4.2.3).
const BME280_SKIP_TP: i32 = 0x80000; // press or temp skipped
const BME280_SKIP_H:  i32 = 0x8000;  // hum skipped

fn raw_and_compensate(bus: &mut I2cDriver<'_>, addr: u8, c: &Calib, cfg: &Bme280Config) -> anyhow::Result<(f32, f32, f32)> {
    let mut raw = [0u8; 8]; // 0xF7–0xFE
    read_regs(bus, addr, 0xF7, &mut raw)?;

    let adc_p = ((raw[0] as i32) << 12) | ((raw[1] as i32) << 4) | ((raw[2] as i32) >> 4);
    let adc_t = ((raw[3] as i32) << 12) | ((raw[4] as i32) << 4) | ((raw[5] as i32) >> 4);
    let adc_h = ((raw[6] as i32) <<  8) |  (raw[7] as i32);

    // Temperature is always measured when we reach here (needed for t_fine).
    let (temp, t_fine) = if adc_t == BME280_SKIP_TP {
        (0.0_f32, 0.0_f64)
    } else {
        compensate_temperature(adc_t, c)
    };

    let pres = if cfg.send_pressure && adc_p != BME280_SKIP_TP {
        compensate_pressure(adc_p, t_fine, c)
    } else {
        0.0
    };

    let humi = if cfg.send_humidity && adc_h != BME280_SKIP_H {
        compensate_humidity(adc_h, t_fine, c)
    } else {
        0.0
    };

    Ok((temp, pres, humi))
}

// ─── Compensation formulas (Bosch BME280 datasheet §4.2.3, float variant) ─────

fn compensate_temperature(adc_t: i32, c: &Calib) -> (f32, f64) {
    let v1 = adc_t as f64 / 16384.0 - c.t1 as f64 / 1024.0;
    let v1 = v1 * c.t2 as f64;
    let v2 = adc_t as f64 / 131072.0 - c.t1 as f64 / 8192.0;
    let v2 = v2 * v2 * c.t3 as f64;
    let t_fine = v1 + v2;
    ((t_fine / 5120.0) as f32, t_fine)
}

fn compensate_pressure(adc_p: i32, t_fine: f64, c: &Calib) -> f32 {
    let v1 = t_fine / 2.0 - 64000.0;
    let v2 = v1 * v1 * c.p6 as f64 / 32768.0;
    let v2 = v2 + v1 * c.p5 as f64 * 2.0;
    let v2 = v2 / 4.0 + c.p4 as f64 * 65536.0;
    let v1 = (c.p3 as f64 * v1 * v1 / 524288.0 + c.p2 as f64 * v1) / 524288.0;
    let v1 = (1.0 + v1 / 32768.0) * c.p1 as f64;
    if v1 == 0.0 { return 0.0; }
    let p  = 1048576.0 - adc_p as f64;
    let p  = (p - v2 / 4096.0) * 6250.0 / v1;
    let v1 = c.p9 as f64 * p * p / 2147483648.0;
    let v2 = p * c.p8 as f64 / 32768.0;
    // Pa → hPa
    ((p + (v1 + v2 + c.p7 as f64) / 16.0) / 100.0) as f32
}

fn compensate_humidity(adc_h: i32, t_fine: f64, c: &Calib) -> f32 {
    let x = t_fine - 76800.0;
    let x = (adc_h as f64
        - (c.h4 as f64 * 64.0 + c.h5 as f64 / 16384.0 * x))
        * (c.h2 as f64 / 65536.0
            * (1.0 + c.h6 as f64 / 67108864.0 * x
                * (1.0 + c.h3 as f64 / 67108864.0 * x)));
    let x = x * (1.0 - c.h1 as f64 * x / 524288.0);
    x.clamp(0.0, 100.0) as f32
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn bat_voltage_zero_raw() {
        assert_eq!(bat_voltage_from_raw(0), 0.0);
    }

    #[test]
    fn bat_voltage_full_scale() {
        // raw=4095 → 3.3 V * 1.27 ≈ 4.191 V
        let v = bat_voltage_from_raw(4095);
        assert!((v - 4.191_f32).abs() < 0.001, "expected ≈4.191 V, got {v}");
    }

    #[test]
    fn bat_voltage_midscale() {
        // raw=2048 → ~2.097 V
        let v = bat_voltage_from_raw(2048);
        let expected = 2048_f32 / 4095.0 * 3.3 * (127_000.0 / 100_000.0);
        assert!((v - expected).abs() < 0.001, "midscale mismatch: {v} vs {expected}");
    }

    #[test]
    fn round2_truncates_correctly() {
        assert_eq!(round2(1.234_56), 1.23);
        assert_eq!(round2(1.235), 1.24);
        assert_eq!(round2(0.0), 0.0);
        assert_eq!(round2(-3.145), -3.15);
    }
}
