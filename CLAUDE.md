# CLAUDE.md – ESP32 WeatherNode

Kontext für KI-Assistenten und neue Entwickler.
Vollständige Dokumentation: `DOKU.md` · Benutzerguide: `README.md`

---

## Projektüberblick

Rust-Firmware für einen batteriebetriebenen Wetterknoten (ESP32).
Liest BME280 (Temp/Druck/Feuchte) + Batterie-ADC, sendet JSON via MQTT,
schläft dann im Deep Sleep. Erste-Boot-Konfiguration per WiFi-AP-Portal.
Ablösung einer früheren Arduino-Implementierung (`Arduino/`, read-only).

---

## Architektur

```
main()
  ├─ NVS leer / Fehler  →  ap_mode::run()          (läuft bis esp_restart)
  └─ Config geladen     →  run_measurement_mode()  (-> !, endet in go_to_sleep)

run_measurement_mode():
  Batterie-ADC → BME280 → WiFi → MQTT publish → drop(wifi) → Deep Sleep
```

**Module:**

| Datei | Inhalt |
|---|---|
| `src/main.rs` | Boot, Dispatcher, `run_measurement_mode`, `go_to_sleep` |
| `src/config.rs` | `Config`-Struct, NVS-Persistenz (`wnode_cfg`) |
| `src/ap_mode.rs` | WiFi-AP (APSTA), HTTP-Portal, Test-Endpoints |
| `src/sensor.rs` | BME280-Treiber (I²C, kein externer Crate), ADC-Formel |
| `src/network.rs` | WiFi-STA, MQTT publish (QoS 1 + PUBACK-Wait) |

---

## Toolchain – kritische Constraints

| Komponente | Version | Warum fixiert |
|---|---|---|
| ESP-IDF | **v5.3.2** | esp-idf-hal 0.45 inkompatibel mit IDF 5.5.x (twai/sdmmc) |
| esp-idf-hal | 0.45 | Oneshot-ADC-API, `I2cDriver`-Signatur |
| esp-idf-svc | 0.51 | MQTT-API (`EspMqttClient::new` → `(client, conn)`) |
| Rust toolchain | `esp` (Xtensa-Fork) | Xtensa-Targets (`xtensa-esp32-espidf`) |
| Rust edition | 2021 | Konservativ für Xtensa-Toolchain |

> **Nicht** die lokale IDF v5.5.2 unter `~/esp/esp-idf` verwenden –
> `.cargo/config.toml` setzt `ESP_IDF_VERSION = v5.3.2` (embuild lädt automatisch).

**Arch Linux:** System-Clang statt esp-Clang (SONAME-Problem mit libxml2 ≥ 2.12).
`LIBCLANG_PATH` und `CLANG_PATH` sind in `.cargo/config.toml` gesetzt. `clang` muss installiert sein.

---

## Build & Flash

```bash
source ~/export-esp.sh      # Xtensa-Umgebung laden (einmalig pro Shell)
cargo build                 # Debug
cargo build --release       # Release (empfohlen für Produktion)
cargo run                   # Flash + serieller Monitor (espflash)
espflash erase-flash        # NVS löschen → Werksreset
```

Erster Build lädt ESP-IDF v5.3.2 automatisch herunter (~mehrere Minuten).

---

## Bekannte API-Gotchas

- **MQTT**: `EspMqttClient::new(url, conf)` → `(client, conn)`.
  `conn` muss in einem Hintergrund-Thread kontinuierlich geleert werden.
  `EventPayload::Connected(_)` und `EventPayload::Published(_)` sind die Schlüssel-Events.
  Nach `publish()` wird auf PUBACK gewartet (Timeout 5 s, QoS 1).

- **BME280**: `read_bme280` nimmt `&Bme280Config`, nicht `&Config` – `sensor.rs`
  hat damit keine Abhängigkeit zur Konfigurationsschicht. Aufruf:
  ```rust
  let bme_cfg = sensor::Bme280Config { addr: cfg.bme280_addr,
      send_temperature: cfg.send_temperature, send_pressure: cfg.send_pressure,
      send_humidity: cfg.send_humidity };
  ```

- **ADC**: Oneshot-API – `AdcDriver::new(p.adc1)`, dann `AdcChannelDriver::new(&adc, pin, &cfg)`.
  Dämpfung `DB_11` für 0–3,9 V Eingangsbereich. Nur GPIO 32–39 sind ADC1-fähig.

- **WiFi AP**: `None::<EspDefaultNvsPartition>` als NVS-Argument (AP braucht kein NVS).
  APSTA-Modus: STA bleibt idle bis `/test_wifi` aufgerufen wird – AP ist immer erreichbar.

- **HTTP-Handler**: Rückgabetyp erfordert explizite Typ-Annotation:
  `Ok::<(), anyhow::Error>(())`. `embedded_svc::io::Write as _` muss importiert sein.

- **heapless::String<32>** für AP-SSID in `AccessPointConfiguration`.

