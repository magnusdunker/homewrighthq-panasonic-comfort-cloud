//! Client for Panasonic Comfort Cloud.
//!
//! Login goes through Panasonic's Auth0 tenant the same way the Android app
//! does it: OAuth2 authorization code with PKCE, a username/password POST and
//! a WS-Fed style callback form. The resulting access token is then used
//! against the Comfort Cloud API (`accsmart.panasonic.com`), where every call
//! carries a timestamp and a key derived from it (`X-CFC-API-KEY`).
//!
//! Based on the protocol as implemented by `aio-panasonic-comfort-cloud`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::anyhow;
use base64::Engine as _;
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue, LOCATION, SET_COOKIE};
use reqwest::{Method, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const AUTH_URL: &str = "https://authglb.digital.panasonic.com";
pub const ACC_URL: &str = "https://accsmart.panasonic.com";
pub const VERSION_URL: &str = "https://play.google.com/store/apps/details?id=com.panasonic.ACCsmart";
pub const APP_CLIENT_ID: &str = "Xmy6xIYIitMxngjB2rHvlm6HSDNnaMJx";
pub const AUTH0_CLIENT: &str = "eyJuYW1lIjoiQXV0aDAuQW5kcm9pZCIsImVudiI6eyJhbmRyb2lkIjoiMzAifSwidmVyc2lvbiI6IjIuOS4zIn0=";
pub const REDIRECT_URI: &str = "panasonic-iot-cfc://authglb.digital.panasonic.com/android/com.panasonic.ACCsmart/callback";
pub const DEFAULT_APP_VERSION: &str = "1.21.0";
const SCOPE: &str = "openid offline_access comfortcloud.control a2w.control";
const AUTH_API_UA: &str = "okhttp/4.10.0";
const AUTH_BROWSER_UA: &str = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/113.0.0.0 Mobile Safari/537.36";
const API_KEY_SALT: &str = "521325fb2dd486bf4831b47644317fca";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("forkert brugernavn eller adgangskode")]
    Auth,
    #[error("kontoen bruger totrinsbekræftelse, som ikke understøttes endnu")]
    Mfa,
    #[error("nye vilkår skal accepteres i Comfort Cloud-appen")]
    Agreement,
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

/// Where the client talks to. Tests point these at a mock server.
#[derive(Clone, Debug)]
pub struct Urls {
    pub auth: String,
    pub acc: String,
    pub version: String,
}

impl Default for Urls {
    fn default() -> Self {
        Self { auth: AUTH_URL.into(), acc: ACC_URL.into(), version: VERSION_URL.into() }
    }
}

/// Tokens and ids that survive a restart.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
    pub client_id: Option<String>,
    pub app_version: Option<String>,
}

impl Session {
    fn token_valid(&self) -> bool {
        !self.access_token.is_empty() && self.expires_at - 60 > chrono::Utc::now().timestamp()
    }
}

pub type Persist = Arc<dyn Fn(&Session) + Send + Sync>;

pub struct Client {
    http: reqwest::Client,
    urls: Urls,
    username: String,
    password: String,
    session: tokio::sync::Mutex<Session>,
    persist: Option<Persist>,
}

