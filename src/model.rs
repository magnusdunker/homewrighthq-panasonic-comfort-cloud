//! Comfort Cloud device data and how it maps to entity states.

use serde_json::{json, Map, Value};

/// `insideTemperature` and friends use 126 for "no reading".
pub const INVALID_TEMPERATURE: f64 = 126.0;
pub const MIN_TEMP: f64 = 16.0;
pub const MAX_TEMP: f64 = 30.0;
/// Preset for the "+8/15 °C heat" frost guard, named as in Home Assistant.
pub const PRESET_8_15: &str = "heat_8_15";
/// Target temperature when leaving the frost guard.
const LEAVE_8_15_TEMP: f64 = 20.0;

/// Device types that are not air conditioners (Aquarea, hot-water tank).
const OTHER_TYPES: [&str; 2] = ["2", "11"];

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub guid: String,
    pub name: String,
    pub model: Option<String>,
    pub group: Option<String>,
}

/// Air conditioners in a `/device/group` response.
pub fn parse_devices(v: &Value) -> Vec<DeviceInfo> {
    let mut out = vec![];
    for g in v["groupList"].as_array().into_iter().flatten() {
        let list = g.get("deviceList").or_else(|| g.get("deviceIdList"));
        for d in list.and_then(Value::as_array).into_iter().flatten() {
            let Some(guid) = d["deviceGuid"].as_str() else { continue };
            let dtype = match &d["deviceType"] {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            };
            if OTHER_TYPES.contains(&dtype.as_str()) || d.get("parameters").is_none() {
                continue;
            }
            let s = |k: &str| d[k].as_str().filter(|s| !s.is_empty()).map(String::from);
            out.push(DeviceInfo {
                guid: guid.into(),
                name: s("deviceName").unwrap_or_else(|| guid.into()),
                model: s("deviceModuleNumber"),
                group: g["groupName"].as_str().filter(|s| !s.is_empty()).map(String::from),
            });
        }
    }
    out
}

/// What a unit supports, from the top level of a status response.
#[derive(Clone, Debug, PartialEq)]
pub struct Features {
    pub auto: bool,
    pub heat: bool,
    pub cool: bool,
    pub dry: bool,
    pub fan: bool,
    pub nanoe: bool,
    pub powerful: bool,
    pub quiet: bool,
    pub swing_lr: bool,
    /// `summerHouse`: the "+8/15 °C heat" frost guard. 0 = none, 2 = 8-15 °C,
    /// 1 and 3 = 8 or 10 °C only.
    pub summer_house: i64,
}

impl Features {
    pub fn parse(status: &Value) -> Self {
        // Modes default to supported when the field is missing; extras do not.
        let flag = |k: &str, default: bool| status.get(k).and_then(Value::as_bool).unwrap_or(default);
        Self {
            auto: flag("autoMode", true),
            heat: flag("heatMode", true),
            cool: flag("coolMode", true),
            dry: flag("dryMode", true),
            fan: flag("fanMode", true),
            nanoe: flag("nanoe", false),
            powerful: flag("powerfulMode", true),
            quiet: flag("quietMode", true),
            swing_lr: flag("airSwingLR", false),
            summer_house: status.get("summerHouse").and_then(Value::as_i64).unwrap_or(0),
        }
    }

    pub fn hvac_modes(&self) -> Vec<&'static str> {
        let mut v = vec!["off"];
        for (on, m) in [(self.auto, "heat_cool"), (self.heat, "heat"), (self.cool, "cool"), (self.dry, "dry"), (self.fan, "fan_only")] {
            if on {
                v.push(m);
            }
        }
        v
    }

    pub fn preset_modes(&self) -> Vec<&'static str> {
        let mut v = vec!["none"];
        if self.powerful {
            v.push("boost");
        }
        if self.quiet {
            v.push("quiet");
        }
        if self.summer_house > 0 {
            v.push(PRESET_8_15);
        }
        v
    }

    pub fn min_temp(&self) -> f64 {
        if self.summer_house > 0 { 8.0 } else { MIN_TEMP }
    }

    /// A target below 16 °C as the frost guard allows it, or `None` without one.
    fn frost_target(&self, t: f64) -> Option<f64> {
        match self.summer_house {
            2 => Some(t.clamp(8.0, 15.0).round()),
            1 | 3 => Some(if t < 9.0 { 8.0 } else { 10.0 }),
            _ => None,
        }
    }
}

/// Whether the unit runs the frost guard. Like the Comfort Cloud app, this
/// goes by the target temperature, which is below 16 °C only in that mode.
pub fn in_frost_guard(p: &Map<String, Value>, f: &Features) -> bool {
    let Some(t) = p.get("temperatureSet").and_then(Value::as_f64) else { return false };
    match f.summer_house {
        2 => (8.0..=15.0).contains(&t),
        1 | 3 => t == 8.0 || t == 10.0,
        _ => false,
    }
}

