//! Panasonic Comfort Cloud: air conditioners and heat pumps (air-to-air)
//! through Panasonic's cloud, as an external integration for HomewrightHQ.
//!
//! The user logs in with their Panasonic ID. Each unit becomes a device with
//! a climate entity, temperature sensors, air direction selects, a nanoe
//! switch where supported and today's energy use. Aquarea (air-to-water) and
//! hot-water tanks use other endpoints and are skipped.
//!
//! `api` and `model` are the same as in HomewrightHQ's built-in `panasonic_cc`;
//! this file connects them to HomewrightHQ through the `thehome-plugin` SDK.

pub mod api;
pub mod model;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thehome_plugin::{async_trait, json, Device, Entity, Entry, Error, Flow, FlowReply, Host, Integration, Map, Value};

use api::{ApiError, Client, Session, Urls};
use model::{DeviceInfo, Features};

pub const DOMAIN: &str = "panasonic_comfort_cloud";
const POLL: Duration = Duration::from_secs(120);
const ENERGY_POLL: Duration = Duration::from_secs(15 * 60);
/// After a command the cloud may still report the old values for a while.
const SETTLE: Duration = Duration::from_secs(20);
/// target temperature, fan mode, preset, turn off, turn on
const CLIMATE_FEATURES: u64 = 1 | 8 | 16 | 128 | 256;

/// Logging to HomewrightHQ's log through stderr.
#[doc(hidden)]
pub fn emit(level: &str, message: String) {
    thehome_plugin::log(level, message);
}

#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => { $crate::emit("DEBUG", format!($($t)*)) };
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::emit("INFO", format!($($t)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::emit("WARN", format!($($t)*)) };
}

fn login_schema() -> Value {
    json!({
        "type": "object",
        "required": ["username", "password"],
        "properties": {
            "username": { "type": "string", "title": "E-mail", "description": "Panasonic ID, som du bruger i Comfort Cloud-appen" },
            "password": { "type": "string", "format": "password", "title": "Adgangskode" }
        }
    })
}

/// Endpoints; tests put mock URLs in the entry or flow data.
fn urls(get: impl Fn(&str) -> Option<String>) -> Urls {
    let d = Urls::default();
    Urls {
        auth: get("auth_url").unwrap_or(d.auth),
        acc: get("acc_url").unwrap_or(d.acc),
        version: get("version_url").unwrap_or(d.version),
    }
}

fn error_code(e: &ApiError) -> &'static str {
    match e {
        ApiError::Auth => "invalid_auth",
        ApiError::Mfa => "mfa_not_supported",
        ApiError::Agreement => "agreement_required",
        ApiError::Other(_) => "cannot_connect",
    }
}

#[derive(Default)]
pub struct Panasonic {
    accounts: Mutex<HashMap<String, Arc<Account>>>,
}

impl Panasonic {
    fn account(&self, entry_id: &str) -> Result<Arc<Account>, Error> {
        self.accounts.lock().unwrap().get(entry_id).cloned().ok_or_else(|| Error::unavailable("opsætningen kører ikke"))
    }
}

/// Where an entry's session is kept between restarts.
fn session_file(host: &Host, entry_id: &str) -> PathBuf {
    host.data_dir().join(format!("session-{entry_id}.json"))
}

#[async_trait]
impl Integration for Panasonic {
    async fn config_flow(&self, _host: &Host, flow: Flow, step: &str, input: Option<Value>) -> Result<FlowReply, Error> {
        match (step, input) {
            ("user", None) | ("reauth", None) => Ok(FlowReply::form("user", login_schema())),
            ("user", Some(v)) => {
                let username = v["username"].as_str().unwrap_or_default().trim().to_string();
                let password = v["password"].as_str().unwrap_or_default().to_string();
                let urls = urls(|k| flow.data.get(k).and_then(Value::as_str).map(String::from));
                let client = Client::new(urls, &username, &password, None, None);
                let devices = match client.groups().await {
                    Ok(g) => model::parse_devices(&g),
                    Err(e) => {
                        info!("{e}");
                        return Ok(FlowReply::form_with_errors("user", login_schema(), json!({ "base": error_code(&e) })));
                    }
                };
                if flow.source == "reauth" {
                    let mut data = flow.entry.as_ref().and_then(|e| e.data.as_object().cloned()).unwrap_or_default();
                    data.insert("username".into(), json!(username));
                    data.insert("password".into(), json!(password));
                    return Ok(FlowReply::create("", Value::Object(data)));
                }
                if devices.is_empty() {
                    return Ok(FlowReply::abort("no_devices_found"));
                }
                let mut data = flow.data.clone();
                data.insert("username".into(), json!(username));
                data.insert("password".into(), json!(password));
                Ok(FlowReply::create(&format!("Comfort Cloud ({username})"), Value::Object(data)).with_unique_id(username.to_lowercase()))
            }
            (s, _) => Err(Error::invalid(format!("ukendt trin {s}"))),
        }
    }