impl Client {
    pub fn new(urls: Urls, username: &str, password: &str, session: Option<Session>, persist: Option<Persist>) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(StdDuration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            http,
            urls,
            username: username.into(),
            password: password.into(),
            session: tokio::sync::Mutex::new(session.unwrap_or_default()),
            persist,
        }
    }

    pub async fn session(&self) -> Session {
        self.session.lock().await.clone()
    }

    // ------------------------------------------------------------------
    // Comfort Cloud API
    // ------------------------------------------------------------------

    pub async fn groups(&self) -> Result<Value, ApiError> {
        self.call(Method::GET, "/device/group", None).await
    }

    /// Live status. Falls back to the cloud's cached copy when the unit
    /// does not answer.
    pub async fn status(&self, guid: &str) -> Result<Value, ApiError> {
        let g = encode_guid(guid);
        match self.call(Method::GET, &format!("/deviceStatus/{g}"), None).await {
            Ok(v) => Ok(v),
            Err(ApiError::Other(e)) => {
                crate::debug!("live status failed ({e}), using cached");
                self.call(Method::GET, &format!("/deviceStatus/now/{g}"), None).await
            }
            Err(e) => Err(e),
        }
    }

    pub async fn control(&self, guid: &str, parameters: &serde_json::Map<String, Value>) -> Result<(), ApiError> {
        self.call(Method::POST, "/deviceStatus/control", Some(json!({ "deviceGuid": guid, "parameters": parameters })))
            .await
            .map(|_| ())
    }

    /// Daily history for the month containing `date` (`YYYYMMDD`).
    pub async fn month_history(&self, guid: &str, date: &str, tz: &str) -> Result<Value, ApiError> {
        self.call(
            Method::POST,
            "/deviceHistoryData",
            Some(json!({ "deviceGuid": guid, "dataMode": 2, "date": date, "osTimezone": tz })),
        )
        .await
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, ApiError> {
        let mut last = None;
        for _ in 0..3 {
            let s = self.ensure_session().await?;
            let mut req = self
                .http
                .request(method.clone(), format!("{}{path}", self.urls.acc))
                .headers(acc_headers(&s, true));
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(|e| anyhow!("Comfort Cloud: {}", e.without_url()))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                return serde_json::from_str(&text).map_err(|_| anyhow!("Comfort Cloud: ugyldig JSON fra {path}").into());
            }
            if text.contains("4103") {
                return Err(ApiError::Agreement);
            }
            if status == StatusCode::UNAUTHORIZED {
                let mut s = self.session.lock().await;
                if text.contains("4106") {
                    // A newer app version is required.
                    let v = self.latest_app_version().await;
                    crate::info!("skifter app-version til {v}");
                    s.app_version = Some(v);
                } else {
                    s.expires_at = 0;
                }
                last = Some(anyhow!("Comfort Cloud: HTTP {status} fra {path}"));
                continue;
            }
            return Err(anyhow!("Comfort Cloud: HTTP {status} fra {path}: {}", excerpt(&text)).into());
        }
        Err(last.unwrap_or_else(|| anyhow!("Comfort Cloud: for mange forsøg")).into())
    }

    /// Make sure there is a valid access token and an ACC client id.
    async fn ensure_session(&self) -> Result<Session, ApiError> {
        let mut s = self.session.lock().await;
        if s.token_valid() && s.client_id.is_some() {
            return Ok(s.clone());
        }
        if s.app_version.is_none() {
            s.app_version = Some(self.latest_app_version().await);
        }
        let mut refreshed = false;
        if !s.token_valid() {
            if let Some(rt) = s.refresh_token.clone() {
                match self.refresh(&rt).await {
                    Ok(t) => {
                        t.apply(&mut s);
                        refreshed = true;
                    }
                    Err(e) => crate::info!("fornyelse af token fejlede, logger ind igen: {e}"),
                }
            }
            if !refreshed {
                self.login().await?.apply(&mut s);
                s.client_id = None;
            }
        }
        if s.client_id.is_none() {
            s.client_id = Some(self.acc_client_id(&mut s).await?);
        }
        if let Some(p) = &self.persist {
            p(&s);
        }
        Ok(s.clone())
    }

    async fn acc_client_id(&self, s: &mut Session) -> Result<String, ApiError> {
        for _ in 0..2 {
            let resp = self
                .http
                .post(format!("{}/auth/v2/login", self.urls.acc))
                .headers(acc_headers(s, false))
                .json(&json!({ "language": 0 }))
                .send()
                .await
                .map_err(|e| anyhow!("Comfort Cloud: {}", e.without_url()))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                let v: Value = serde_json::from_str(&text).map_err(|_| anyhow!("Comfort Cloud: ugyldig JSON fra login"))?;
                return v["clientId"].as_str().map(String::from).ok_or_else(|| anyhow!("Comfort Cloud: clientId mangler").into());
            }
            if text.contains("4103") {
                return Err(ApiError::Agreement);
            }
            if status == StatusCode::UNAUTHORIZED && text.contains("4106") {
                s.app_version = Some(self.latest_app_version().await);
                continue;
            }
            return Err(anyhow!("Comfort Cloud: login gav HTTP {status}: {}", excerpt(&text)).into());
        }
        Err(anyhow!("Comfort Cloud: app-versionen afvises stadig").into())
    }

    /// The app version Panasonic currently accepts, read from the Play Store
    /// page. Falls back to a known version.
    async fn latest_app_version(&self) -> String {
        let page = async {
            let r = self.http.get(&self.urls.version).header("user-agent", AUTH_BROWSER_UA).send().await.ok()?;
            r.text().await.ok()
        }
        .await;
        page.as_deref().and_then(find_version).unwrap_or_else(|| DEFAULT_APP_VERSION.into())
    }

    // ------------------------------------------------------------------
    // Auth0
    // ------------------------------------------------------------------

    async fn refresh(&self, refresh_token: &str) -> Result<Tokens, ApiError> {
        let resp = self
            .http
            .post(format!("{}/oauth/token", self.urls.auth))
            .header("Auth0-Client", AUTH0_CLIENT)
            .header("user-agent", AUTH_API_UA)
            .json(&json!({ "scope": SCOPE, "client_id": APP_CLIENT_ID, "refresh_token": refresh_token, "grant_type": "refresh_token" }))
            .send()
            .await
            .map_err(|e| anyhow!("Panasonic ID: {}", e.without_url()))?;
        Tokens::from_response(resp, "refresh").await
    }

    /// Full login with username and password.
    pub async fn login(&self) -> Result<Tokens, ApiError> {
        let mut jar = Jar::default();
        let verifier = random_string(43);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_string(20);
        let audience = format!("https://digital.panasonic.com/{APP_CLIENT_ID}/api/v1/");

        let resp = self
            .http
            .get(format!("{}/authorize", self.urls.auth))
            .header("user-agent", AUTH_API_UA)
            .query(&[
                ("scope", SCOPE),
                ("audience", &audience),
                ("protocol", "oauth2"),
                ("response_type", "code"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("auth0Client", AUTH0_CLIENT),
                ("client_id", APP_CLIENT_ID),
                ("redirect_uri", REDIRECT_URI),
                ("state", &state),
            ])
            .send()
            .await
            .map_err(|e| anyhow!("Panasonic ID: {}", e.without_url()))?;
        let location = expect_redirect(&resp, "authorize")?;
        jar.absorb(&resp);

        let code = if location.starts_with(REDIRECT_URI) {
            query_param(&location, "code").ok_or_else(|| anyhow!("Panasonic ID: kode mangler"))?
        } else {
            self.login_form(&mut jar, &location, &audience).await?
        };

        let resp = self
            .http
            .post(format!("{}/oauth/token", self.urls.auth))
            .header("Auth0-Client", AUTH0_CLIENT)
            .header("user-agent", AUTH_API_UA)
            .json(&json!({
                "scope": "openid",
                "client_id": APP_CLIENT_ID,
                "grant_type": "authorization_code",
                "code": code,
                "redirect_uri": REDIRECT_URI,
                "code_verifier": verifier,
            }))
            .send()
            .await
            .map_err(|e| anyhow!("Panasonic ID: {}", e.without_url()))?;
        Tokens::from_response(resp, "token").await
    }

    async fn login_form(&self, jar: &mut Jar, location: &str, audience: &str) -> Result<String, ApiError> {
        let state = query_param(location, "state").unwrap_or_default();
        let resp = self
            .http
            .get(self.auth_url(location))
            .header("cookie", jar.header())
            .send()
            .await
            .map_err(|e| anyhow!("Panasonic ID: {}", e.without_url()))?;
        jar.absorb(&resp);
        if resp.status() != StatusCode::OK {
            return Err(anyhow!("Panasonic ID: login-siden gav HTTP {}", resp.status()).into());
        }
        let csrf = jar.get("_csrf").ok_or_else(|| anyhow!("Panasonic ID: _csrf-cookie mangler"))?.to_string();

        let resp = self
            .http
            .post(format!("{}/usernamepassword/login", self.urls.auth))
            .header("Auth0-Client", AUTH0_CLIENT)
            .header("user-agent", AUTH_API_UA)
            .header("cookie", jar.header())
            .json(&json!({
                "client_id": APP_CLIENT_ID,
                "redirect_uri": REDIRECT_URI,
                "tenant": "pdpauthglb-a1",
                "response_type": "code",
                "scope": SCOPE,
                "audience": audience,
                "_csrf": csrf,
                "state": state,
                "_intstate": "deprecated",
                "username": self.username,
                "password": self.password,
                "lang": "en",
                "connection": "PanasonicID-Authentication",
            }))
            .send()
            .await
            .map_err(|e| anyhow!("Panasonic ID: {}", e.without_url()))?;
        jar.absorb(&resp);
        let status = resp.status();
        let html = resp.text().await.unwrap_or_default();
        if matches!(status.as_u16(), 400 | 401 | 403) {
            crate::debug!("login afvist: {}", excerpt(&html));
            return Err(ApiError::Auth);
        }
        if status != StatusCode::OK {
            return Err(anyhow!("Panasonic ID: login gav HTTP {status}").into());
        }
        let form = hidden_inputs(&html);
        if is_mfa_page(&html, &form) {
            return Err(ApiError::Mfa);
        }

        let resp = self
            .http
            .post(format!("{}/login/callback", self.urls.auth))
            .header("user-agent", AUTH_BROWSER_UA)
            .header("cookie", jar.header())
            .form(&form)
            .send()
            .await
            .map_err(|e| anyhow!("Panasonic ID: {}", e.without_url()))?;
        let location = expect_redirect(&resp, "login/callback")?;
        jar.absorb(&resp);

        let resp = self
            .http
            .get(self.auth_url(&location))
            .header("cookie", jar.header())
            .send()
            .await
            .map_err(|e| anyhow!("Panasonic ID: {}", e.without_url()))?;
        let location = expect_redirect(&resp, "login redirect")?;
        jar.absorb(&resp);
        if let Some(code) = query_param(&location, "code") {
            return Ok(code);
        }
        if location.starts_with(REDIRECT_URI) {
            return Err(anyhow!("Panasonic ID: login sluttede uden kode").into());
        }
        // Some accounts are sent to an MFA challenge page here instead.
        let resp = self
            .http
            .get(self.auth_url(&location))
            .header("cookie", jar.header())
            .send()
            .await
            .map_err(|e| anyhow!("Panasonic ID: {}", e.without_url()))?;
        let html = resp.text().await.unwrap_or_default();
        if is_mfa_page(&html, &hidden_inputs(&html)) {
            return Err(ApiError::Mfa);
        }
        Err(anyhow!("Panasonic ID: login gav ingen kode").into())
    }

    fn auth_url(&self, location: &str) -> String {
        if location.starts_with("http://") || location.starts_with("https://") {
            location.into()
        } else {
            format!("{}/{}", self.urls.auth, location.trim_start_matches('/'))
        }
    }
}

