//! The chat relay: `POST /api/chat` goes to Halogen's `/v1/chat/completions`, and the answer (usually a
//! server-sent-event stream) comes back as it is produced. The Halogen API key, if any, is added here and never reaches
//! the browser. Nothing is stored or logged: the conversation lives in the browser.

use super::{Config, connect, json, response_head};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// Largest chat request accepted (images travel as base64 inside it).
const MAX_BODY: usize = 48 << 20;
/// Chat streams relayed at once; Halogen queues the rest itself, this only bounds threads and memory here.
const MAX_ACTIVE: usize = 8;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// A parsed request head: request line, headers (names lowercased) and the body bytes already read with it.
pub struct Head {
    pub method: String,
    pub target: String,
    headers: Vec<(String, String)>,
    leftover: Vec<u8>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub fn read_head(stream: &mut TcpStream) -> Option<Head> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16 * 1024 {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(k) => buf.extend_from_slice(&chunk[..k]),
        }
    };
    let text = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = text.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next().unwrap_or("/").to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some(Head {
        method,
        target,
        headers,
        leftover: buf[end + 4..].to_vec(),
    })
}

struct Slot;

impl Slot {
    fn take() -> Option<Slot> {
        if ACTIVE.fetch_add(1, Ordering::SeqCst) >= MAX_ACTIVE {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

fn read_body(client: &mut TcpStream, head: &Head, max: usize) -> Result<Vec<u8>, (&'static str, &'static str)> {
    let len: usize = head.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len == 0 {
        return Err(("400 Bad Request", "empty body"));
    }
    if len > max {
        return Err(("413 Payload Too Large", "request too large"));
    }
    let mut body = head.leftover.clone();
    body.truncate(len);
    client.set_read_timeout(Some(Duration::from_secs(30))).ok();
    if body.len() < len {
        let mut rest = vec![0u8; len - body.len()];
        client
            .read_exact(&mut rest)
            .map_err(|_| ("400 Bad Request", "incomplete body"))?;
        body.extend_from_slice(&rest);
    }
    Ok(body)
}

// ------------------------------------------------------------------------------------------------ chat password --

/// Sessions opened by a correct password: random ids, kept in memory only (a restart logs everyone out).
static SESSIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());
const MAX_SESSIONS: usize = 64;
const COOKIE: &str = "hui_chat";

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn cookie(head: &Head) -> Option<&str> {
    head.header("cookie")?
        .split(';')
        .filter_map(|kv| kv.trim().strip_prefix(COOKIE)?.strip_prefix('='))
        .next()
}

/// With no password configured the chat is open; otherwise a session cookie (or `Authorization: Bearer <password>`,
/// for scripts) is needed.
fn authorized(cfg: &Config, head: &Head) -> bool {
    let Some(token) = &cfg.chat_token else { return true };
    if head
        .header("authorization")
        .and_then(|a| a.strip_prefix("Bearer "))
        .is_some_and(|t| same(t.trim().as_bytes(), token.as_bytes()))
    {
        return true;
    }
    cookie(head).is_some_and(|c| {
        SESSIONS
            .lock()
            .map(|s| s.iter().any(|x| same(x.as_bytes(), c.as_bytes())))
            .unwrap_or(false)
    })
}

fn random_id() -> Option<String> {
    let mut b = [0u8; 24];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut b).ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// `GET /api/chat-login`: does the chat ask for a password, and is this browser already in?
pub fn auth_state(mut client: TcpStream, cfg: &Config, head: &Head) {
    let body = format!(
        "{{\"required\":{},\"ok\":{}}}",
        cfg.chat_token.is_some(),
        authorized(cfg, head)
    );
    super::respond(&mut client, "200 OK", "application/json", body.as_bytes(), false);
}

/// `POST /api/chat-login`: the body is the password; the answer sets the session cookie.
pub fn login(mut client: TcpStream, cfg: &Config, head: &Head) {
    let Some(token) = &cfg.chat_token else {
        return error(&mut client, "400 Bad Request", "no password is set");
    };
    if head.header("sec-fetch-site") == Some("cross-site") {
        return error(&mut client, "403 Forbidden", "same-origin requests only");
    }
    let body = match read_body(&mut client, head, 1024) {
        Ok(b) => b,
        Err((code, msg)) => return error(&mut client, code, msg),
    };
    if !same(&body, token.as_bytes()) {
        thread::sleep(Duration::from_secs(1)); // slows guessing down: one try per second per connection
        return error(&mut client, "401 Unauthorized", "wrong password");
    }
    let Some(id) = random_id() else {
        return error(&mut client, "500 Internal Server Error", "no randomness available");
    };
    if let Ok(mut s) = SESSIONS.lock() {
        if s.len() >= MAX_SESSIONS {
            s.remove(0);
        }
        s.push(id.clone());
    }
    let extra =
        format!("Set-Cookie: {COOKIE}={id}; HttpOnly; SameSite=Strict; Max-Age=2592000\r\nContent-Length: 2\r\n");
    let head = response_head("200 OK", "application/json", false, &extra);
    let _ = client.write_all(head.as_bytes());
    let _ = client.write_all(b"\r\n{}");
}

fn error(stream: &mut TcpStream, code: &str, msg: &str) {
    let body = format!("{{\"error\":{{\"message\":{}}}}}", json::escape(msg));
    super::respond(stream, code, "application/json", body.as_bytes(), false);
}

pub fn handle(mut client: TcpStream, cfg: &Config, head: &Head) {
    // A cross-site page must not be able to make a visitor's browser use the model: refuse anything that is not
    // same-origin JSON (a JSON content type forces a CORS preflight, which this server never grants).
    let json_body = head
        .header("content-type")
        .is_some_and(|c| c.to_ascii_lowercase().starts_with("application/json"));
    if head.header("sec-fetch-site") == Some("cross-site") || !json_body {
        return error(&mut client, "403 Forbidden", "same-origin JSON requests only");
    }
    if !authorized(cfg, head) {
        return error(&mut client, "401 Unauthorized", "password required");
    }
    let body = match read_body(&mut client, head, MAX_BODY) {
        Ok(b) => b,
        Err((code, msg)) => return error(&mut client, code, msg),
    };
    let Some(_slot) = Slot::take() else {
        return error(
            &mut client,
            "429 Too Many Requests",
            "too many chats at once, try again in a moment",
        );
    };

    let (mut up, hostport, base) = match connect(cfg) {
        Ok(c) => c,
        Err(e) => return error(&mut client, "502 Bad Gateway", &format!("cannot reach Halogen: {e}")),
    };
    up.set_write_timeout(Some(Duration::from_secs(30))).ok();
    // Halogen may hold a request in its queue for a long time before the first byte.
    up.set_read_timeout(Some(Duration::from_secs(3700))).ok();
    let auth = cfg
        .key
        .as_ref()
        .map(|k| format!("Authorization: Bearer {k}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {base}/v1/chat/completions HTTP/1.0\r\nHost: {hostport}\r\n{auth}Content-Type: application/json\r\n\
         Accept: text/event-stream, application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if up.write_all(req.as_bytes()).and_then(|_| up.write_all(&body)).is_err() {
        return error(&mut client, "502 Bad Gateway", "cannot send the request to Halogen");
    }

    // The browser stopping (Stop button, closed tab) must stop the generation even while Halogen is silent (reading a
    // long prompt, waiting in its queue): a watcher notices the hang-up and closes the Halogen side.
    client.set_read_timeout(None).ok();
    if let (Ok(mut watch), Ok(up_watch)) = (client.try_clone(), up.try_clone()) {
        thread::spawn(move || {
            let mut b = [0u8; 64];
            while matches!(watch.read(&mut b), Ok(n) if n > 0) {}
            let _ = up_watch.shutdown(Shutdown::Both);
        });
    }
    client.set_write_timeout(Some(Duration::from_secs(30))).ok();

    // Halogen's answer head, then its body unchanged (including chunked framing, if it uses any).
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 16 * 1024];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 64 * 1024 {
            return error(&mut client, "502 Bad Gateway", "malformed answer from Halogen");
        }
        match up.read(&mut chunk) {
            Ok(0) | Err(_) => {
                return error(
                    &mut client,
                    "502 Bad Gateway",
                    "Halogen closed the connection without answering",
                );
            }
            Ok(k) => buf.extend_from_slice(&chunk[..k]),
        }
    };
    let text = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = text.lines();
    let status = lines
        .next()
        .unwrap_or("")
        .split_once(' ')
        .map_or("502 Bad Gateway", |(_, s)| s)
        .to_string();
    let (mut ctype, mut extra) = ("application/json".to_string(), String::new());
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-type" => ctype = v.trim().to_string(),
                "transfer-encoding" if v.to_ascii_lowercase().contains("chunked") => {
                    extra.push_str("Transfer-Encoding: chunked\r\n")
                }
                "content-length" => extra.push_str(&format!("Content-Length: {}\r\n", v.trim())),
                _ => {}
            }
        }
    }
    extra.push_str("X-Accel-Buffering: no\r\n");
    let out = response_head(&status, &ctype, false, &extra);
    if client
        .write_all(out.as_bytes())
        .and_then(|_| client.write_all(b"\r\n"))
        .is_err()
    {
        return;
    }
    if client.write_all(&buf[end + 4..]).is_err() {
        return;
    }
    loop {
        match up.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                if client.write_all(&chunk[..k]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = up.shutdown(Shutdown::Both);
    let _ = client.shutdown(Shutdown::Both);
}
