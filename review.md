# Code Review – Findings (commit `2af171c`, HEAD~1..HEAD)

Review-Scope: `git diff HEAD~1 HEAD` (src/ap_mode.rs, src/config.rs, src/main.rs,
src/network.rs, src/sensor.rs) – fügt `/save_wifi` (Verify-before-Save), `/test_bat`,
konfigurierbares MQTT-QoS und `/config` (Formular-Vorbefüllung) hinzu.

Priorität-Skala:
- **P0** – Datenverlust / Blocker, muss vor Release behoben werden
- **P1** – Bricht eine neue Kernfunktion für einen relevanten Nutzerkreis, kein Workaround
- **P2** – Realer Bug mit engerem Auslöse-Fenster, aber reproduzierbar
- **P3** – Edge Case / Doku-Drift, Verhaltensänderung ohne Absicherung
- **P4** – Kosmetisch / UX, keine Fehlfunktion der Kernlogik
- **P5** – Cleanup / Wartbarkeit, kein aktueller Bug

---

## P0 – NVS-Lock schützt nur Schreibzugriffe, nicht Lesezugriffe → möglicher Config-Totalverlust

**Datei:** `src/config.rs:189` (NVS_LOCK), `src/config.rs:220` (`load_from_nvs`, ungeschützt)

`NVS_LOCK` wird ausschließlich in `save_to_nvs` gehalten. `load_from_nvs` ruft
`open_nvs()` (→ `EspDefaultNvsPartition::take()`) direkt auf, ohne das Lock zu
nehmen – obwohl der Kommentar über `NVS_LOCK` selbst festhält, dass ein zweites
`take()` fehlschlägt, solange ein erstes Handle noch lebt.

**Failure Scenario:** Die Polling-Loop in `ap_mode.rs` (Zeilen 305–327) hält beim
Schreiben (`save_to_nvs`, ~17 `set_str`-Aufrufe = 17 Flash-Writes) ein `EspNvs`-
Handle und `NVS_LOCK`. Trifft währenddessen ein HTTP-Request ein, der
`config::load_from_nvs()` ungeschützt aufruft (`GET /config` Zeile 93, oder
`POST /save_wifi` Zeile 172/191), schlägt `take()` fehl. Der Fehler wird in
`/save_wifi` durch `.ok().flatten()` verschluckt → `cfg` fällt auf
`Config::default()` zurück, und `/save_wifi` persistiert diesen abgespeckten
Default (nur `wifi_ssid`/`wifi_password` bleiben erhalten) – `mqtt_server`,
`room`, `device_name`, Sensor-Pins, `mqtt_qos` etc. gehen kommentarlos verloren.

**Empfehlung:** Lock in `open_nvs()` selbst verschieben (oder ein Guard
zurückgeben), sodass jeder Aufrufer – lesend wie schreibend – denselben Mutex
nimmt.

---

## P1 – `/save_wifi`-Verify nutzt falsche Auth-Methode → WPA3-Nutzer können WLAN nicht mehr konfigurieren

**Datei:** `src/ap_mode.rs:415` (`test_wifi_connection`) vs. `src/network.rs:38`
(`connect_wifi`)

`connect_wifi` (echter Boot-Pfad) setzt explizit
`auth_method: AuthMethod::WPA2WPA3Personal`. `test_wifi_connection` (genutzt von
`/test_wifi` **und** dem neuen `/save_wifi`) baut weiterhin
`ClientConfiguration { ssid, password, ..Default::default() }` ohne
`auth_method`-Override.

**Failure Scenario:** Nutzer mit korrektem Passwort an einem WPA3-only-AP klickt
„Verbindung testen“ / „Speichern & weiter“. `test_wifi_connection` verwendet die
Default-Auth-Methode, der Verbindungsversuch läuft in den 15s-Timeout
(„Passwort falsch“), `/save_wifi` verweigert das Speichern funktionierender
Zugangsdaten. Da laut Kommentar `/save_wifi` „the ONLY path that may update
wifi_ssid/wifi_password“ ist (sowohl `/update` als auch die `/save`-Polling-Loop
überschreiben übermittelte WLAN-Felder wieder mit dem gespeicherten Wert), gibt
es für WPA3-only-Nutzer **keinen** Weg mehr, das Gerät über das Portal zu
konfigurieren.

**Empfehlung:** `AuthMethod::WPA2WPA3Personal` auch in `test_wifi_connection`
setzen.

---

## P1 – Haupt-Formular überschreibt WLAN-Zugangsdaten kommentarlos beim Speichern

**Datei:** `src/ap_mode.rs:1077` (Formularfelder `wifi_ssid`/`wifi_password`,
weiterhin aktiv/nicht disabled) vs. `src/ap_mode.rs:311-317` (Polling-Loop-Restore)

Das Hauptformular (`action="/save"`) enthält weiterhin editierbare
`wifi_ssid`/`wifi_password`-Felder. Die `/save`-Polling-Loop überschreibt
`cfg.wifi_ssid`/`wifi_password` aber grundsätzlich mit dem zuletzt in NVS
gespeicherten Wert, bevor sie validiert und schreibt.

