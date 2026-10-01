//! Loopback HTTP API. Every route requires the bearer token from
//! `runtime.json`; the server binds 127.0.0.1 only.
//!
//! Mutating requests are forwarded to the sampler thread as `Command`s so all
//! state changes happen in one place, and the caller waits for the answer.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tiny_http::{Header, Method, Response, Server};

use crate::config::Config;
use crate::model::Snapshot;

pub type Shared = Arc<RwLock<Arc<Snapshot>>>;

pub enum Command {
    SetMode {
        mode: String,
        reply: Sender<Result<String, String>>,
    },
    Kill {
        pid: i32,
        force: bool,
        reply: Sender<Result<String, String>>,
    },
    Protect {
        pid: Option<i32>,
        prefix: Option<String>,
        reply: Sender<Result<String, String>>,
    },
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

fn cors_header() -> Header {
    Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap()
}

fn respond_json(request: tiny_http::Request, code: u16, body: String) {
    let resp = Response::from_string(body)
        .with_status_code(code)
        .with_header(json_header())
        .with_header(cors_header());
    let _ = request.respond(resp);
}

fn authorized(request: &tiny_http::Request, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    let want = format!("Bearer {token}");
    request
        .headers()
        .iter()
        .any(|h| h.field.equiv("Authorization") && h.value.as_str() == want)
}

pub fn run(
    server: Server,
    state: Shared,
    tx: Sender<Command>,
    config: Arc<RwLock<Config>>,
    config_path: String,
) {
    for mut request in server.incoming_requests() {
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();
        let method = request.method().clone();

        // CORS preflight (for a browser debugging the API directly).
        if method == Method::Options {
            respond_json(request, 204, String::new());
            continue;
        }

        let token = config.read().map(|c| c.token.clone()).unwrap_or_default();
        if !authorized(&request, &token) {
            respond_json(request, 401, r#"{"error":"unauthorized"}"#.to_string());
            continue;
        }

        match (method, path.as_str()) {
            (Method::Get, "/api/health") => {
                let snap = state.read().map(|s| s.clone()).ok();
                let body = match snap {
                    Some(s) => format!(
                        r#"{{"ok":true,"version":"{}","pid":{},"uptime_s":{:.1},"mode":"{}"}}"#,
                        s.version, s.pid, s.uptime_s, s.mode
                    ),
                    None => r#"{"ok":true}"#.to_string(),
                };
                respond_json(request, 200, body);
            }
            (Method::Get, "/api/snapshot") => {
                let body = state
                    .read()
                    .ok()
                    .and_then(|s| serde_json::to_string(&**s).ok())
                    .unwrap_or_else(|| r#"{"error":"no snapshot yet"}"#.to_string());
                respond_json(request, 200, body);
            }
            (Method::Get, "/api/config") => {
                let body = config
                    .read()
                    .ok()
                    .and_then(|c| serde_json::to_string(&*c).ok())
                    .unwrap_or_else(|| "{}".to_string());
                respond_json(request, 200, body);
            }
            (Method::Get, "/api/info") => {
                let body = format!(
                    r#"{{"version":"{}","config_path":"{}"}}"#,
                    crate::config::VERSION,
                    config_path
                );
                respond_json(request, 200, body);
            }
            (Method::Post, "/api/mode") => {
                let body = read_body(&mut request);
                let mode = extract_string(&body, "mode").unwrap_or_default();
                if mode != "armed" && mode != "observe" {
                    respond_json(request, 400, err("mode must be \"armed\" or \"observe\""));
                    continue;
                }
                let (rtx, rrx) = std::sync::mpsc::channel();
                let _ = tx.send(Command::SetMode { mode, reply: rtx });
                reply(request, rrx);
            }
            (Method::Post, "/api/kill") => {
                let body = read_body(&mut request);
                let pid = match extract_i64(&body, "pid") {
                    Some(p) => p as i32,
                    None => {
                        respond_json(request, 400, err("pid required"));
                        continue;
                    }
                };
                let force = extract_bool(&body, "force").unwrap_or(false);
                let (rtx, rrx) = std::sync::mpsc::channel();
                let _ = tx.send(Command::Kill {
                    pid,
                    force,
                    reply: rtx,
                });
                reply(request, rrx);
            }
            (Method::Post, "/api/protect") => {
                let body = read_body(&mut request);
                let pid = extract_i64(&body, "pid").map(|p| p as i32);
                let prefix = extract_string(&body, "prefix");
                if pid.is_none() && prefix.is_none() {
                    respond_json(request, 400, err("pid or prefix required"));
                    continue;
                }
                let (rtx, rrx) = std::sync::mpsc::channel();
                let _ = tx.send(Command::Protect {
                    pid,
                    prefix,
                    reply: rtx,
                });
                reply(request, rrx);
            }
            _ => {
                respond_json(request, 404, err("not found"));
            }
        }
    }
}

fn reply(request: tiny_http::Request, rx: Receiver<Result<String, String>>) {
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(msg)) => respond_json(request, 200, ok(&msg)),
        Ok(Err(msg)) => respond_json(request, 409, err(&msg)),
        Err(_) => respond_json(request, 504, err("sampler did not answer")),
    }
}

fn ok(msg: &str) -> String {
    serde_json::json!({ "ok": true, "message": msg }).to_string()
}

fn err(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}

fn read_body(request: &mut tiny_http::Request) -> String {
    let mut s = String::new();
    let _ = request.as_reader().read_to_string(&mut s);
    s
}

fn extract_string(body: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string()))
}

fn extract_i64(body: &str, key: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get(key).and_then(|x| x.as_i64()))
}

fn extract_bool(body: &str, key: &str) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get(key).and_then(|x| x.as_bool()))
}