#[derive(Debug)]
pub struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
}

impl Tokens {
    async fn from_response(resp: Response, what: &str) -> Result<Self, ApiError> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if matches!(status.as_u16(), 400 | 401 | 403) {
            crate::debug!("{what} afvist: {}", excerpt(&text));
            return Err(ApiError::Auth);
        }
        if !status.is_success() {
            return Err(anyhow!("Panasonic ID: {what} gav HTTP {status}").into());
        }
        let v: Value = serde_json::from_str(&text).map_err(|_| anyhow!("Panasonic ID: ugyldig JSON"))?;
        let access_token = v["access_token"].as_str().ok_or_else(|| anyhow!("Panasonic ID: access_token mangler"))?.to_string();
        Ok(Self {
            access_token,
            refresh_token: v["refresh_token"].as_str().map(String::from),
            expires_at: chrono::Utc::now().timestamp() + v["expires_in"].as_i64().unwrap_or(3600),
        })
    }

    fn apply(self, s: &mut Session) {
        s.access_token = self.access_token;
        // Refresh tokens rotate; keep the old one if none was returned.
        if self.refresh_token.is_some() {
            s.refresh_token = self.refresh_token;
        }
        s.expires_at = self.expires_at;
    }
}

fn acc_headers(s: &Session, include_client_id: bool) -> HeaderMap {
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut h = HeaderMap::new();
    let mut put = |k: &'static str, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(k, v);
        }
    };
    put("accept", "application/json; charset=utf-8");
    put("content-type", "application/json");
    put("user-agent", "G-RAC");
    put("x-app-name", "Comfort Cloud");
    put("x-app-timestamp", &timestamp);
    put("x-app-type", "1");
    put("x-app-version", s.app_version.as_deref().unwrap_or(DEFAULT_APP_VERSION));
    put("x-cfc-api-key", &api_key(&timestamp, &s.access_token));
    put("x-user-authorization-v2", &format!("Bearer {}", s.access_token));
    if include_client_id {
        if let Some(c) = &s.client_id {
            put("x-client-id", c);
        }
    }
    h
}

