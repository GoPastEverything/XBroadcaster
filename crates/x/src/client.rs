use std::time::Duration;

use serde_json::{json, Value};

use crate::oauth::{refresh, AppConfig, Session, XError};

const API: &str = "https://api.x.com";

#[derive(Clone, Debug)]
pub struct StreamSource {
    pub id: String,
    pub name: String,
    pub region: String,
    pub rtmps_url: String,
    pub stream_key: String,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct CreatedBroadcast {
    pub id: String,
    pub share_url: Option<String>,
    pub state: Option<String>,
}

pub struct XClient {
    http: reqwest::blocking::Client,
    pub config: AppConfig,
    pub session: Session,
}

impl XClient {
    pub fn new(config: AppConfig, session: Session) -> Result<Self, XError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|err| XError::message(err.to_string()))?;
        Ok(Self { http, config, session })
    }

    pub fn ensure_fresh(&mut self) -> Result<(), XError> {
        if self.session.expired() {
            self.session = refresh(&self.config, &self.session)?;
        }
        Ok(())
    }

    pub fn recommended_region(&mut self) -> Result<String, XError> {
        let value = self.get("/2/region")?;
        value
            .get("region")
            .and_then(|region| region.as_str())
            .map(|region| region.to_string())
            .ok_or_else(|| XError::message(format!("region response had no region field: {value}")))
    }

    pub fn list_sources(&mut self) -> Result<Vec<StreamSource>, XError> {
        let value = self.get(&format!("/2/users/{}/sources", self.session.user_id))?;
        let list = value
            .get("sources")
            .or_else(|| value.get("data"))
            .and_then(|item| item.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(list.iter().filter_map(parse_source).collect())
    }

    pub fn create_source(&mut self, name: &str, region: &str) -> Result<StreamSource, XError> {
        let value = self.send(
            reqwest::Method::POST,
            &format!("/2/users/{}/sources", self.session.user_id),
            Some(json!({ "name": name, "region": region })),
        )?;
        let source = value.get("source").unwrap_or(&value);
        parse_source(source).ok_or_else(|| XError::message(format!("create source response: {value}")))
    }

    pub fn get_source(&mut self, id: &str) -> Result<StreamSource, XError> {
        let value = self.get(&format!("/2/users/{}/sources/{id}", self.session.user_id))?;
        let source = value.get("source").unwrap_or(&value);
        parse_source(source).ok_or_else(|| XError::message(format!("get source response: {value}")))
    }

    pub fn create_broadcast(
        &mut self,
        source_id: &str,
        region: &str,
        low_latency: bool,
    ) -> Result<CreatedBroadcast, XError> {
        let value = self.send(
            reqwest::Method::POST,
            &format!("/2/users/{}/broadcasts", self.session.user_id),
            Some(json!({
                "source_id": source_id,
                "region": region,
                "is_low_latency": low_latency,
            })),
        )?;
        let broadcast = value.get("broadcast").unwrap_or(&value);
        let id = broadcast
            .get("id")
            .or_else(|| broadcast.get("broadcast_id"))
            .and_then(|id| id.as_str())
            .ok_or_else(|| XError::message(format!("create broadcast response: {value}")))?
            .to_string();
        Ok(CreatedBroadcast {
            id,
            share_url: value.get("share_url").and_then(|url| url.as_str()).map(str::to_string),
            state: broadcast.get("state").and_then(|state| state.as_str()).map(str::to_string),
        })
    }

    pub fn publish(
        &mut self,
        broadcast_id: &str,
        title: &str,
        chat_option: u8,
        post_announcement: bool,
    ) -> Result<(), XError> {
        self.send(
            reqwest::Method::PUT,
            &format!("/2/users/{}/broadcasts/{broadcast_id}/state", self.session.user_id),
            Some(json!({
                "state": "PUBLISH",
                "title": title,
                "should_not_tweet": !post_announcement,
                "locale": "en",
                "chat_option": chat_option,
            })),
        )?;
        Ok(())
    }

    pub fn end(&mut self, broadcast_id: &str) -> Result<(), XError> {
        self.send(
            reqwest::Method::PUT,
            &format!("/2/users/{}/broadcasts/{broadcast_id}/state", self.session.user_id),
            Some(json!({ "state": "END" })),
        )?;
        Ok(())
    }

    pub fn send_chat(&mut self, broadcast_id: &str, text: &str) -> Result<(), XError> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 140 {
            return Err(XError::message("chat messages are 1 to 140 characters"));
        }
        self.send(
            reqwest::Method::POST,
            &format!("/2/broadcasts/{broadcast_id}/chat"),
            Some(json!({ "text": text })),
        )?;
        Ok(())
    }

    fn get(&mut self, path: &str) -> Result<Value, XError> {
        self.send(reqwest::Method::GET, path, None)
    }

    fn send(&mut self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, XError> {
        self.ensure_fresh()?;
        let url = format!("{API}{path}");
        let send_once = |token: &str| {
            let mut last = None;
            for attempt in 0..3 {
                let mut request = self.http.request(method.clone(), &url).bearer_auth(token);
                if let Some(body) = &body {
                    request = request.json(body);
                }
                match request.send() {
                    Ok(response) => return Ok(response),
                    Err(err) => {
                        last = Some(err);
                        if attempt < 2 {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            }
            Err(last.expect("request attempted"))
        };
        let mut response = send_once(&self.session.access_token.clone()).map_err(|err| XError::message(err.to_string()))?;
        if response.status().as_u16() == 401 {
            self.session = refresh(&self.config, &self.session)?;
            response = send_once(&self.session.access_token.clone()).map_err(|err| XError::message(err.to_string()))?;
        }
        let status = response.status().as_u16();
        let text = response.text().unwrap_or_default();
        if status >= 400 {
            return Err(api_error(status, path, &text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|err| XError::message(format!("json: {err}: {text}")))
    }
}

fn api_error(status: u16, path: &str, text: &str) -> XError {
    let body: String = text.chars().take(500).collect();
    // Livestream routes answer 403 with an empty object when the app is not whitelisted.
    // A signed-in users.read token still succeeds on /2/users/me.
    if status == 403 && (body.trim().is_empty() || body.trim() == "{}") {
        return XError::LivestreamLocked;
    }
    XError::Api {
        status,
        body: format!("{path} {body}"),
    }
}

fn parse_source(value: &Value) -> Option<StreamSource> {
    let id = value.get("id").and_then(|id| id.as_str())?.to_string();
    let key = value
        .get("rtmp_stream_key")
        .and_then(|key| key.as_str())
        .unwrap_or(&id)
        .to_string();
    Some(StreamSource {
        id,
        name: value.get("name").and_then(|name| name.as_str()).unwrap_or("").to_string(),
        region: value
            .get("rtmp_region")
            .or_else(|| value.get("region"))
            .and_then(|region| region.as_str())
            .unwrap_or("")
            .to_string(),
        rtmps_url: value.get("rtmps_url").and_then(|url| url.as_str()).unwrap_or("").to_string(),
        stream_key: key,
        active: value.get("is_stream_active").and_then(|flag| flag.as_bool()).unwrap_or(false),
    })
}
