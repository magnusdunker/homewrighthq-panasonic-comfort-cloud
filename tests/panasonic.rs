//! The API and model tests are the same as for HomewrightHQ's built-in
//! panasonic_cc; `plugin_setup_and_control` plays HomewrightHQ over a pipe.

mod host;

use serde_json::{json, Map, Value};
use thehome_panasonic_comfort_cloud::api::{self, ApiError, Client, Urls, REDIRECT_URI};
use thehome_panasonic_comfort_cloud::model;
use wiremock::matchers::{body_partial_json, body_string_contains, header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const GUID: &str = "CS-Z25XKEW+4590012345";

#[test]
fn api_key_matches_reference() {
    // Value computed with the Python reference implementation.
    assert_eq!(
        api::api_key("2026-10-12 14:03:05", "abc.def"),
        "0c6d4a889cfc787bd0dbf09f057f9e91e58dec11135cf184b0354d7cef58494e13d"
    );
}

#[test]
fn helpers() {
    assert_eq!(api::encode_guid("a/b+c=d e"), "afb%2Bc%3Dd+e");
    assert_eq!(api::find_version(r#"x[["foo"]],["1.22.3"],[null]"#).as_deref(), Some("1.22.3"));
    assert_eq!(api::find_version("nothing here"), None);
    assert_eq!(api::query_param("/u/login?state=abc&x=1", "state").as_deref(), Some("abc"));
    assert_eq!(api::query_param(&format!("{REDIRECT_URI}?code=c1&state=s"), "code").as_deref(), Some("c1"));
    let form = api::hidden_inputs(
        r#"<form method="post" action="https://x/login/callback"><input type="hidden" name="wa" value="wsignin1.0"/>
        <input type='hidden' name="wctx" value="{&#34;strategy&#34;:&quot;auth0&quot;}"><input type="submit" value="Go"></form>"#,
    );
    assert_eq!(form.len(), 2);
    assert_eq!(form["wa"], "wsignin1.0");
    assert_eq!(form["wctx"], r#"{"strategy":"auth0"}"#);
}

fn params() -> Map<String, Value> {
    json!({
        "operate": 1, "operationMode": 3, "temperatureSet": 21.5, "insideTemperature": 20, "outTemperature": 126,
        "fanSpeed": 0, "ecoMode": 2, "airSwingUD": 2, "airSwingLR": 1, "fanAutoMode": 3, "nanoe": 2
    })
    .as_object()
    .cloned()
    .unwrap()
}

#[test]
fn state_mapping() {
    let p = params();
    let f = model::Features::parse(&json!({ "autoMode": true, "heatMode": true, "coolMode": true, "dryMode": false, "fanMode": false, "nanoe": true, "airSwingLR": true }));
    assert_eq!(f.hvac_modes(), vec!["off", "heat_cool", "heat", "cool"]);
    let (state, a) = model::climate(&p, &f);
    assert_eq!(state, "heat");
    assert_eq!(a["temperature"], 21.5);
    assert_eq!(a["current_temperature"], 20.0);
    assert_eq!(a["fan_mode"], "auto");
    assert_eq!(a["preset_mode"], "quiet");
    assert_eq!(model::temperature(&p, "outTemperature"), None);
    assert_eq!(model::vertical(&p), Some("middle"));
    assert_eq!(model::horizontal(&p), Some("auto"));
    assert_eq!(model::nanoe_on(&p), Some(true));

    // Vertical to auto while horizontal is auto: both automatic.
    assert_eq!(model::set_vertical(&p, "auto").unwrap(), *json!({ "fanAutoMode": 0 }).as_object().unwrap());
    // Horizontal to a fixed position keeps vertical fixed.
    assert_eq!(model::set_horizontal(&p, "right").unwrap(), *json!({ "airSwingLR": 0, "fanAutoMode": 1 }).as_object().unwrap());
    assert!(model::set_vertical(&p, "sideways").is_none());

    let d = |v: Value| v.as_object().cloned().unwrap();
    assert_eq!(model::climate_change("set_hvac_mode", &d(json!({ "hvac_mode": "off" })), &p, &f).unwrap(), d(json!({ "operate": 0 })));
    assert_eq!(model::climate_change("set_hvac_mode", &d(json!({ "hvac_mode": "cool" })), &p, &f).unwrap(), d(json!({ "operate": 1, "operationMode": 2 })));
    assert_eq!(model::climate_change("set_temperature", &d(json!({ "temperature": 22.3 })), &p, &f).unwrap(), d(json!({ "temperatureSet": 22.5 })));
    assert_eq!(model::climate_change("set_temperature", &d(json!({ "temperature": 5 })), &p, &f).unwrap(), d(json!({ "temperatureSet": 16.0 })));
    assert_eq!(model::climate_change("set_fan_mode", &d(json!({ "fan_mode": "high" })), &p, &f).unwrap(), d(json!({ "fanSpeed": 5 })));
    assert!(model::climate_change("set_fan_mode", &d(json!({ "fan_mode": "turbo" })), &p, &f).is_err());
    assert_eq!(model::climate_change("set_swing_mode", &Map::new(), &p, &f).unwrap_err(), "");
}

#[test]
fn frost_guard() {
    let d = |v: Value| v.as_object().cloned().unwrap();
    let f = model::Features::parse(&json!({ "summerHouse": 2 }));
    let normal = d(json!({ "operate": 0, "operationMode": 3, "temperatureSet": 21.0, "ecoMode": 0 }));
    assert_eq!(f.preset_modes(), vec!["none", "boost", "quiet", "heat_8_15"]);
    assert_eq!(f.min_temp(), 8.0);

    // Below 16 °C starts the frost guard: heat, powerful, fan high.
    assert_eq!(
        model::climate_change("set_temperature", &d(json!({ "temperature": 12, "hvac_mode": "heat" })), &normal, &f).unwrap(),
        d(json!({ "operate": 1, "operationMode": 3, "temperatureSet": 12.0, "ecoMode": 1, "fanSpeed": 5 }))
    );
    assert!(model::climate_change("set_temperature", &d(json!({ "temperature": 12, "hvac_mode": "cool" })), &normal, &f).is_err());
    assert_eq!(
        model::climate_change("set_preset_mode", &d(json!({ "preset_mode": "heat_8_15" })), &normal, &f).unwrap(),
        d(json!({ "operationMode": 3, "temperatureSet": 8.0, "ecoMode": 1, "fanSpeed": 5 }))
    );

    // Running at 12 °C it shows as the preset; 20 °C leaves it.
    let guard = d(json!({ "operate": 1, "operationMode": 3, "temperatureSet": 12.0, "ecoMode": 1, "fanSpeed": 5 }));
    let (_, a) = model::climate(&guard, &f);
    assert_eq!(a["preset_mode"], "heat_8_15");
    assert_eq!(a["min_temp"], 8.0);
    assert_eq!(
        model::climate_change("set_temperature", &d(json!({ "temperature": 20 })), &guard, &f).unwrap(),
        d(json!({ "temperatureSet": 20.0, "ecoMode": 0, "fanSpeed": 0 }))
    );
    assert_eq!(
        model::climate_change("set_preset_mode", &d(json!({ "preset_mode": "quiet" })), &guard, &f).unwrap(),
        d(json!({ "ecoMode": 2, "temperatureSet": 20.0, "fanSpeed": 0 }))
    );

    // Units with only 8 and 10 °C snap to those.
    let f10 = model::Features::parse(&json!({ "summerHouse": 1 }));
    assert_eq!(model::climate_change("set_temperature", &d(json!({ "temperature": 12 })), &normal, &f10).unwrap()["temperatureSet"], 10.0);
    // Without the feature the floor stays 16 °C.
    let none = model::Features::parse(&json!({}));
    assert_eq!(model::climate_change("set_temperature", &d(json!({ "temperature": 12 })), &normal, &none).unwrap(), d(json!({ "temperatureSet": 16.0 })));
}

#[test]
fn parse_devices_skips_other_types() {
    let g = groups();
    let d = model::parse_devices(&g);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].guid, GUID);
    assert_eq!(d[0].name, "Stue");
    assert_eq!(d[0].model.as_deref(), Some("CS-Z25XKEW"));
}

fn groups() -> Value {
    json!({ "groupList": [{ "groupId": 1, "groupName": "Hjem", "deviceList": [
        { "deviceGuid": GUID, "deviceType": "1", "deviceName": "Stue", "deviceModuleNumber": "CS-Z25XKEW", "parameters": {} },
        { "deviceGuid": "aquarea-1", "deviceType": "2", "deviceName": "Varmepumpe" }
    ] }] })
}

fn status() -> Value {
    json!({ "autoMode": true, "heatMode": true, "coolMode": true, "dryMode": true, "fanMode": true,
            "nanoe": true, "airSwingLR": true, "powerfulMode": true, "quietMode": true, "parameters": params() })
}

async fn mock_cloud(server: &MockServer) {
    let login_html = r#"<html><form method="post" name="hiddenform" action="https://example/login/callback">
        <input type="hidden" name="wa" value="wsignin1.0">
        <input type="hidden" name="wresult" value="eyJ.jwt.sig">
        <input type="hidden" name="wctx" value="{&#34;strategy&#34;:&#34;auth0&#34;}">
        </form></html>"#;

    Mock::given(method("GET")).and(path("/authorize"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/u/login?state=st1").insert_header("set-cookie", "auth0=sess; Path=/"))
        .mount(server).await;
    Mock::given(method("GET")).and(path("/u/login")).and(header("cookie", "auth0=sess"))
        .respond_with(ResponseTemplate::new(200).insert_header("set-cookie", "_csrf=csrf1; Path=/usernamepassword/login").set_body_string("<html/>"))
        .mount(server).await;
    Mock::given(method("POST")).and(path("/usernamepassword/login"))
        .and(body_partial_json(json!({ "username": "me@example.com", "password": "secret", "_csrf": "csrf1", "state": "st1" })))
        .respond_with(ResponseTemplate::new(200).set_body_string(login_html))
        .with_priority(1)
        .mount(server).await;
    Mock::given(method("POST")).and(path("/usernamepassword/login"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({ "name": "ValidationError", "code": "invalid_user_password" })))
        .with_priority(5)
        .mount(server).await;
    Mock::given(method("POST")).and(path("/login/callback")).and(body_string_contains("wctx=%7B%22strategy%22%3A%22auth0%22%7D"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/authorize/resume?state=st2"))
        .mount(server).await;
    Mock::given(method("GET")).and(path("/authorize/resume"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{REDIRECT_URI}?code=code1&state=st1").as_str()))
        .mount(server).await;
    Mock::given(method("POST")).and(path("/oauth/token")).and(body_partial_json(json!({ "grant_type": "authorization_code", "code": "code1" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "access_token": "acc1", "refresh_token": "ref1", "expires_in": 86400, "token_type": "Bearer" })))
        .mount(server).await;
    Mock::given(method("GET")).and(path("/store"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"<script>[["x"],["1.23.4"]]</script>"#))
        .mount(server).await;

    Mock::given(method("POST")).and(path("/auth/v2/login")).and(header("x-app-version", "1.23.4")).and(header("x-user-authorization-v2", "Bearer acc1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "clientId": "cid1" })))
        .mount(server).await;
    Mock::given(method("GET")).and(path("/device/group")).and(header("x-client-id", "cid1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(groups()))
        .mount(server).await;
    Mock::given(method("GET")).and(path_regex(r"^/deviceStatus/CS-Z25XKEW%2B4590012345$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status()))
        .mount(server).await;
    Mock::given(method("POST")).and(path("/deviceHistoryData"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "historyDataList": [
            { "dataTime": chrono::Local::now().format("%Y%m%d").to_string(), "consumption": 1.75 } ] })))
        .mount(server).await;
    Mock::given(method("POST")).and(path("/deviceStatus/control"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": 0 })))
        .mount(server).await;
}

fn test_urls(server: &MockServer) -> Urls {
    Urls { auth: server.uri(), acc: server.uri(), version: format!("{}/store", server.uri()) }
}

#[tokio::test]
async fn login_and_api() {
    let server = MockServer::start().await;
    mock_cloud(&server).await;

    let bad = Client::new(test_urls(&server), "me@example.com", "wrong", None, None);
    assert!(matches!(bad.groups().await, Err(ApiError::Auth)));

    let c = Client::new(test_urls(&server), "me@example.com", "secret", None, None);
    let g = c.groups().await.unwrap();
    assert_eq!(model::parse_devices(&g).len(), 1);
    let s = c.session().await;
    assert_eq!(s.access_token, "acc1");
    assert_eq!(s.refresh_token.as_deref(), Some("ref1"));
    assert_eq!(s.client_id.as_deref(), Some("cid1"));
    assert_eq!(s.app_version.as_deref(), Some("1.23.4"));
    assert_eq!(c.status(GUID).await.unwrap()["parameters"]["temperatureSet"], 21.5);
}

#[tokio::test]
async fn mfa_is_reported() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/authorize"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/u/login?state=st1"))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/u/login"))
        .respond_with(ResponseTemplate::new(200).insert_header("set-cookie", "_csrf=c"))
        .mount(&server).await;
    Mock::given(method("POST")).and(path("/usernamepassword/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"<form><input type="hidden" name="mfa_token" value="m1"></form>"#))
        .mount(&server).await;
    let c = Client::new(test_urls(&server), "me@example.com", "secret", None, None);
    assert!(matches!(c.groups().await, Err(ApiError::Mfa)));
}


async fn wait_state(h: &host::FakeHost, id: &str) -> (String, Value) {
    for _ in 0..200 {
        if let Some(s) = h.state(id).filter(|s| s.0 != "unknown" && s.0 != "unavailable") {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("{id} fik aldrig en tilstand");
}

fn entry(server: &MockServer, password: &str) -> Value {
    json!({ "entry_id": "e1", "domain": "panasonic_comfort_cloud", "title": "Comfort Cloud",
            "data": { "username": "me@example.com", "password": password,
                      "auth_url": server.uri(), "acc_url": server.uri(), "version_url": format!("{}/store", server.uri()) },
            "options": {} })
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_flow() {
    let server = MockServer::start().await;
    mock_cloud(&server).await;
    let h = host::FakeHost::start(thehome_panasonic_comfort_cloud::Panasonic::default());
    let urls = json!({ "auth_url": server.uri(), "acc_url": server.uri(), "version_url": format!("{}/store", server.uri()) });
    let flow = |source: &str| json!({ "flow_id": "f1", "source": source, "data": urls });

    let r = h.call("config_flow", json!({ "flow": flow("user"), "step": "user", "input": null })).await.unwrap();
    assert_eq!(r["result"]["type"], "form");
    let r = h
        .call("config_flow", json!({ "flow": flow("user"), "step": "user", "input": { "username": "me@example.com", "password": "wrong" } }))
        .await
        .unwrap();
    assert_eq!(r["result"]["errors"]["base"], "invalid_auth");
    let r = h
        .call("config_flow", json!({ "flow": flow("user"), "step": "user", "input": { "username": " me@example.com ", "password": "secret" } }))
        .await
        .unwrap();
    assert_eq!(r["result"]["type"], "create_entry");
    assert_eq!(r["result"]["title"], "Comfort Cloud (me@example.com)");
    assert_eq!(r["unique_id"], "me@example.com");
    assert_eq!(r["result"]["data"]["password"], "secret");

    // reauth keeps the entry's other data
    let mut reauth = flow("reauth");
    reauth["entry"] = json!({ "entry_id": "e1", "data": { "username": "me@example.com", "password": "old", "keep": 1 } });
    let r = h.call("config_flow", json!({ "flow": reauth, "step": "reauth", "input": null })).await.unwrap();
    assert_eq!(r["result"]["step_id"], "user");
    let r = h
        .call("config_flow", json!({ "flow": reauth, "step": "user", "input": { "username": "me@example.com", "password": "secret" } }))
        .await
        .unwrap();
    assert_eq!(r["result"]["data"], json!({ "username": "me@example.com", "password": "secret", "keep": 1 }));

    // a wrong password on an entry asks for a new login
    let e = h.call("setup_entry", json!({ "entry": entry(&server, "wrong") })).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "auth_failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_setup_and_control() {
    let server = MockServer::start().await;
    mock_cloud(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let h = host::FakeHost::start(thehome_panasonic_comfort_cloud::Panasonic::default());
    h.call("initialize", json!({ "api_version": 1, "data_dir": dir.path() })).await.unwrap();
    h.call("setup_entry", json!({ "entry": entry(&server, "secret") })).await.unwrap();

    {
        let rec = h.rec.lock().unwrap();
        assert_eq!(rec.devices.len(), 1);
        assert_eq!(rec.devices[0]["identifiers"], json!([["panasonic_comfort_cloud", GUID]]));
        assert_eq!(rec.devices[0]["model"], "CS-Z25XKEW");
    }
    let climate = h.entity_ending("climate.stue");
    let (spec, handler) = h.rec.lock().unwrap().entities[&climate].clone();
    assert!(handler);
    assert_eq!(spec["capabilities"]["supported_features"], 409);
    assert_eq!(spec["capabilities"]["min_temp"], 16.0);
    let st = wait_state(&h, &climate).await;
    assert_eq!(st.0, "heat");
    assert_eq!(st.1["temperature"], 21.5);
    assert_eq!(wait_state(&h, &h.entity_ending("indetemperatur")).await.0, "20");
    assert_eq!(wait_state(&h, &h.entity_ending("energi_i_dag")).await.0, "1.75");
    assert_eq!(wait_state(&h, &h.entity_ending("lodret_luftretning")).await.0, "middle");
    assert_eq!(wait_state(&h, &h.entity_ending("vandret_luftretning")).await.0, "auto");
    assert_eq!(wait_state(&h, &h.entity_ending("nanoe")).await.0, "on");
    // outTemperature 126 means no reading
    assert_eq!(h.state(&h.entity_ending("udetemperatur")).unwrap().0, "unknown");

    let call = |entity: String, service: &str, data: Value| json!({ "entry_id": "e1", "entity_id": entity, "service": service, "data": data });
    h.call("entity/call_service", call(climate.clone(), "set_temperature", json!({ "temperature": 23 }))).await.unwrap();
    h.call("entity/call_service", call(climate.clone(), "set_hvac_mode", json!({ "hvac_mode": "off" }))).await.unwrap();
    h.call("entity/call_service", call(h.entity_ending("lodret_luftretning"), "select_option", json!({ "option": "auto" }))).await.unwrap();
    let e = h.call("entity/call_service", call(climate.clone(), "set_swing_mode", json!({}))).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "not_supported");
    let e = h.call("entity/call_service", call(climate.clone(), "set_fan_mode", json!({ "fan_mode": "turbo" }))).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "invalid_argument");
    assert_eq!(h.state(&climate).unwrap().0, "off");
    assert_eq!(h.state(&climate).unwrap().1["temperature"], 23.0);
    assert_eq!(h.state(&h.entity_ending("lodret_luftretning")).unwrap().0, "auto");

    let sent: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r: &&Request| r.url.path() == "/deviceStatus/control")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0], json!({ "deviceGuid": GUID, "parameters": { "temperatureSet": 23.0 } }));
    assert_eq!(sent[1]["parameters"], json!({ "operate": 0 }));
    assert_eq!(sent[2]["parameters"], json!({ "fanAutoMode": 0 }));

    // The session is saved in the data folder, so a restart refreshes
    // instead of logging in again.
    let saved: api::Session = serde_json::from_str(&std::fs::read_to_string(dir.path().join("session-e1.json")).unwrap()).unwrap();
    assert_eq!(saved.refresh_token.as_deref(), Some("ref1"));
    assert_eq!(saved.client_id.as_deref(), Some("cid1"));

    let diag = h.call("diagnostics", json!({ "entry": entry(&server, "secret") })).await.unwrap();
    assert_eq!(diag["password"], "**REDACTED**");
    h.call("unload_entry", json!({ "entry_id": "e1" })).await.unwrap();
    let e = h.call("entity/call_service", call(climate, "turn_on", json!({}))).await.unwrap_err();
    assert_eq!(e["data"]["kind"], "unavailable");
}