- **`esp_idf_sys as _`** am Crate-Root sichert den IDF-Startup-Linkage.

- **ADC-Pin-Match**: Der `match cfg.adc_pin { 32 => ..., 33 => ... }` in `main.rs`
  ist kein Designproblem – Peripheral-Tokens sind ZSTs mit eigenem Compile-Zeit-Typ,
  dynamische Dispatch ohne `unsafe` unmöglich.

- **`url_decode`**: Bytes werden gesammelt und via `String::from_utf8_lossy()` dekodiert
  – UTF-8-Sequenzen (Umlaute in Passwörtern) werden korrekt verarbeitet.

- **`json_escape`** (`network.rs`): `device_name` und `room` werden vor der Einbettung
  in den JSON-Payload escaped (`\` → `\\`, `"` → `\"`). Verhindert JSON-Injection.

- **I²C-Pin-Validierung** (`main.rs`): `sda_pin` und `scl_pin` werden vor `AnyIOPin::new`
  geprüft. GPIO > 33 → `go_to_sleep()` (34–39 sind input-only auf dem ESP32).

- **NVS-Lesepuffer**: 256 Byte (vormals 128) – ausreichend für lange Passwörter/URLs.

- **Tests**: `sensor.rs` und `network.rs` haben je ein `#[cfg(test)]`-Modul mit Unit-Tests
  für reine Logik (`bat_voltage_from_raw`, `round2`, `json_escape`, `build_payload`).
  `ap_mode.rs` testet `url_decode` und `parse_kv`.

---

## NVS-Schema

Namespace: `wnode_cfg`

| Schlüssel | Feld | Typ |
|---|---|---|
| `device_name` | `device_name` | String |
| `room` | `room` | String |
| `wifi_ssid` | `wifi_ssid` | String (Erstboot-Erkennung) |
| `wifi_password` | `wifi_password` | String |
| `mqtt_server` | `mqtt_server` | String |
| `mqtt_port` | `mqtt_port` | String (u16) |
| `mqtt_user` | `mqtt_user` | String |
| `mqtt_password` | `mqtt_password` | String |
| `mqtt_topic` | `mqtt_topic` | String |
| `sleep_mins` | `sleep_minutes` | String (u32) |
| `sda_pin` | `sda_pin` | String (u8) |
| `scl_pin` | `scl_pin` | String (u8) |
| `adc_pin` | `adc_pin` | String (u8) |
| `send_temp` | `send_temperature` | `"1"`/`"0"` |
| `send_pres` | `send_pressure` | `"1"`/`"0"` |
| `send_humi` | `send_humidity` | `"1"`/`"0"` |
| `send_bat` | `send_battery` | `"1"`/`"0"` |

Fehlender `send_*`-Schlüssel → `true` (alle Sensoren standardmäßig aktiv).
Erstboot-Erkennung: `wifi_ssid` fehlt oder leer → AP-Portal.

---

## MQTT-Payload

```json
{"ESPName":"knoten-1","room":"keller","temp":21.35,"pres":1013.47,"humi":54.20,"batvoltvin":3.84}
```

- `ESPName` immer vorhanden; `room` nur wenn nicht leer
- String-Felder werden JSON-escaped (`json_escape` in `network.rs`)
- Alle Zahlenwerte auf 2 Dezimalstellen gerundet
- QoS 1 (AtLeastOnce), kein Retain

---

## AP-Portal-Endpunkte

| Methode | Pfad | Funktion |
|---|---|---|
| GET | `/` | HTML-Konfigurationsformular |
| POST | `/save` | Config in NVS schreiben + `esp_restart()` |
| POST | `/test_wifi` | STA-Verbindung testen → `{"ok":bool,"msg":"..."}` |
| POST | `/test_mqtt` | MQTT-Broker testen → `{"ok":bool,"msg":"..."}` |
| POST | `/sensor` | BME280 live lesen → `{"ok":bool,"temp":"...","pres":"...","humi":"..."}` |

Portal-IP: `192.168.71.1` · SSID: `weathernode-<chipid>` (offen, kein Passwort)

---

## Fehlerbehandlung

| Fehler | Verhalten |
|---|---|
| Batterie-ADC | nicht fatal – `bat_v = None`, weiter |
| I²C-Pin ungültig (GPIO > 33) | fatal → `go_to_sleep()` |
| BME280 | fatal → `go_to_sleep()` |
| WiFi-Connect | fatal → `go_to_sleep()` |
| MQTT publish | nicht fatal für Sleep – trotzdem geschlafen |

Kein `panic!()` in der Firmware. Alle Pfade enden im Deep Sleep (`-> !`).

---

## Offene Punkte / mögliche nächste Schritte

- OTA-Updates (aktuell nur physisch per USB/espflash)
- HTTPS/WPA2 für das AP-Portal (aktuell HTTP + offenes WLAN)
- Konfigurierbarer WiFi-Connect-Timeout (aktuell 50 s hardcoded)
- Mehrere Knoten: `room`-Feld im Payload bereits vorhanden