    async fn setup_entry(&self, host: &Host, entry: Entry) -> Result<(), Error> {
        let entry_id = entry.entry_id.clone();
        let username = entry.get_str("username").ok_or_else(|| Error::auth_failed("brugernavn mangler"))?.to_string();
        let password = entry.get_str("password").unwrap_or_default().to_string();
        let urls = urls(|k| entry.get_str(k).map(String::from));

        let file = session_file(host, &entry_id);
        let saved: Option<Session> = std::fs::read_to_string(&file).ok().and_then(|s| serde_json::from_str(&s).ok());
        let last_saved = Arc::new(Mutex::new(saved.clone()));
        let persist: api::Persist = Arc::new(move |s: &Session| {
            let mut last = last_saved.lock().unwrap();
            if last.as_ref() == Some(s) {
                return;
            }
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match serde_json::to_vec(s).map_err(|e| e.to_string()).and_then(|b| std::fs::write(&file, b).map_err(|e| e.to_string())) {
                Ok(()) => *last = Some(s.clone()),
                Err(e) => warn!("kunne ikke gemme session: {e}"),
            }
        });
        let client = Arc::new(Client::new(urls, &username, &password, saved, Some(persist)));

        let groups = match client.groups().await {
            Ok(g) => g,
            Err(ApiError::Auth) => return Err(Error::auth_failed("Panasonic afviste brugernavn eller adgangskode")),
            Err(ApiError::Mfa) => return Err(Error::auth_failed("totrinsbekræftelse understøttes ikke")),
            Err(ApiError::Agreement) => {
                host.create_issue(
                    &entry_id,
                    "agreement",
                    "error",
                    "Comfort Cloud: nye vilkår",
                    "Panasonic har opdateret vilkårene. Åbn Comfort Cloud-appen, accepter dem, og genindlæs integrationen.",
                )
                .await?;
                return Err(Error::not_ready("nye vilkår skal accepteres"));
            }
            Err(e) => return Err(Error::not_ready(e)),
        };
        host.delete_issue(&entry_id, "agreement").await?;
        let devices = model::parse_devices(&groups);

        let account = Arc::new(Account {
            entry_id: entry_id.clone(),
            host: host.clone(),
            client: client.clone(),
            units: Mutex::new(HashMap::new()),
            entities: Mutex::new(vec![]),
            task: Mutex::new(None),
        });
        for info in devices {
            let status = client.status(&info.guid).await.map_err(Error::not_ready)?;
            let features = Features::parse(&status);
            let params = status["parameters"].as_object().cloned().unwrap_or_default();
            account.register(&info, &features).await?;
            account.units.lock().unwrap().insert(
                info.guid.clone(),
                Unit { features, params, energy: None, last_control: None, available: true },
            );
            account.publish(&info.guid).await;
        }

        let acc = account.clone();
        let task = tokio::spawn(async move {
            let mut last_energy: Option<Instant> = None;
            loop {
                let energy_due = last_energy.is_none_or(|t| t.elapsed() >= ENERGY_POLL);
                acc.poll(energy_due).await;
                if energy_due {
                    last_energy = Some(Instant::now());
                }
                tokio::time::sleep(POLL).await;
            }
        });
        *account.task.lock().unwrap() = Some(task);
        if let Some(old) = self.accounts.lock().unwrap().insert(entry_id, account) {
            old.stop();
        }
        Ok(())
    }

    async fn unload_entry(&self, _host: &Host, entry_id: &str) -> Result<(), Error> {
        if let Some(a) = self.accounts.lock().unwrap().remove(entry_id) {
            a.stop();
        }
        Ok(())
    }

    async fn entity_service(&self, _host: &Host, entry_id: &str, entity_id: &str, service: &str, data: Map<String, Value>) -> Result<(), Error> {
        self.account(entry_id)?.call_service(entity_id, service, &data).await
    }