/// `X-CFC-API-KEY`: SHA-256 over app name, a fixed salt, the timestamp in
/// milliseconds and the bearer token, with "cfc" spliced in after nine hex
/// digits. The app reads the local wall-clock time as if it were UTC, so do
/// the same.
pub fn api_key(timestamp: &str, token: &str) -> String {
    let ms = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S")
        .map(|d| d.and_utc().timestamp_millis())
        .unwrap_or_default();
    let mut h = Sha256::new();
    h.update(b"Comfort Cloud");
    h.update(API_KEY_SALT.as_bytes());
    h.update(ms.to_string().as_bytes());
    h.update(b"Bearer ");
    h.update(token.as_bytes());
    let hex = hex::encode(h.finalize());
    format!("{}cfc{}", &hex[..9], &hex[9..])
}

/// Device GUIDs can contain `/`, which the API wants as `f`, and other
/// characters that need form-style escaping.
pub fn encode_guid(guid: &str) -> String {
    let mut out = String::new();
    for b in guid.replace('/', "f").bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn random_string(len: usize) -> String {
    rand::thread_rng().sample_iter(&rand::distributions::Alphanumeric).take(len).map(char::from).collect()
}

fn expect_redirect(resp: &Response, step: &str) -> Result<String, ApiError> {
    if !resp.status().is_redirection() {
        return Err(anyhow!("Panasonic ID: {step} gav HTTP {} i stedet for en omdirigering", resp.status()).into());
    }
    resp.headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .ok_or_else(|| anyhow!("Panasonic ID: {step} mangler Location").into())
}

pub fn query_param(url: &str, key: &str) -> Option<String> {
    let parsed = Url::parse(url).or_else(|_| Url::parse("http://x/").and_then(|b| b.join(url))).ok()?;
    parsed.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

/// The Play Store page embeds the version as `["1.2.3"]`.
pub fn find_version(page: &str) -> Option<String> {
    let mut rest = page;
    while let Some(i) = rest.find("[\"") {
        rest = &rest[i + 2..];
        let end = rest.find("\"]")?;
        let cand = &rest[..end];
        let parts: Vec<&str> = cand.split('.').collect();
        if parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
            return Some(cand.into());
        }
    }
    None
}

fn is_mfa_page(html: &str, form: &BTreeMap<String, String>) -> bool {
    form.contains_key("mfa_token") || html.contains("__g_config")
}

/// Hidden `<input>` fields of an HTML form, entity-decoded.
pub fn hidden_inputs(html: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<input") {
        let start = pos + i;
        let Some(len) = lower[start..].find('>') else { break };
        let tag = &html[start + 6..start + len];
        pos = start + len;
        let attrs = parse_attrs(tag);
        if attrs.get("type").map(|t| t.eq_ignore_ascii_case("hidden")) != Some(true) {
            continue;
        }
        if let Some(name) = attrs.get("name") {
            out.insert(name.clone(), attrs.get("value").cloned().unwrap_or_default());
        }
    }
    out
}

fn parse_attrs(tag: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let b = tag.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ks = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        if ks == i {
            i += 1;
            continue;
        }
        let key = tag[ks..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                let vs = i;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                value = tag[vs..i].to_string();
                i += 1;
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = tag[vs..i].to_string();
            }
        }
        out.insert(key, decode_entities(&value));
    }
    out
}

pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "quot" => Some('"'),
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn excerpt(s: &str) -> String {
    s.chars().take(200).collect()
}

/// Minimal cookie jar for the login flow, which stays on one host.
#[derive(Default)]
struct Jar(BTreeMap<String, String>);

impl Jar {
    fn absorb(&mut self, resp: &Response) {
        for v in resp.headers().get_all(SET_COOKIE) {
            let Ok(v) = v.to_str() else { continue };
            let pair = v.split(';').next().unwrap_or_default();
            if let Some((k, val)) = pair.split_once('=') {
                self.0.insert(k.trim().to_string(), val.trim().to_string());
            }
        }
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str)
    }

    fn header(&self) -> String {
        self.0.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }
}