**Failure Scenario:** Ein Gerät hat bereits eine gespeicherte WLAN-Config.
Nutzer trägt im Hauptformular eine neue SSID/Passwort ein (Felder sind – anders
als z. B. das ADC-Pin-Feld bei deaktivierter Batterie – nicht disabled) und
klickt „Speichern & Neustart“ (→ `/save`). Die Polling-Loop verwirft die Eingabe
stillschweigend, das Gerät startet neu und verbindet sich mit dem **alten**
Netzwerk – ohne jede Fehlermeldung.

**Empfehlung:** Felder im Hauptformular entweder entfernen/deaktivieren und auf
`/save_wifi` verweisen, oder `/save`/`/update` explizit informieren, wenn
übermittelte WLAN-Daten verworfen wurden.

---

## P1 – Test-Assertion `html_escape_entities` stimmt nicht mit der Implementierung überein

**Datei:** `src/ap_mode.rs:1282`

`html_escape(s)` führt `s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;")`
aus. Für `<b>&"x"</b>` ist das tatsächliche Ergebnis
`&lt;b&gt;&amp;"x"&lt;/b&gt;` (beide `>` werden zu `&gt;`). Der Test erwartet
jedoch `&lt;b>&amp;"x"&lt;/b>` (rohes, unescaped `>`).

**Failure Scenario:** `cargo test` schlägt bei dieser Assertion fehl – der Test
widerspricht der eigenen Implementierung.

**Empfehlung:** Erwarteten Wert im Test auf `&lt;b&gt;&amp;"x"&lt;/b&gt;`
korrigieren.

---

## P2 – `json_str` (ap_mode.rs) escaped keine Steuerzeichen → `/config`-Response kann ungültiges JSON werden

**Datei:** `src/ap_mode.rs:746` (`json_str`) vs. `src/network.rs:200`
(`json_escape`, RFC-8259-vollständig, im selben Diff hinzugefügt)