    async fn diagnostics(&self, _host: &Host, entry: Entry) -> Value {
        let mut d = entry.data.clone();
        if let Value::Object(m) = &mut d {
            m.insert("password".into(), json!("**REDACTED**"));
        }
        d
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Climate,
    Inside,
    Outside,
    Vertical,
    Horizontal,
    Nanoe,
    Energy,
}

/// A state and its attributes.
type StateUpdate = (String, Map<String, Value>);

struct Unit {
    features: Features,
    params: Map<String, Value>,
    energy: Option<f64>,
    last_control: Option<Instant>,
    available: bool,
}

struct Account {
    entry_id: String,
    host: Host,
    client: Arc<Client>,
    units: Mutex<HashMap<String, Unit>>,
    /// (entity_id, guid, kind)
    entities: Mutex<Vec<(String, String, Kind)>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Account {
    fn stop(&self) {
        if let Some(t) = self.task.lock().unwrap().take() {
            t.abort();
        }
    }

    async fn register(&self, info: &DeviceInfo, f: &Features) -> Result<(), Error> {
        let host = &self.host;
        let dev = host
            .register_device(
                &self.entry_id,
                Device {
                    identifiers: vec![(DOMAIN.into(), info.guid.clone())],
                    name: info.name.clone(),
                    manufacturer: Some("Panasonic".into()),
                    model: info.model.clone(),
                    ..Default::default()
                },
            )
            .await?;
        let guid = &info.guid;
        let entity = |key: &str, name: Option<&str>, domain: &str| {
            let mut e = Entity::new(domain, if key.is_empty() { guid.clone() } else { format!("{guid}-{key}") }).device(&dev);
            e.name = name.map(String::from);
            e
        };
        let temp = |e: Entity| e.device_class("temperature").unit("°C").state_class("measurement");

        let mut list: Vec<(Kind, Entity, bool)> = vec![
            (
                Kind::Climate,
                entity("", None, "climate").capabilities(json!({
                    "hvac_modes": f.hvac_modes(), "fan_modes": model::options(&model::FAN), "preset_modes": f.preset_modes(),
                    "min_temp": f.min_temp(), "max_temp": model::MAX_TEMP, "target_temp_step": 0.5,
                    "supported_features": CLIMATE_FEATURES })),
                true,
            ),
            (Kind::Inside, temp(entity("inside", Some("Indetemperatur"), "sensor")), false),
            (Kind::Outside, temp(entity("outside", Some("Udetemperatur"), "sensor")), false),
        ];
        let mut ud = vec!["auto".to_string()];
        ud.extend(model::options(&model::SWING_UD));
        list.push((
            Kind::Vertical,
            entity("swing_ud", Some("Lodret luftretning"), "select").icon("mdi:arrow-up-down").capabilities(json!({ "options": ud })),
            true,
        ));
        if f.swing_lr {
            let mut lr = vec!["auto".to_string()];
            lr.extend(model::options(&model::SWING_LR));
            list.push((
                Kind::Horizontal,
                entity("swing_lr", Some("Vandret luftretning"), "select").icon("mdi:arrow-left-right").capabilities(json!({ "options": lr })),
                true,
            ));
        }
        if f.nanoe {
            list.push((Kind::Nanoe, entity("nanoe", Some("nanoe"), "switch").icon("mdi:air-purifier"), true));
        }
        list.push((
            Kind::Energy,
            entity("energy_today", Some("Energi i dag"), "sensor").device_class("energy").unit("kWh").state_class("total_increasing"),
            false,
        ));

        for (kind, e, handler) in list {
            if let Some(id) = host.add_entity(&self.entry_id, e, handler).await? {
                self.entities.lock().unwrap().push((id, guid.clone(), kind));
            }
        }
        Ok(())
    }

    /// Push states for every entity of a unit.
    async fn publish(&self, guid: &str) {
        let (available, updates): (bool, Vec<(String, Option<StateUpdate>)>) = {
            let units = self.units.lock().unwrap();
            let Some(u) = units.get(guid) else { return };
            let p = &u.params;
            let list = self
                .entities
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, g, _)| g == guid)
                .map(|(id, _, kind)| {
                    let value = if !u.available {
                        None
                    } else {
                        let num = |v: Option<f64>| v.map(|t| (t.to_string(), Map::new()));
                        match kind {
                            Kind::Climate => Some(model::climate(p, &u.features)),
                            Kind::Inside => num(model::temperature(p, "insideTemperature")),
                            Kind::Outside => num(model::temperature(p, "outTemperature")),
                            Kind::Vertical => model::vertical(p).map(|s| (s.to_string(), Map::new())),
                            Kind::Horizontal => model::horizontal(p).map(|s| (s.to_string(), Map::new())),
                            Kind::Nanoe => model::nanoe_on(p).map(|on| ((if on { "on" } else { "off" }).to_string(), Map::new())),
                            Kind::Energy => u.energy.map(|e| (format!("{e:.2}"), Map::new())),
                        }
                    };
                    (id.clone(), value)
                })
                .collect();
            (u.available, list)
        };
        for (id, value) in updates {
            let res = match value {
                Some((state, attrs)) => self.host.set_state(&id, state, attrs).await,
                None if !available => self.host.set_unavailable(&id).await,
                None => self.host.set_state(&id, "unknown", Map::new()).await,
            };
            if let Err(e) = res {
                warn!("kunne ikke opdatere {id}: {e}");
            }
        }
    }