/// Parameters that start the frost guard at `t` °C: heat, powerful, fan high.
fn enter_frost_guard(m: &mut Map<String, Value>, t: f64) {
    m.insert("temperatureSet".into(), json!(t));
    m.insert("operationMode".into(), json!(3));
    m.insert("ecoMode".into(), json!(1));
    m.insert("fanSpeed".into(), json!(5));
}

/// Parameters that leave the frost guard for a normal target.
fn leave_frost_guard(m: &mut Map<String, Value>) {
    m.entry("temperatureSet").or_insert(json!(LEAVE_8_15_TEMP));
    m.entry("ecoMode").or_insert(json!(0));
    m.insert("fanSpeed".into(), json!(0));
}

pub const HVAC: [(i64, &str); 5] = [(0, "heat_cool"), (1, "dry"), (2, "cool"), (3, "heat"), (4, "fan_only")];
pub const FAN: [(i64, &str); 6] = [(0, "auto"), (1, "low"), (2, "low_medium"), (3, "medium"), (4, "medium_high"), (5, "high")];
pub const PRESET: [(i64, &str); 3] = [(0, "none"), (1, "boost"), (2, "quiet")];
pub const SWING_UD: [(i64, &str); 6] = [(0, "up"), (3, "up_middle"), (2, "middle"), (4, "down_middle"), (1, "down"), (5, "swing")];
pub const SWING_LR: [(i64, &str); 5] = [(1, "left"), (5, "left_middle"), (2, "middle"), (4, "right_middle"), (0, "right")];

