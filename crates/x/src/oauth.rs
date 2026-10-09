use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const AUTHORIZE_URL: &str = "https://x.com/i/oauth2/authorize";
pub const TOKEN_URL: &str = "https://api.x.com/2/oauth2/token";
pub const SCOPES: &str = "broadcast.read broadcast.write users.read tweet.read offline.access";

#[derive(Debug, thiserror::Error)]
pub enum XError {
    #[error("{0}")]
    Message(String),
    #[error("X API {status}: {body}")]
    Api { status: u16, body: String },
}

impl XError {
    pub fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    /// Public OAuth client id for this build. It is not an account key and
    /// cannot read anyone's account by itself. There is no client secret:
    /// a published desktop app uses PKCE so each person signs in as themselves.
    #[serde(default)]
    pub client_id: String,
    #[serde(default = "default_redirect_port")]
    pub redirect_port: u16,
}

fn default_redirect_port() -> u16 {
    43821
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            redirect_port: default_redirect_port(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_unix: u64,
    pub user_id: String,
    pub username: String,
    pub name: String,
}

impl Session {
    pub fn expired(&self) -> bool {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        now + 60 >= self.expires_at_unix
    }
}

pub fn sign_in(config: &AppConfig) -> Result<Session, XError> {
    if config.client_id.trim().is_empty() {
        return Err(XError::message(
            "This build has no public X client id. Set XB_X_CLIENT_ID when building. That id is not a user key, and the app has no API secret.",
        ));
    }
    let verifier = code_verifier();
    let challenge = code_challenge(&verifier);
    let state = code_verifier();
    let redirect = format!("http://127.0.0.1:{}/callback", config.redirect_port);
    let url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        encode(&config.client_id),
        encode(&redirect),
        encode(SCOPES),
        encode(&state),
        encode(&challenge),
    );
    let listener = TcpListener::bind(("127.0.0.1", config.redirect_port))
        .map_err(|err| XError::message(format!("callback port {} is busy: {err}", config.redirect_port)))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| XError::message(err.to_string()))?;
    // A .url file keeps the long query string intact. cmd `start` would split it on `&`.
    open_browser(&url)?;

    let code = wait_for_code(&listener, &state)?;
    let tokens = exchange(config, &redirect, &verifier, &code)?;
    let mut session = Session {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + tokens.expires_in.unwrap_or(7200),
        user_id: String::new(),
        username: String::new(),
        name: String::new(),
    };
    let me = users_me(&session.access_token)?;
    session.user_id = me.0;
    session.name = me.1;
    session.username = me.2;
    Ok(session)
}

pub fn refresh(config: &AppConfig, session: &Session) -> Result<Session, XError> {
    let refresh_token = session
        .refresh_token
        .clone()
        .ok_or_else(|| XError::message("no refresh token; sign in again"))?;
    let client = http()?;
    let request = client.post(TOKEN_URL).form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", config.client_id.as_str()),
    ]);
    let response = send(request)?;
    let status = response.status().as_u16();
    let body = response.text().unwrap_or_default();
    if status >= 400 {
        return Err(XError::Api {
            status,
            body: trim_body(&body),
        });
    }
    let tokens: TokenResponse = serde_json::from_str(&body).map_err(|err| XError::message(err.to_string()))?;
    Ok(Session {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token.or(session.refresh_token.clone()),
        expires_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + tokens.expires_in.unwrap_or(7200),
        user_id: session.user_id.clone(),
        username: session.username.clone(),
        name: session.name.clone(),
    })
}

fn exchange(config: &AppConfig, redirect: &str, verifier: &str, code: &str) -> Result<TokenResponse, XError> {
    let client = http()?;
    let request = client.post(TOKEN_URL).form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect),
        ("code_verifier", verifier),
        ("client_id", config.client_id.as_str()),
    ]);
    let response = send(request)?;
    let status = response.status().as_u16();
    let body = response.text().unwrap_or_default();
    if status >= 400 {
        return Err(XError::Api {
            status,
            body: trim_body(&body),
        });
    }
    serde_json::from_str(&body).map_err(|err| XError::message(format!("token response: {err}")))
}