`json_str` escaped nur `"`, `\`, `\n`, `\r`, `\t`; alle anderen Kontrollzeichen
(U+0000–U+001F) werden unverändert durchgereicht. `json_escape` in
`network.rs` wurde in genau diesem Diff um exakt diese Fälle erweitert – die
Schwester-Funktion in `ap_mode.rs` wurde dabei nicht nachgezogen.

**Failure Scenario:** `device_name` (oder `room`/`wifi_ssid`/`mqtt_server`/
`mqtt_user`/`mqtt_topic`) enthält ein rohes Steuerzeichen, z. B. via
`url_decode("%01")` (keine Zeichenklassen-Prüfung in `parse_form` oder
`Config::validate()`). Beim nächsten Laden der Portal-Seite bettet
`GET /config` dieses Byte unescaped in ein JSON-String-Literal ein. Das
Frontend-JS (`fetch('/config').then(r=>r.json())`, Zeile 1003) wirft beim
Parsen, der leere `catch(e){}` (Zeile 1023) verschluckt den Fehler – die
komplette Formular-Vorbefüllung (inkl. `mqtt-warning`-Banner) lädt
kommentarlos nicht.

**Empfehlung:** `json_escape` aus `network.rs` `pub(crate)` machen und
wiederverwenden, oder die Kontrollzeichen-Behandlung in `json_str`
nachziehen.

---

## P2 – `read_body` kappt bei 4096 Byte ohne das Socket zu leeren → möglicher Connection-Desync

**Datei:** `src/ap_mode.rs:800`

Die neue Implementierung bricht das Lesen ab, sobald `MAX_BODY` erreicht ist,
liest aber verbleibende Bytes im Request-Stream nicht mehr aus. Die alte
Implementierung las immer bis `Ok(0) | Err(_)` (EOF).

**Failure Scenario:** Ein POST-Body > 4096 Byte (kein Feld hat ein
`maxlength`; lange `mqtt_topic`/Passwort-Werte mit Prozent-Encoding
können das leicht überschreiten) lässt Restbytes ungelesen im Socket zurück.
Nutzt der ESP-IDF-httpd-Server Keep-Alive für die Verbindung, werden diese
Restbytes als Beginn der nächsten Request-Zeile interpretiert – potenzieller
Parse-Fehler/Hänger beim nächsten Request derselben Verbindung.

**Empfehlung:** Nach Erreichen von `MAX_BODY` weiterhin bis EOF lesen (Ergebnis
verwerfen), statt den Loop sofort zu verlassen.

---

## P2 – DHCP-Poll-Loop bricht bei transientem Fehler sofort ab statt zu retryen

**Datei:** `src/network.rs:64`

Die neue DHCP-Polling-Loop ruft `wifi.wifi().sta_netif().get_ip_info()?` mit
einem nackten `?` **innerhalb** der Retry-Loop auf. Die alte Implementierung
nutzte `wifi.wait_netif_up()` (blockiert intern bis der Netif wirklich bereit
ist) gefolgt von einem einzelnen `get_ip_info()`-Aufruf.

**Failure Scenario:** Tritt direkt nach erfolgreicher Assoziation kurzzeitig ein
`Err` von `get_ip_info()` auf (z. B. Netif-Handle noch nicht vollständig
initialisiert), bricht `connect_wifi` sofort ab, statt die vollen 10s zu
pollen – die Retry-Loop wird durch das `?` faktisch ausgehebelt.

**Empfehlung:** `Err`-Fall im Loop behandeln (loggen + weiter pollen) statt
`?` zu verwenden, oder `wait_netif_up()` beibehalten.

---

## P3 – WLAN-Verbindungstimeout von 50 s auf 15 s reduziert, Doku nicht nachgezogen

**Datei:** `src/network.rs:53` vs. `CLAUDE.md` („Offene Punkte“: „Konfigurierbarer
WiFi-Connect-Timeout (aktuell 50 s hardcoded)“)

**Failure Scenario:** Ein Knoten am Rand der AP-Reichweite, der bisher
innerhalb von z. B. 30–45 s assoziieren konnte (durch das alte 50s-Timeout
toleriert), erreicht jetzt regelmäßig „WiFi connect timeout (15 s)“ und meldet
keine Daten mehr – obwohl sich an Hardware/Entfernung nichts geändert hat.
CLAUDE.md/DOKU.md dokumentieren weiterhin den alten 50s-Wert.

**Empfehlung:** Timeout-Wert und Rationale in CLAUDE.md/DOKU.md aktualisieren,
oder Timeout wieder erhöhen bzw. konfigurierbar machen (war ohnehin als
offener Punkt vermerkt).

---

## P4 – `saveWifi()` blendet MQTT-Warnbanner unabhängig vom tatsächlichen MQTT-Status aus

**Datei:** `src/ap_mode.rs:996`

Bei jedem erfolgreichen `/save_wifi`-Response wird
`document.getElementById('mqtt-warning').style.display='none'` ausgeführt –
unabhängig davon, ob MQTT tatsächlich konfiguriert ist.

**Failure Scenario:** Gerät bootet ins Portal, weil `setup_state()==WifiOnly`
(MQTT noch nicht gesetzt); Banner wird angezeigt. Nutzer bestätigt/speichert
WLAN erneut (z. B. nur um Konnektivität zu prüfen) → Banner verschwindet, obwohl
MQTT weiterhin unkonfiguriert ist. Sieht nach abgeschlossenem Setup aus, obwohl
ein Neustart wieder im Portal landen wird.

**Empfehlung:** Banner-Sichtbarkeit anhand von `setup_state`/`mqtt_server`
neu bewerten statt pauschal auszublenden.

---

## P5 – Zugangsdaten-Restore-Block in `/update` und Polling-Loop dupliziert

**Datei:** `src/ap_mode.rs:267` (`/update`) und `src/ap_mode.rs:311` (Polling-Loop)

Der Block „lade gespeicherte Config, stelle `wifi_ssid`/`wifi_password`
wieder her, übernehme `mqtt_password` nur wenn leer übermittelt“ ist in beiden
Handlern identisch kopiert. Nichts in `Config`/`save_to_nvs` erzwingt dieses
Verhalten strukturell.

**Failure Scenario:** Ein künftiger weiterer Schreib-Pfad (z. B. Bulk-Import,
Firmware-Update-Trigger), der ebenfalls `parse_form()` + `save_to_nvs()`
aufruft, vergisst diesen Block zu kopieren – WLAN-Zugangsdaten würden dann
kommentarlos durch übermittelte (evtl. leere) Formularwerte ersetzt.

**Empfehlung:** Gemeinsame Hilfsfunktion `fn preserve_credentials(cfg: &mut
Config, saved: &Config)` extrahieren und an beiden Stellen nutzen.

---

## Übersicht

| Prio | Kurzbeschreibung | Datei:Zeile |
|---|---|---|
| P0 | NVS-Lock schützt nicht vor Read/Write-Race → Config-Totalverlust möglich | config.rs:189 |
| P1 | `/save_wifi` nutzt falsche Auth-Methode, blockiert WPA3-Nutzer komplett | ap_mode.rs:415 |
| P1 | Hauptformular verwirft neue WLAN-Zugangsdaten kommentarlos | ap_mode.rs:1077 |
| P1 | Test `html_escape_entities` widerspricht eigener Implementierung | ap_mode.rs:1282 |
| P2 | `json_str` escaped keine Steuerzeichen → `/config` kann ungültiges JSON liefern | ap_mode.rs:746 |
| P2 | `read_body` kappt ohne Socket zu leeren → möglicher Connection-Desync | ap_mode.rs:800 |
| P2 | DHCP-Poll-Loop bricht bei transientem Fehler sofort ab | network.rs:64 |
| P3 | WLAN-Timeout 50s→15s reduziert, Doku nicht aktualisiert | network.rs:53 |
| P4 | MQTT-Warnbanner wird unabhängig vom MQTT-Status ausgeblendet | ap_mode.rs:996 |
| P5 | Zugangsdaten-Restore-Logik dupliziert (2 Stellen) | ap_mode.rs:267, 311 |