    async fn poll(&self, energy: bool) {
        let guids: Vec<String> = self.units.lock().unwrap().keys().cloned().collect();
        let today = chrono::Local::now();
        let day = today.format("%Y%m%d").to_string();
        let tz = today.format("%:z").to_string();
        for guid in guids {
            let settling = self.units.lock().unwrap().get(&guid).and_then(|u| u.last_control).is_some_and(|t| t.elapsed() < SETTLE);
            if !settling {
                match self.client.status(&guid).await {
                    Ok(status) => {
                        let _ = self.host.delete_issue(&self.entry_id, "auth").await;
                        let _ = self.host.delete_issue(&self.entry_id, "agreement").await;
                        if let Some(u) = self.units.lock().unwrap().get_mut(&guid) {
                            u.features = Features::parse(&status);
                            if let Some(p) = status["parameters"].as_object() {
                                u.params = p.clone();
                            }
                            u.available = true;
                        }
                    }
                    Err(e) => {
                        self.report(&e).await;
                        if let Some(u) = self.units.lock().unwrap().get_mut(&guid) {
                            u.available = false;
                        }
                    }
                }
            }
            if energy {
                match self.client.month_history(&guid, &day, &tz).await {
                    Ok(h) => {
                        if let Some(u) = self.units.lock().unwrap().get_mut(&guid) {
                            u.energy = model::consumption_on(&h, &day);
                        }
                    }
                    Err(e) => debug!("energi: {e}"),
                }
            }
            self.publish(&guid).await;
        }
    }

    async fn report(&self, e: &ApiError) {
        let res = match e {
            ApiError::Auth | ApiError::Mfa => {
                self.host
                    .create_issue(
                        &self.entry_id,
                        "auth",
                        "error",
                        "Comfort Cloud: login afvist",
                        "Panasonic afviste brugernavn eller adgangskode. Godkend integrationen igen med de rigtige oplysninger.",
                    )
                    .await
            }
            ApiError::Agreement => {
                self.host
                    .create_issue(
                        &self.entry_id,
                        "agreement",
                        "error",
                        "Comfort Cloud: nye vilkår",
                        "Panasonic har opdateret vilkårene. Åbn Comfort Cloud-appen og accepter dem.",
                    )
                    .await
            }
            ApiError::Other(err) => {
                warn!("{err}");
                Ok(())
            }
        };
        if let Err(e) = res {
            warn!("kunne ikke oprette problem: {e}");
        }
    }

    async fn call_service(&self, entity_id: &str, service: &str, data: &Map<String, Value>) -> Result<(), Error> {
        let (guid, kind) = self
            .entities
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _, _)| id == entity_id)
            .map(|(_, g, k)| (g.clone(), *k))
            .ok_or_else(|| Error::new("not_found", entity_id))?;
        let (params, features) = self
            .units
            .lock()
            .unwrap()
            .get(&guid)
            .map(|u| (u.params.clone(), u.features.clone()))
            .unwrap_or_else(|| (Map::new(), Features::parse(&Value::Null)));
        let option = || data.get("option").and_then(Value::as_str).unwrap_or_default().to_string();
        let changes = match (kind, service) {
            (Kind::Climate, _) => model::climate_change(service, data, &params, &features).map_err(|m| {
                if m.is_empty() {
                    Error::not_supported(format!("climate.{service}"))
                } else {
                    Error::invalid(m)
                }
            })?,
            (Kind::Vertical, "select_option") => {
                model::set_vertical(&params, &option()).ok_or_else(|| Error::invalid(format!("ukendt retning: {}", option())))?
            }
            (Kind::Horizontal, "select_option") => {
                model::set_horizontal(&params, &option()).ok_or_else(|| Error::invalid(format!("ukendt retning: {}", option())))?
            }
            (Kind::Nanoe, "turn_on" | "turn_off") => {
                let mut m = Map::new();
                m.insert("nanoe".into(), json!(if service == "turn_on" { 2 } else { 1 }));
                m
            }
            _ => return Err(Error::not_supported(service)),
        };
        if let Err(e) = self.client.control(&guid, &changes).await {
            self.report(&e).await;
            return Err(Error::failed(e));
        }
        if let Some(u) = self.units.lock().unwrap().get_mut(&guid) {
            u.params.extend(changes);
            u.last_control = Some(Instant::now());
            u.available = true;
        }
        self.publish(&guid).await;
        Ok(())
    }
}