fn users_me(token: &str) -> Result<(String, String, String), XError> {
    let client = http()?;
    let response = client
        .get("https://api.x.com/2/users/me")
        .query(&[("user.fields", "id,name,username")])
        .bearer_auth(token);
    let response = send(response)?;
    let status = response.status().as_u16();
    let body = response.text().unwrap_or_default();
    if status >= 400 {
        return Err(XError::Api {
            status,
            body: trim_body(&body),
        });
    }
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|err| XError::message(err.to_string()))?;
    let data = value.get("data").unwrap_or(&value);
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if id.is_empty() {
        return Err(XError::message("users/me did not return an id"));
    }
    let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let username = data.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string();
    Ok((id, name, username))
}

fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String, XError> {
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                let query = read_query(stream)?;
                // Browsers also request /favicon.ico. Keep waiting for the redirect.
                if !query.split('?').next().unwrap_or("").starts_with("/callback") {
                    continue;
                }
                let returned = query_param(&query, "state").unwrap_or_default();
                if returned != expected_state {
                    return Err(XError::message("OAuth state did not match"));
                }
                if let Some(error) = query_param(&query, "error") {
                    let description = query_param(&query, "error_description").unwrap_or_default();
                    return Err(XError::message(format!("X login: {error} {description}")));
                }
                return query_param(&query, "code").ok_or_else(|| XError::message("X login returned no code"));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(XError::message(err.to_string())),
        }
    }
    Err(XError::message("timed out waiting for the X login redirect"))
}

fn open_browser(url: &str) -> Result<(), XError> {
    let path = std::env::temp_dir().join("xbroadcaster-signin.url");
    let file = format!("[InternetShortcut]\r\nURL={url}\r\n");
    std::fs::write(&path, file).map_err(|err| XError::message(format!("sign-in shortcut: {err}")))?;
    std::process::Command::new("explorer.exe")
        .arg(&path)
        .spawn()
        .map_err(|err| XError::message(format!("open browser: {err}")))?;
    Ok(())
}

fn read_query(mut stream: TcpStream) -> Result<String, XError> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < 16 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(2).any(|pair| pair == b"\r\n") {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock || err.kind() == std::io::ErrorKind::TimedOut => {
                break;
            }
            Err(err) => return Err(XError::message(err.to_string())),
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let line = request.lines().next().unwrap_or("");
    let query = line.split_whitespace().nth(1).unwrap_or("/");
    let page = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><html><body style=\"font-family:sans-serif;background:#111;color:#eee;padding:40px\"><h2>Signed in</h2><p>You can close this tab and return to XBroadcaster.</p></body></html>";
    let _ = stream.write_all(page);
    Ok(query.to_string())
}

fn query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("00");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Windows sometimes refuses the first outbound connect (error 10013) when the
/// chosen local port is in an excluded range. A second attempt uses a new port.
fn send(request: reqwest::blocking::RequestBuilder) -> Result<reqwest::blocking::Response, XError> {
    let mut last = String::from("request failed");
    for attempt in 0..3 {
        let Some(retry) = request.try_clone() else {
            return request.send().map_err(|err| XError::message(err.to_string()));
        };
        match retry.send() {
            Ok(response) => return Ok(response),
            Err(err) => {
                last = err.to_string();
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
    Err(XError::message(last))
}

fn http() -> Result<reqwest::blocking::Client, XError> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| XError::message(err.to_string()))
}

fn code_verifier() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("random");
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn trim_body(body: &str) -> String {
    let mut text = body.chars().take(500).collect::<String>();
    if body.len() > 500 {
        text.push('…');
    }
    text
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_rfc_7636() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(code_challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
