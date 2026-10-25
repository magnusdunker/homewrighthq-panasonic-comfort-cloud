# Panasonic Comfort Cloud for HomewrightHQ

An external integration for [HomewrightHQ](https://github.com/magnusdunker/homewrighthq) for Panasonic air conditioners and air-to-air heat pumps through Panasonic's Comfort Cloud. Aquarea (air-to-water) and hot water tanks use other endpoints and are not included.

## Installation

In HomewrightHQ: **Settings → Integrations → From GitHub**. Paste

```
https://github.com/magnusdunker/homewrighthq-panasonic-comfort-cloud
```

and press **Install**. Then add **Panasonic Comfort Cloud** under **Add integration**, and log in with the Panasonic ID you use in the Comfort Cloud app.

## What you get

Each unit becomes a device in HomewrightHQ with:

- a climate entity with modes (auto, heat, cool, dry, fan), temperature, fan speed and the presets boost and quiet
- `heat_8_15` on units with "+8/15 °C heat". A target temperature below 16 °C turns it on, as in the app
- indoor temperature and outdoor temperature
- vertical and horizontal airflow direction, where the unit has horizontal
- nanoe, where the unit has it
- energy consumption today

The state is fetched every two minutes, the energy every fifteen minutes. After a command the integration waits 20 seconds before reading again, because the cloud can briefly answer with the old values.

The login session is stored in the integration's data folder, so a restart does not need a new login. If Panasonic rejects the password, or new terms have to be accepted in the app, HomewrightHQ shows it under repairs, and the integration can be reauthenticated with **Log in again**. Accounts with two-factor authentication are not supported yet.

## Relation to the built-in integration

HomewrightHQ also has Panasonic Comfort Cloud built in (`panasonic_cc`). The external integration has its own domain, `panasonic_comfort_cloud`, so the two can be installed at the same time. The code is the same, but the external one runs as its own program and is updated independently of HomewrightHQ.

If you switch from the built-in one, the integration has to be set up again. Devices and entities get new IDs, so automations and dashboards that use the old ones have to be updated. Delete the built-in setup afterwards, so the units are not controlled from two places.

## Development

Built in Rust on the `thehome-plugin` SDK from [thehome-example-integration-rust](https://github.com/magnusdunker/homewrighthq-example-integration-rust).

- `src/api.rs`: login with Panasonic ID and the Comfort Cloud API
- `src/model.rs`: Comfort Cloud's data and how it becomes states and commands
- `src/lib.rs`: the connection to HomewrightHQ
- `tests/`: tests against a fake Panasonic cloud, and a fake HomewrightHQ in `tests/host`

```
cargo test
cargo build --release
```

A new version is released with a tag, e.g. `git tag v0.2.0 && git push origin v0.2.0`. The workflow builds for Windows, Linux (x86_64 and arm64, static musl) and macOS and puts the binaries on the release.

## License

MIT
