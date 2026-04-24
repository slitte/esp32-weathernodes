# Technische Dokumentation – ESP32 WeatherNode

## Inhaltsverzeichnis

1. [Projektstruktur](#1-projektstruktur)
2. [Boot-Sequenz](#2-boot-sequenz)
3. [Module](#3-module)
   - [main.rs](#31-mainrs)
   - [config.rs](#32-configrs)
   - [ap_mode.rs](#33-ap_moders)
   - [sensor.rs](#34-sensorks)
   - [network.rs](#35-networkrs)
4. [Konfiguration & NVS](#4-konfiguration--nvs)
5. [Hardware-Details](#5-hardware-details)
6. [BME280-Treiber](#6-bme280-treiber)
7. [Batterie-ADC](#7-batterie-adc)
8. [MQTT-Architektur](#8-mqtt-architektur)
9. [Deep Sleep & Fehlerbehandlung](#9-deep-sleep--fehlerbehandlung)
10. [Toolchain & Abhängigkeiten](#10-toolchain--abhängigkeiten)
11. [Bekannte Einschränkungen](#11-bekannte-einschränkungen)

---

## 1. Projektstruktur

```
esp32-weathernodes/
├── .cargo/
│   └── config.toml        Ziel-Triple, Linker, IDF-Version, Clang-Pfade
├── Arduino/               Referenz-Implementierung (read-only)
│   ├── ESP32Weathernodes.ino
│   └── settings.h
├── src/
│   ├── main.rs            Einstiegspunkt, Boot-Dispatcher, Deep Sleep
│   ├── config.rs          Config-Struct, NVS-Persistenz
│   ├── ap_mode.rs         AP-Konfigurationsportal (HTTP + WiFi AP)
│   ├── sensor.rs          BME280-Treiber, Batterie-Spannungsformel
│   └── network.rs         WiFi STA, MQTT publish, JSON-Payload
├── build.rs               embuild-Integration
├── Cargo.toml             Abhängigkeiten
└── rust-toolchain.toml    Pinnt auf "esp" Toolchain (Xtensa-Fork)
```

---

## 2. Boot-Sequenz

```
main()
  │
  ├─ link_patches()           IDF-Startup-Linkage sicherstellen
  ├─ EspLogger::initialize()  UART0-Logger initialisieren
  ├─ log_wakeup_reason()      Wakeup-Ursache loggen (Timer / EXT / Power-On)
  ├─ Peripherals::take()      Alle Peripherals einmal belegen
  ├─ EspSystemEventLoop::take()
  │
  └─ config::load_from_nvs()
       │
       ├─ Ok(None)   → ap_mode::run()           [kehrt nie zurück: esp_restart()]
       ├─ Err(_)     → ap_mode::run()           [kehrt nie zurück: esp_restart()]
       └─ Ok(Some(cfg)) → run_measurement_mode() [kehrt nie zurück: -> !]
```

`run_measurement_mode` ist als `-> !` deklariert und endet immer in `go_to_sleep()`,
das `esp_deep_sleep_start()` aufruft (kehrt nie zurück).

---

## 3. Module

### 3.1 `main.rs`

Enthält:
- `main()` – Einstiegspunkt, delegiert anhand der NVS-Konfig
- `run_measurement_mode(cfg, peripherals, sysloop) -> !` – Mess- und Sendezyklus
- `go_to_sleep(minutes: u32) -> !` – setzt Timer, ruft Deep Sleep

**Fehlerbehandlung in `run_measurement_mode`:**

| Fehlerquelle | Verhalten |
|---|---|
| Batterie-ADC | nicht fatal: `bat_v = None`, weiter |
| I²C-Pin ungültig (GPIO > 33) | fatal: loggen → `go_to_sleep()` |
| BME280 | fatal: loggen → `go_to_sleep()` |
| WiFi | fatal: loggen → `go_to_sleep()` |
| MQTT | loggen, WiFi droppen → `go_to_sleep()` |

**I²C-Pin-Validierung:** GPIO 34–39 sind auf dem ESP32 reine Eingänge und können
den I²C-Bus nicht treiben. `run_measurement_mode` prüft `sda_pin` und `scl_pin`
vor `AnyIOPin::new()` und schläft bei ungültigen Werten sofort ein.

---

### 3.2 `config.rs`

Definiert die `Config`-Struct und deren Persistenz im ESP-IDF NVS (Non-Volatile Storage).

**`Config`-Felder:**

| Feld | Typ | Standard | Beschreibung |
|---|---|---|---|
| `device_name` | `String` | `"weathernode"` | MQTT Client-ID / ESPName im Payload |
| `room` | `String` | `""` | Raumname, optional im Payload (`"room"`) |
| `wifi_ssid` | `String` | – | WLAN-Netzwerkname |
| `wifi_password` | `String` | – | WLAN-Passwort |
| `mqtt_server` | `String` | – | IP oder Hostname des MQTT-Brokers |
| `mqtt_port` | `u16` | `1883` | MQTT-Port |
| `mqtt_user` | `String` | – | MQTT-Benutzername (optional) |
| `mqtt_password` | `String` | – | MQTT-Passwort (optional) |
| `mqtt_topic` | `String` | `"/data/nodes"` | Publish-Topic |
| `sleep_minutes` | `u32` | `5` | Deep-Sleep-Dauer |
| `bme280_addr` | `u8` | `0x76` | I²C-Adresse des BME280 (nicht konfigurierbar) |
| `sda_pin` | `u8` | `21` | I²C SDA GPIO-Nummer |
| `scl_pin` | `u8` | `22` | I²C SCL GPIO-Nummer |
| `adc_pin` | `u8` | `33` | ADC GPIO-Nummer (nur 32–39 gültig, ADC1) |
| `send_temperature` | `bool` | `true` | Temperatur messen und senden |
| `send_pressure` | `bool` | `true` | Luftdruck messen und senden |
| `send_humidity` | `bool` | `true` | Luftfeuchtigkeit messen und senden |
| `send_battery` | `bool` | `true` | Batteriespannung messen und senden |

**NVS-Namespace:** `wnode_cfg`

**NVS-Schlüssel** (max. 15 Zeichen, ESP-IDF-Limit):

| Schlüssel | Feld |
|---|---|
| `device_name` | `device_name` |
| `wifi_ssid` | `wifi_ssid` |
| `wifi_password` | `wifi_password` |
| `mqtt_server` | `mqtt_server` |
| `mqtt_port` | `mqtt_port` (als String) |
| `mqtt_user` | `mqtt_user` |
| `mqtt_password` | `mqtt_password` |
| `mqtt_topic` | `mqtt_topic` |
| `sleep_mins` | `sleep_minutes` |
| `send_temp` | `send_temperature` (`"1"`/`"0"`) |
| `send_pres` | `send_pressure` (`"1"`/`"0"`) |
| `send_humi` | `send_humidity` (`"1"`/`"0"`) |
| `send_bat` | `send_battery` (`"1"`/`"0"`) |

**Erkennung Erstboot:** `load_from_nvs()` gibt `Ok(None)` zurück, wenn `wifi_ssid`
nicht im NVS vorhanden oder leer ist.

**Bool-Default:** Fehlt ein `send_*`-Schlüssel im NVS (z. B. nach Firmware-Update),
wird `true` zurückgegeben – alle Sensoren sind standardmäßig aktiv.

**`open_nvs()`** wird intern immer mit `rw=true` geöffnet (Namespace wird beim
Erstboot angelegt). Der `rw`-Parameter wurde entfernt, da er nie `false` war.

**NVS-Lesepuffer:** `get_str()` verwendet einen 256-Byte-Puffer (ausreichend für
lange Passwörter und URLs; vormals 128 Byte).

---

### 3.3 `ap_mode.rs`

Startet den ESP32 als offenen WLAN-Accesspoint und stellt ein Konfigurationsportal
per HTTP bereit.

**SSID-Schema:** `weathernode-<CHIPID>` wobei `<CHIPID>` die letzten 3 Bytes
der Basis-MAC-Adresse als Hex-String sind (z. B. `weathernode-a1b2c3`).

**IP des Portals:** `192.168.71.1` (Standard-Gateway-IP des AP)

**Endpunkte:**

| Methode | Pfad | Funktion |
|---|---|---|
| `GET` | `/` | HTML-Formular ausliefern |
| `POST` | `/save` | Formular parsen, in NVS schreiben, `esp_restart()` |
| `POST` | `/test_wifi` | STA-Verbindung testen → JSON `{"ok":bool,"msg":"..."}` |
| `POST` | `/test_mqtt` | MQTT-Broker testen → JSON `{"ok":bool,"msg":"..."}` |
| `POST` | `/sensor` | BME280 live auslesen → JSON `{"ok":bool,"temp":"...","pres":"...","humi":"..."}` |

**Form-Parsing:** `application/x-www-form-urlencoded`, eigene `url_decode()`-Implementierung.
Checkboxen: nicht übertragene Felder → Schlüssel fehlt im POST-Body → `false`.

**Architektur:** HTTP-Handler läuft auf dem IDF-Task-Stack. Die geparste Config
wird per `Arc<Mutex<Option<Config>>>` an einen Polling-Loop übergeben, der im
Haupt-Task den NVS-Schreibvorgang ausführt (NVS darf nicht aus einem Interrupt-Kontext
heraus beschrieben werden).

---

### 3.4 `sensor.rs`

Enthält den BME280-I²C-Treiber und die Batteriespannungsformel.
Kein externer Sensor-Crate – direkte Implementierung gegen `esp_idf_hal::i2c::I2cDriver`
vermeidet `embedded-hal`-Versionskompatibilitätsprobleme.

**Öffentliche API:**

```rust
/// Minimale BME280-Konfiguration, entkoppelt vom vollen Config-Struct.
pub struct Bme280Config {
    pub addr:             u8,    // I²C-Adresse (0x76 oder 0x77)
    pub send_temperature: bool,
    pub send_pressure:    bool,
    pub send_humidity:    bool,
}

pub fn read_bme280<'d>(cfg: &Bme280Config, i2c, sda, scl)
    -> anyhow::Result<(Option<f32>, Option<f32>, Option<f32>)>
// Rückgabe: (Temperatur °C, Luftdruck hPa, Luftfeuchtigkeit %RH)
// None wenn das jeweilige send_*-Flag false ist oder Kanal übersprungen wurde.

pub fn bat_voltage_from_raw(raw: u16) -> f32
// Konvertiert ADC-Rohwert (0–4095) in Batterie-Eingangsspannung [V].
// Unit-Tests im Modul (cfg(test)) prüfen Grenzwerte und round2().

pub struct SensorData {
    pub temperature:     Option<f32>,
    pub pressure:        Option<f32>,
    pub humidity:        Option<f32>,
    pub battery_voltage: Option<f32>,
}
```

**Aufruf in `main.rs`:**

```rust
let bme_cfg = sensor::Bme280Config {
    addr:             cfg.bme280_addr,
    send_temperature: cfg.send_temperature,
    send_pressure:    cfg.send_pressure,
    send_humidity:    cfg.send_humidity,
};
let (temp, pres, humi) = sensor::read_bme280(&bme_cfg, p.i2c0, sda, scl)?;
```

Detaillierte Beschreibung des Treibers: → [Abschnitt 6](#6-bme280-treiber)

---

### 3.5 `network.rs`

Enthält WiFi-STA-Verbindung und MQTT-Publish.

**Öffentliche API:**

```rust
pub fn connect_wifi(cfg, modem, sysloop)
    -> anyhow::Result<BlockingWifi<EspWifi<'static>>>
// Gibt das WiFi-Handle zurück. Droppen des Handles trennt die Verbindung.

pub fn publish_mqtt(cfg, data: &SensorData)
    -> anyhow::Result<()>
// Verbindet zum Broker, sendet Payload, trennt.
```

Detaillierte Beschreibung: → [Abschnitt 8](#8-mqtt-architektur)

---

## 4. Konfiguration & NVS

Die Konfiguration wird einmalig über das AP-Portal eingetragen und dann dauerhaft
im Flash-NVS gespeichert. Beim nächsten Boot (auch nach Deep Sleep) wird sie
aus dem NVS geladen – kein Datenverlust durch Neustart.

**NVS löschen / Werksreset:**

```bash
espflash erase-flash
```

Der nächste Boot startet wieder das AP-Portal.

---

## 5. Hardware-Details

### I²C-Pinbelegung

| Signal | GPIO |
|---|---|
| SDA | 21 |
| SCL | 22 |

Das sind die Standard-I²C-Pins der ESP32-Arduino-BSP-Belegung.
Die I²C-Taktrate beträgt 400 kHz (Fast Mode).

### Batterie-ADC

```
Batterie (+) ──┬── 127 kΩ ──┬── GPIO 33 (ADC1 CH5)
               │            │
              GND         100 kΩ
                            │
                           GND
```

**Spannungsteiler-Verhältnis:** (127k + 100k) / 100k = 2,27

**Spannungsformel:**
```
U_bat = ADC_raw / 4095 × 3,3 V × (127000 / 100000)
      = ADC_raw / 4095 × 4,191 V
```

**Messbereich:** 0 – ca. 4,19 V Eingangsspannung (ADC-Sättigung bei 3,3 V)

**ADC-Konfiguration:** GPIO 33, ADC1 Kanal 5, Dämpfung DB_11 (0–3,9 V Eingangsspannung am ADC-Pin)

---

## 6. BME280-Treiber

Der Treiber in `sensor.rs` ist eine minimale direkte I²C-Implementierung
ohne externen Sensor-Crate.

### Ablauf einer Messung

```
1. Kalibrierungsdaten lesen (einmalig pro Wake-Up)
   ├─ 0x88–0x9F → T1..T3, P1..P9 (24 Bytes)
   ├─ 0xA1      → H1 (1 Byte)
   └─ 0xE1–0xE7 → H2..H6 (7 Bytes)

2. Forced Mode triggern
   ├─ 0xF2 (ctrl_hum)  → osrs_h  (0 oder 001b)
   └─ 0xF4 (ctrl_meas) → osrs_t | osrs_p | mode=01

3. Warten: 15 ms (konservativ, Worst-Case ≈ 9 ms bei 1×-Oversampling)

4. Rohdaten lesen: 0xF7–0xFE (8 Bytes)
   ├─ [0..2] → adc_P (20 Bit)
   ├─ [3..5] → adc_T (20 Bit)
   └─ [6..7] → adc_H (16 Bit)

5. Kompensation (Bosch-Formeln, Float-Variante, Datenblatt §4.2.3)
   ├─ compensate_temperature() → (temp °C, t_fine)
   ├─ compensate_pressure(t_fine) → hPa
   └─ compensate_humidity(t_fine) → %RH
```

### Selektive Messung

Nur aktivierte Kanäle werden tatsächlich gemessen:

| `send_pressure` | `send_humidity` | `osrs_t` | `osrs_p` | `osrs_h` |
|:---:|:---:|:---:|:---:|:---:|
| ✓ | ✓ | 001 | 001 | 001 |
| ✓ | ✗ | 001 | 001 | 000 |
| ✗ | ✓ | 001 | 000 | 001 |
| ✗ | ✗ | 001* | 000 | 000 |

*Temperatur wird intern immer mitgemessen, solange mindestens ein BME280-Wert
aktiviert ist, da die Kompensationsformeln für Druck und Feuchte `t_fine`
(aus der Temperatur-ADC-Messung) benötigen. Ist `send_temperature = false`,
wird der Temperaturwert nicht in den Payload aufgenommen, aber für die
interne Berechnung verwendet.

Sind alle drei BME280-Flags `false`, wird der I²C-Bus gar nicht erst geöffnet.

### Sentinel-Werte

Übersprungene BME280-Kanäle liefern definierte Sentinel-Werte (Datenblatt §4.2.3):

| Kanal | Sentinel |
|---|---|
| Temperatur / Druck (übersprungen) | `0x80000` |
| Feuchte (übersprungen) | `0x8000` |

Diese werden erkannt und führen dazu, dass die Kompensation für den jeweiligen
Kanal übersprungen wird.

---

## 7. Batterie-ADC

Der ADC wird nur initialisiert wenn `send_battery = true`.
Ein ADC-Fehler ist nicht fatal – die Messung läuft ohne Batteriewert weiter.

**API (esp-idf-hal 0.45, IDF 5.x Oneshot-Modus):**

```rust
use esp_idf_svc::hal::adc::oneshot::{
    config::AdcChannelConfig, AdcChannelDriver, AdcDriver,
};
use esp_idf_svc::hal::adc::attenuation::DB_11;

let adc    = AdcDriver::new(p.adc1)?;
let ch_cfg = AdcChannelConfig { attenuation: DB_11, ..Default::default() };
let mut ch = AdcChannelDriver::new(&adc, p.pins.gpio33, &ch_cfg)?;
let raw: u16 = ch.read()?;   // 0–4095
```

---

## 8. MQTT-Architektur

### Verbindungsablauf

Der IDF-MQTT-Task erfordert, dass seine Event-Queue kontinuierlich geleert wird.
`publish_mqtt()` löst das durch einen Background-Thread und zwei Channels:

```
publish_mqtt()
  │
  ├─ EspMqttClient::new(url, conf) → (client, conn)
  │
  ├─ Thread: loop { conn.next() }
  │     ├─ EventPayload::Connected(_)  → connected_tx.send(true)
  │     ├─ EventPayload::Published(_)  → published_tx.send(true), break
  │     ├─ EventPayload::Disconnected  → break
  │     └─ Err(_)                      → connected_tx.send(false), break
  │
  ├─ connected_rx.recv_timeout(10 s)
  │     ├─ Ok(true)  → verbunden, weiter
  │     ├─ Ok(false) → Broker hat abgelehnt, Err zurück
  │     └─ Timeout   → Err zurück
  │
  ├─ client.publish(topic, QoS::AtLeastOnce, payload)
  │
  ├─ published_rx.recv_timeout(5 s)   // auf PUBACK warten
  │     └─ Timeout → Warnung loggen (nicht fatal)
  │
  ├─ drop(client)    // trennt vom Broker
  └─ conn_thread.join()
```

### JSON-Payload

Aufgebaut per `build_payload()`, keine externe JSON-Bibliothek:

```json
{"ESPName":"mein-knoten","room":"keller","temp":21.35,"pres":1013.47,"humi":54.20,"batvoltvin":3.84}
```

- `ESPName` ist immer vorhanden
- `room` nur wenn nicht leer konfiguriert
- Alle anderen Felder nur wenn das entsprechende `send_*`-Flag gesetzt ist
- Zahlen gerundet auf 2 Dezimalstellen (via `round2()` in `sensor.rs`)
- QoS 1 (At Least Once) – Broker muss mit PUBACK bestätigen
- **JSON-Injection-Schutz:** `device_name` und `room` werden durch `json_escape()`
  gefiltert (`\` → `\\`, `"` → `\"`), bevor sie in den Payload eingebettet werden.
  Unit-Tests in `network.rs` (`#[cfg(test)]`) prüfen Escaping und Payload-Format.

### WiFi

`connect_wifi()` nutzt `BlockingWifi::connect()` + `wait_netif_up()`.
Beide Aufrufe blockieren intern bis zur Verbindung oder zum Fehler.
Das zurückgegebene Handle `BlockingWifi<EspWifi<'static>>` hält WiFi am Leben;
`drop()` des Handles trennt die Verbindung sauber vor dem Deep Sleep.

---

## 9. Deep Sleep & Fehlerbehandlung

### `go_to_sleep(minutes: u32) -> !`

```rust
fn go_to_sleep(minutes: u32) -> ! {
    let us = minutes as u64 * 60 * 1_000_000;
    unsafe {
        esp_idf_sys::esp_sleep_enable_timer_wakeup(us);
        esp_idf_sys::esp_deep_sleep_start();   // kehrt nie zurück
    }
    unreachable!()
}
```

Der Timer wird direkt vor dem Schlaf gesetzt (nicht beim Boot), da `sleep_minutes`
zur Laufzeit aus dem NVS geladen wird.

### Fehler-Philosophie

```
ADC-Fehler         → nicht fatal (Batteriedaten fehlen im Payload)
BME280-Fehler      → fatal (kein sinnvoller Payload möglich) → Sleep
WiFi-Connect-Fehler → fatal → Sleep
MQTT-Fehler        → nicht fatal für Sleep-Entscheidung (wird trotzdem geschlafen)
```

In allen Fällen endet der Zyklus im Deep Sleep. Ein `panic!()` tritt
in der Firmware nicht auf – alle Fehler werden als `anyhow::Result` propagiert
oder direkt behandelt.

---

## 10. Toolchain & Abhängigkeiten

### Rust-Toolchain

| Komponente | Version | Quelle |
|---|---|---|
| Rust | `esp` (Xtensa-Fork, nightly-basiert) | `espup` |
| rustc | 1.92.0-nightly | `~/.rustup/toolchains/esp` |
| Cargo-Target | `xtensa-esp32-espidf` | `build-std` |

### Crates

| Crate | Version | Zweck |
|---|---|---|
| `esp-idf-sys` | 0.36 | Bindgen-Bindings zu ESP-IDF |
| `esp-idf-hal` | 0.45 | I²C, ADC, GPIO, Delay, Peripherals |
| `esp-idf-svc` | 0.51 | WiFi, MQTT, NVS, HTTP-Server |
| `embedded-svc` | 0.28 | Traits für HTTP-Handler |
| `anyhow` | 1 | Fehlerbehandlung |
| `log` | 0.4 | Logging-Facade |
| `heapless` | 0.8 | Stack-allokierte Strings (SSID, AP-SSID) |
| `embuild` | 0.33 | Build-Dependency: ESP-IDF-Download |

### ESP-IDF

| Komponente | Version | Verwaltet durch |
|---|---|---|
| ESP-IDF | v5.3.2 | `embuild` (automatischer Download) |

> esp-idf-hal 0.45.x ist **inkompatibel** mit IDF 5.5.x (geänderte Structs in
> `twai` und `sdmmc`). Die lokal installierte IDF v5.5.2 unter `~/esp/esp-idf`
> wird deshalb nicht verwendet; `.cargo/config.toml` setzt `ESP_IDF_VERSION = v5.3.2`.

### Werkzeuge

| Werkzeug | Version | Zweck |
|---|---|---|
| `espup` | 0.16.0 | Xtensa-Toolchain-Installer |
| `ldproxy` | 0.3.4 | Linker-Proxy für IDF |
| `espflash` | 4.3.0 | Flashen + serieller Monitor |

---

## 11. Bekannte Einschränkungen

**ADC-API versionssensitiv**
Die ADC-Oneshot-API (`esp_idf_hal::adc::oneshot`) wurde in esp-idf-hal 0.44/0.45
für IDF 5.x eingeführt. Die exakte Signatur von `AdcChannelConfig`, `AdcDriver::new`
und `ch.read()` kann sich zwischen Patch-Versionen unterscheiden.

**ADC-Pin-Match notwendig**
In `main.rs` wird der ADC-Pin per `match cfg.adc_pin { 32 => ..., 33 => ..., ... }`
ausgewählt. Das ist kein Designfehler: Peripheral-Tokens sind in esp-idf-hal
Zero-Sized-Types mit je eigenem Compile-Zeit-Typ – dynamische Dispatch ohne
`unsafe` ist nicht möglich. Jeder Arm enthält identischen Code, nur der Typ
des Pin-Arguments unterscheidet sich.

**WiFi-Verbindungs-Timeout**
`BlockingWifi::connect()` hat keinen konfigurierbaren Timeout auf Rust-Ebene.
Das interne IDF-Timeout gilt. Falls das WLAN nicht erreichbar ist, kann
der Aufruf sehr lange blockieren, bevor er einen Fehler zurückgibt.

**Watchdog im WiFi-Polling-Loop**
Der Polling-Loop in `connect_wifi()` schläft 250 ms pro Iteration; der FreeRTOS-
Task-Watchdog (Standard 30 s) wird dadurch während des 50-s-Timeouts einmal
gerissen. Eine Anmerkung im Code erinnert daran, den Watchdog anzupassen,
falls der Timeout jemals über 30 s angehoben wird.

**MQTT-Verbindungs-Thread**
Der `EspMqttConnection`-Iterator läuft in einem eigenen Thread mit 4 kB Stack.
Falls komplexere Event-Behandlung nötig ist (z. B. Subscribe/Receive),
muss der Stack-Size-Wert in `network.rs` angepasst werden.

**Keine Verschlüsselung des AP-Portals**
Das Konfigurationsportal läuft über HTTP (kein HTTPS) auf einem offenen WLAN.
Zugangsdaten werden im Klartext übertragen. Für sicherheitskritische
Umgebungen müsste ein WPA2-gesicherter AP und TLS verwendet werden.

**Keine OTA-Updates**
Firmware-Updates erfordern aktuell physischen Zugang (espflash über USB).

**URL-Decode** (`ap_mode.rs`) sammelt Bytes und dekodiert via
`String::from_utf8_lossy()`, sodass multi-byte UTF-8-Sequenzen (z. B. Umlaute
in WLAN-Passwörtern) korrekt verarbeitet werden.
