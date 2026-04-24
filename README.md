# ESP32 WeatherNode

Firmware für einen batteriebetriebenen Wetterknoten auf Basis des ESP32.
Liest Temperatur, Luftdruck und Luftfeuchtigkeit per BME280 sowie die Batteriespannung,
und sendet die Daten per MQTT an einen Broker.
Zwischen den Messungen schläft der Chip im Deep-Sleep (Standard: 5 Minuten).

Geschrieben in Rust mit dem esp-idf-svc-Stack.
Abgelöst eine frühere Arduino-Implementierung (`Arduino/`).

---

## Hardware

| Bauteil | Detail |
|---|---|
| Mikrocontroller | ESP32 (Xtensa LX6, getestet: ESP32-DevKitC) |
| Umgebungssensor | BME280, I²C-Adresse 0x76 |
| I²C SDA | GPIO 21 |
| I²C SCL | GPIO 22 |
| Batterie-ADC | GPIO 33 (ADC1 Kanal 5) |
| Spannungsteiler | 127 kΩ / 100 kΩ (Eingang → ADC) |

---

## Voraussetzungen

### Rust-Toolchain

```bash
# Xtensa-fähige Rust-Toolchain installieren
cargo install espup
espup install

# Umgebungsvariablen laden (einmalig pro Shell-Session)
source ~/export-esp.sh
```

### Flash-Werkzeuge

```bash
cargo install ldproxy
cargo install espflash
```

> `~/.cargo/bin` muss im `PATH` sein. In der Regel in `~/.bashrc` eintragen:
> ```bash
> export PATH="$HOME/.cargo/bin:$PATH"
> ```

### Systemabhängigkeiten (Arch Linux)

Auf Arch Linux wird das mitgelieferte esp-clang wegen einer SONAME-Änderung in
libxml2 2.12+ nicht funktionieren. Das Projekt ist bereits so konfiguriert, dass
stattdessen der System-Clang verwendet wird (`LIBCLANG_PATH`, `CLANG_PATH` in
`.cargo/config.toml`). Clang muss installiert sein:

```bash
sudo pacman -S clang
```

---

## Bauen & Flashen

```bash
# Debug-Build + direkt flashen + seriellen Monitor öffnen
cargo run

# Nur bauen
cargo build

# Release-Build (kleinere Binärgröße, empfohlen für Produktion)
cargo build --release
```

Der erste Build lädt automatisch ESP-IDF v5.3.2 herunter (via `embuild`).
Das dauert beim ersten Mal mehrere Minuten.

---

## Erstkonfiguration (AP-Portal)

Beim ersten Start – oder wenn keine Konfiguration im NVS gespeichert ist –
öffnet der ESP32 einen eigenen WLAN-Accesspoint:

```
SSID: weathernode-<CHIPID>   (z. B. weathernode-a1b2c3)
Passwort: keines (offen)
```

Verbinde dein Gerät mit diesem WLAN und öffne im Browser:

```
http://192.168.71.1
```

Folgende Felder können konfiguriert werden:

| Feld | Beschreibung | Standard |
|---|---|---|
| Gerätename | MQTT-Client-ID, erscheint im Payload als `ESPName` | `weathernode` |
| Raum | optionaler Raumname, erscheint im Payload als `room` | – |
| WLAN SSID | Name des Heim-WLANs | – |
| WLAN Passwort | WPA2-Passwort | – |
| MQTT Server | IP-Adresse oder Hostname des Brokers | – |
| MQTT Port | Port des Brokers | `1883` |
| MQTT Benutzer | optional | – |
| MQTT Passwort | optional | – |
| MQTT Topic | Topic zum Publishen | `/data/nodes` |
| Sleep-Intervall | Schlafzeit zwischen Messungen in Minuten | `5` |
| Temperatur / Luftdruck / Luftfeuchtigkeit / Batterie | Welche Werte übertragen werden | alle an |

Nach dem Speichern startet der ESP32 neu und verbindet sich mit dem konfigurierten WLAN.

> Um die Konfiguration zurückzusetzen, NVS-Partition löschen:
> ```bash
> espflash erase-flash
> ```

---

## MQTT-Payload

```json
{"ESPName":"weathernode","room":"keller","temp":21.35,"pres":1013.47,"humi":54.20,"batvoltvin":3.84}
```

Nur aktivierte Felder werden gesendet. `ESPName` ist immer enthalten.
`room` wird nur gesendet wenn ein Raumname konfiguriert ist.
Alle Zahlenwerte sind auf 2 Dezimalstellen gerundet.

| JSON-Feld | Einheit | Quelle |
|---|---|---|
| `ESPName` | – | Gerätename aus Konfiguration |
| `room` | – | Raumname aus Konfiguration (optional) |
| `temp` | °C | BME280 |
| `pres` | hPa | BME280 |
| `humi` | %RH | BME280 |
| `batvoltvin` | V | ADC GPIO 33 + Spannungsteiler |

---

## Normaler Betriebszyklus

```
Aufwachen (Power-On oder Timer-Wakeup)
  │
  ├─ Konfiguration aus NVS laden
  │    ├─ Keine Konfig → AP-Portal (läuft bis Neustart)
  │    └─ Konfig vorhanden ↓
  │
  ├─ Batterie-ADC auslesen (wenn aktiviert)
  ├─ BME280 auslesen (nur aktivierte Kanäle)
  ├─ WiFi verbinden
  ├─ MQTT publish
  ├─ WiFi trennen
  └─ Deep Sleep (konfigurierte Dauer)
       → Wakeup durch Timer
```

Bei Fehlern (BME280 nicht gefunden, WiFi-Timeout, MQTT-Fehler) wird
ebenfalls in den Deep Sleep gegangen, sodass der nächste Versuch
nach dem konfigurierten Intervall erfolgt.
