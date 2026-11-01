//! Thin fetch wrapper. The session cookie is HttpOnly (JS never sees it); the CSRF token returned
//! by login is echoed on every request.

use gloo_net::http::{Request, Response};
use rustle_types::ApiError;
use serde::de::DeserializeOwned;
use std::cell::RefCell;

#[derive(Clone, Debug, PartialEq)]
pub struct ApiErr {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ApiErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

thread_local! {
    static CSRF: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn set_csrf(v: String) {
    CSRF.with(|c| *c.borrow_mut() = v);
}

fn net(e: impl std::fmt::Display) -> ApiErr {
    ApiErr { status: 0, code: "network".into(), message: format!("network error: {e}") }
}

async fn send(method: &str, path: &str, body: Option<serde_json::Value>) -> Result<Response, ApiErr> {
    let b = match method {
        "POST" => Request::post(path),
        "PUT" => Request::put(path),
        "PATCH" => Request::patch(path),
        "DELETE" => Request::delete(path),
        _ => Request::get(path),
    };
    let csrf = CSRF.with(|c| c.borrow().clone());
    let b = b.header("x-csrf-token", &csrf);
    let req = match body {
        Some(v) => b.json(&v).map_err(net)?,
        None => b.build().map_err(net)?,
    };
    let resp = req.send().await.map_err(net)?;
    if resp.ok() {
        return Ok(resp);
    }
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let (code, message) = match serde_json::from_str::<ApiError>(&text) {
        Ok(e) => (e.error.code, e.error.message),
        Err(_) => ("http".to_string(), format!("request failed ({status})")),
    };
    Err(ApiErr { status, code, message })
}

pub async fn get<T: DeserializeOwned>(path: &str) -> Result<T, ApiErr> {
    send("GET", path, None).await?.json::<T>().await.map_err(net)
}

pub async fn call<T: DeserializeOwned>(method: &str, path: &str, body: serde_json::Value) -> Result<T, ApiErr> {
    send(method, path, Some(body)).await?.json::<T>().await.map_err(net)
}

/// For endpoints that answer 204/empty.
pub async fn call_unit(method: &str, path: &str, body: Option<serde_json::Value>) -> Result<(), ApiErr> {
    send(method, path, body).await.map(|_| ())
}

pub fn js_json<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}
