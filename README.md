# Panasonic Comfort Cloud til HomewrightHQ

En ekstern integration til [HomewrightHQ](https://github.com/magnusdunker/homewrighthq) for Panasonic-aircondition og luft-til-luft-varmepumper gennem Panasonics Comfort Cloud. Aquarea (luft-til-vand) og varmtvandsbeholdere bruger andre endpoints og er ikke med.

## Installation

I HomewrightHQ: **Indstillinger → Integrationer → Fra GitHub**. Indsæt

```
https://github.com/magnusdunker/homewrighthq-panasonic-comfort-cloud
```

og tryk **Installér**. Tilføj derefter **Panasonic Comfort Cloud** under **Tilføj integration**, og log ind med det Panasonic ID, du bruger i Comfort Cloud-appen.

## Hvad du får

Hver enhed bliver en enhed i HomewrightHQ med:

- et klimaanlæg med tilstande (auto, varme, køl, affugt, blæser), temperatur, blæserhastighed og forindstillingerne boost og quiet
- `heat_8_15` på enheder med "+8/15 °C heat". En måltemperatur under 16 °C slår den til, ligesom i appen
- indetemperatur og udetemperatur
- lodret og vandret luftretning, hvor enheden har vandret
- nanoe, hvor enheden har det
- energiforbrug i dag

Tilstanden hentes hvert andet minut, energien hvert kvarter. Efter en kommando venter integrationen 20 sekunder, før den læser igen, fordi skyen et øjeblik kan svare med de gamle værdier.

Login-sessionen gemmes i integrationens datamappe, så en genstart ikke kræver et nyt login. Afviser Panasonic adgangskoden, eller skal nye vilkår accepteres i appen, viser HomewrightHQ det under problemer, og integrationen kan godkendes igen med **Log ind igen**. Konti med totrinsbekræftelse understøttes ikke endnu.

## Forholdet til den indbyggede integration

HomewrightHQ har også Panasonic Comfort Cloud indbygget (`panasonic_cc`). Den eksterne integration har sit eget domæne, `panasonic_comfort_cloud`, så de to kan være installeret samtidig. Koden er den samme, men den eksterne kører som sit eget program og opdateres uafhængigt af HomewrightHQ.

Skifter du fra den indbyggede, skal integrationen sættes op igen. Enheder og entities får nye id'er, så automationer og dashboards, der bruger de gamle, skal rettes. Slet den indbyggede opsætning bagefter, så enhederne ikke styres fra to steder.

## Udvikling

Bygget i Rust på SDK'en `thehome-plugin` fra [thehome-example-integration-rust](https://github.com/magnusdunker/homewrighthq-example-integration-rust).

- `src/api.rs`: login med Panasonic ID og Comfort Cloud-API'et
- `src/model.rs`: Comfort Clouds data og hvordan de bliver til tilstande og kommandoer
- `src/lib.rs`: forbindelsen til HomewrightHQ
- `tests/`: tests mod en falsk Panasonic-sky, og en falsk HomewrightHQ i `tests/host`

```
cargo test
cargo build --release
```

En ny version udgives med et tag, f.eks. `git tag v0.2.0 && git push origin v0.2.0`. Workflowet bygger til Windows, Linux (x86_64 og arm64, statisk musl) og macOS og lægger binærerne på releasen.

## Licens

MIT