fn name_of(table: &[(i64, &'static str)], code: Option<i64>) -> Option<&'static str> {
    let code = code?;
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

pub fn code_of(table: &[(i64, &str)], name: &str) -> Option<i64> {
    table.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

pub fn options(table: &[(i64, &str)]) -> Vec<String> {
    table.iter().map(|(_, n)| n.to_string()).collect()
}

fn int(p: &Map<String, Value>, k: &str) -> Option<i64> {
    p.get(k).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

pub fn temperature(p: &Map<String, Value>, k: &str) -> Option<f64> {
    p.get(k).and_then(Value::as_f64).filter(|t| (*t - INVALID_TEMPERATURE).abs() > f64::EPSILON && *t > -100.0)
}

pub fn is_on(p: &Map<String, Value>) -> bool {
    int(p, "operate") == Some(1)
}

pub fn hvac_mode(p: &Map<String, Value>) -> &'static str {
    if !is_on(p) {
        return "off";
    }
    name_of(&HVAC, int(p, "operationMode")).unwrap_or("heat_cool")
}

// `fanAutoMode`: 0 = both directions automatic, 1 = none, 2 = up/down, 3 = left/right.
fn ud_auto(p: &Map<String, Value>) -> bool {
    matches!(int(p, "fanAutoMode"), Some(0 | 2))
}

fn lr_auto(p: &Map<String, Value>) -> bool {
    matches!(int(p, "fanAutoMode"), Some(0 | 3))
}

pub fn vertical(p: &Map<String, Value>) -> Option<&'static str> {
    if ud_auto(p) {
        return Some("auto");
    }
    name_of(&SWING_UD, int(p, "airSwingUD"))
}

pub fn horizontal(p: &Map<String, Value>) -> Option<&'static str> {
    if lr_auto(p) {
        return Some("auto");
    }
    name_of(&SWING_LR, int(p, "airSwingLR"))
}

fn fan_auto_code(ud: bool, lr: bool) -> i64 {
    match (ud, lr) {
        (true, true) => 0,
        (true, false) => 2,
        (false, true) => 3,
        (false, false) => 1,
    }
}

/// Parameters for a new vertical direction, keeping the horizontal one.
pub fn set_vertical(p: &Map<String, Value>, option: &str) -> Option<Map<String, Value>> {
    let mut m = Map::new();
    if option == "auto" {
        m.insert("fanAutoMode".into(), json!(fan_auto_code(true, lr_auto(p))));
    } else {
        m.insert("airSwingUD".into(), json!(code_of(&SWING_UD, option)?));
        m.insert("fanAutoMode".into(), json!(fan_auto_code(false, lr_auto(p))));
    }
    Some(m)
}

/// Parameters for a new horizontal direction, keeping the vertical one.
pub fn set_horizontal(p: &Map<String, Value>, option: &str) -> Option<Map<String, Value>> {
    let mut m = Map::new();
    if option == "auto" {
        m.insert("fanAutoMode".into(), json!(fan_auto_code(ud_auto(p), true)));
    } else {
        m.insert("airSwingLR".into(), json!(code_of(&SWING_LR, option)?));
        m.insert("fanAutoMode".into(), json!(fan_auto_code(ud_auto(p), false)));
    }
    Some(m)
}

/// Climate entity state and attributes.
pub fn climate(p: &Map<String, Value>, f: &Features) -> (String, Map<String, Value>) {
    let mut a = Map::new();
    let mode = hvac_mode(p);
    a.insert("hvac_modes".into(), json!(f.hvac_modes()));
    a.insert("hvac_action".into(), json!(match mode {
        "off" => "off",
        "heat" => "heating",
        "cool" => "cooling",
        "dry" => "drying",
        "fan_only" => "fan",
        _ => "idle",
    }));
    a.insert("current_temperature".into(), json!(temperature(p, "insideTemperature")));
    a.insert("temperature".into(), json!(temperature(p, "temperatureSet")));
    a.insert("min_temp".into(), json!(f.min_temp()));
    a.insert("max_temp".into(), json!(MAX_TEMP));
    a.insert("target_temp_step".into(), json!(0.5));
    a.insert("fan_modes".into(), json!(options(&FAN)));
    a.insert("fan_mode".into(), json!(name_of(&FAN, int(p, "fanSpeed"))));
    a.insert("preset_modes".into(), json!(f.preset_modes()));
    let preset = if in_frost_guard(p, f) { Some(PRESET_8_15) } else { name_of(&PRESET, int(p, "ecoMode")) };
    a.insert("preset_mode".into(), json!(preset));
    (mode.to_string(), a)
}

/// Parameters for a climate service call on a unit with parameters `p`.
/// `Err` carries a message for the user.
pub fn climate_change(service: &str, data: &Map<String, Value>, p: &Map<String, Value>, f: &Features) -> Result<Map<String, Value>, String> {
    let guard = in_frost_guard(p, f);
    let mut m = Map::new();
    let hvac = |m: &mut Map<String, Value>, mode: &str| -> Result<(), String> {
        if mode == "off" {
            m.insert("operate".into(), json!(0));
        } else {
            let code = code_of(&HVAC, if mode == "auto" { "heat_cool" } else { mode }).ok_or_else(|| format!("ukendt tilstand: {mode}"))?;
            m.insert("operate".into(), json!(1));
            m.insert("operationMode".into(), json!(code));
        }
        Ok(())
    };
    match service {
        "turn_on" => {
            m.insert("operate".into(), json!(1));
        }
        "turn_off" => {
            m.insert("operate".into(), json!(0));
        }
        "set_hvac_mode" => {
            let mode = data.get("hvac_mode").and_then(Value::as_str).ok_or("hvac_mode mangler")?;
            hvac(&mut m, mode)?;
            if guard && !matches!(mode, "off" | "heat") {
                leave_frost_guard(&mut m);
            }
        }
        "set_temperature" => {
            let mode = data.get("hvac_mode").and_then(Value::as_str);
            if let Some(mode) = mode {
                hvac(&mut m, mode)?;
            }
            if let Some(t) = data.get("temperature").and_then(Value::as_f64) {
                match f.frost_target(t).filter(|_| t < MIN_TEMP) {
                    Some(t) => {
                        if mode.is_some_and(|m| !matches!(m, "off" | "heat")) {
                            return Err(format!("{t} °C kan kun bruges med varme"));
                        }
                        enter_frost_guard(&mut m, t);
                    }
                    None => {
                        let t = ((t.clamp(MIN_TEMP, MAX_TEMP)) * 2.0).round() / 2.0;
                        m.insert("temperatureSet".into(), json!(t));
                        if guard {
                            leave_frost_guard(&mut m);
                        }
                    }
                }
            }
            if m.is_empty() {
                return Err("temperature mangler".into());
            }
        }
        "set_fan_mode" => {
            let f = data.get("fan_mode").and_then(Value::as_str).unwrap_or_default();
            m.insert("fanSpeed".into(), json!(code_of(&FAN, f).ok_or_else(|| format!("ukendt blæserhastighed: {f}"))?));
        }
        "set_preset_mode" => {
            let pm = data.get("preset_mode").and_then(Value::as_str).unwrap_or_default();
            if pm == PRESET_8_15 && f.summer_house > 0 {
                enter_frost_guard(&mut m, 8.0);
            } else {
                m.insert("ecoMode".into(), json!(code_of(&PRESET, pm).ok_or_else(|| format!("ukendt forudindstilling: {pm}"))?));
                if guard {
                    leave_frost_guard(&mut m);
                }
            }
        }
        _ => return Err(String::new()),
    }
    Ok(m)
}

/// nanoe: 1 = off, 2 = on (3 and 4 are on in other variants).
pub fn nanoe_on(p: &Map<String, Value>) -> Option<bool> {
    match int(p, "nanoe")? {
        0 => None,
        1 => Some(false),
        _ => Some(true),
    }
}

/// Today's consumption in kWh from a month history response.
pub fn consumption_on(history: &Value, day: &str) -> Option<f64> {
    history["historyDataList"]
        .as_array()?
        .iter()
        .find(|i| i["dataTime"].as_str() == Some(day))
        .and_then(|i| i["consumption"].as_f64())
        .filter(|c| *c >= 0.0)
}
