//! halogen-ui — a live web monitor for a Halogen inference server.
//!
//! Reads three sources, all read-only:
//! - Halogen's `/metrics` and `/health` (state, averages, KV pool, prompt cache, speculative decoding);
//! - Halogen's log, followed through a command (`journalctl -f …`, `docker logs -f …`): one line per finished request
//!   and progress lines while a request runs;
//! - the host's sensors (amdgpu sysfs, `nvidia-smi`, /proc).
//!
//! and serves one page plus `/api/state` (JSON). No prompt or answer text is ever read: the log only carries counts,
//! so the page can be shared publicly.
//!
//! With `--chat` it also serves a chat page (`/chat`) and relays `POST /api/chat` to Halogen's
//! `/v1/chat/completions`, streaming. Nothing is stored or logged here; the conversation lives in the browser.

mod chat;
mod json;
mod parse;
mod sensors;

use json::Obj;
use parse::{Event, Progress, Request};
use sensors::{Host, Sensors};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE: &str = include_str!("../web/style.css");
const FAVICON: &str = include_str!("../web/favicon.svg");
const CHAT_HTML: &str = include_str!("../web/chat.html");
const CHAT_JS: &str = include_str!("../web/chat.js");
const CHAT_CSS: &str = include_str!("../web/chat.css");
const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "halogen-ui — live web monitor for a Halogen inference server

USAGE: halogen-ui [options]

  --listen ADDR       address to serve the page on     (HUI_LISTEN, default 127.0.0.1:8790)
  --halogen URL       Halogen base URL                 (HUI_HALOGEN, default http://127.0.0.1:8731)
  --log-cmd CMD       command printing Halogen's log and following it
                      (HUI_LOG_CMD, default \"docker logs -f -t --tail 3000 halogen\")
  --title TEXT        name shown in the page header    (HUI_TITLE, default Halogen)
  --keep N            finished requests kept in memory (HUI_KEEP, default 500)
  --chat              also serve a chat page (/chat) that talks to the model (HUI_CHAT=1). Off by default: the monitor
                      is read-only, the chat lets anyone who can open the page use the model
  -h, --help          this help

The Halogen API key, if the server requires one, is read from HUI_HALOGEN_KEY (never from the command line).
Examples of --log-cmd:
  journalctl -u halogen.service -f -n 3000 -o short-unix
  podman logs -f -t --tail 3000 halogen
";

struct Config {
    listen: String,
    halogen: String,
    key: Option<String>,
    log_cmd: String,
    title: String,
    keep: usize,
    chat: bool,
    /// Password of the chat, if any (HUI_CHAT_TOKEN, never on the command line).
    chat_token: Option<String>,
}

fn config() -> Config {
    let env = |k: &str, d: &str| {
        std::env::var(k)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| d.to_string())
    };
    let mut c = Config {
        listen: env("HUI_LISTEN", "127.0.0.1:8790"),
        halogen: env("HUI_HALOGEN", "http://127.0.0.1:8731"),
        key: std::env::var("HUI_HALOGEN_KEY").ok().filter(|v| !v.is_empty()),
        log_cmd: env("HUI_LOG_CMD", "docker logs -f -t --tail 3000 halogen"),
        title: env("HUI_TITLE", "Halogen"),
        keep: env("HUI_KEEP", "500").parse().unwrap_or(500),
        chat: matches!(env("HUI_CHAT", "").as_str(), "1" | "true" | "yes" | "on"),
        chat_token: std::env::var("HUI_CHAT_TOKEN").ok().filter(|v| !v.is_empty()),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("halogen-ui: {name} needs a value");
                std::process::exit(2)
            })
        };
        match a.as_str() {
            "--listen" => c.listen = val("--listen"),
            "--halogen" => c.halogen = val("--halogen"),
            "--log-cmd" => c.log_cmd = val("--log-cmd"),
            "--title" => c.title = val("--title"),
            "--keep" => c.keep = val("--keep").parse().unwrap_or(500),
            "--chat" => c.chat = true,
            "-V" | "--version" => {
                println!("halogen-ui {VERSION}");
                std::process::exit(0)
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0)
            }
            _ => {
                eprintln!("halogen-ui: unknown option {a}\n\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    c.halogen = c.halogen.trim_end_matches('/').to_string();
    c.keep = c.keep.clamp(10, 100_000);
    c
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Clone, Default)]
struct Sample {
    t: f64,
    gpu_busy: Option<f64>,
    gpu_temp: Option<f64>,
    power: Option<f64>,
    cpu: Option<f64>,
    mem_used: Option<u64>,
    gpu_mem_used: Option<u64>,
    pool: Option<f64>,
}

#[derive(Default)]
struct State {
    reqs: VecDeque<Request>,
    live: Option<(Progress, f64)>,
    disconnects: u64,
    version: Option<String>,
    log_ok: bool,
    log_err: String,
    log_lines: u64,
    metrics: HashMap<String, f64>,
    build: HashMap<String, String>,
    metrics_ok: bool,
    metrics_err: String,
    health: HashMap<String, String>,
    cache: HashMap<String, String>,
    /// Time of the `serve_api: version` line: when Halogen (re)started, if the log goes back that far.
    started: Option<f64>,
    host: Host,
    samples: VecDeque<Sample>,
}

type Shared = Arc<Mutex<State>>;

// ------------------------------------------------------------------------------------------------ log follower --

fn follow_log(cfg: &Config, st: &Shared) {
    loop {
        let child = Command::new("sh")
            .arg("-c")
            .arg(format!("exec {} 2>&1", cfg.log_cmd))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                set_log_error(st, format!("cannot start the log command: {e}"));
                thread::sleep(Duration::from_secs(10));
                continue;
            }
        };
        {
            let mut s = st.lock().unwrap();
            s.reqs.clear(); // the command prints its backlog again: start from it
            s.disconnects = 0;
            s.log_ok = true;
            s.log_err.clear();
        }
        let mut last_lines = VecDeque::with_capacity(4);
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if last_lines.len() == 4 {
                    last_lines.pop_front();
                }
                last_lines.push_back(line.chars().take(200).collect::<String>());
                let (ts, msg) = parse::split_timestamp(&line);
                let t = ts.unwrap_or_else(now);
                let ev = parse::parse_message(msg, t);
                let mut s = st.lock().unwrap();
                s.log_lines += 1;
                match ev {
                    Some(Event::Done(r)) => {
                        s.live = None;
                        s.reqs.push_back(r);
                        while s.reqs.len() > cfg.keep {
                            s.reqs.pop_front();
                        }
                    }
                    Some(Event::Progress(p)) => s.live = Some((p, t)),
                    Some(Event::Disconnect) => s.disconnects += 1,
                    Some(Event::Version(v)) => {
                        s.version = Some(v);
                        s.started = Some(t);
                    }
                    None => {}
                }
            }
        }
        let status = child.wait().map(|s| s.to_string()).unwrap_or_default();
        let tail = last_lines.into_iter().collect::<Vec<_>>().join(" / ");
        set_log_error(
            st,
            format!(
                "the log command stopped ({status}){}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {tail}")
                }
            ),
        );
        thread::sleep(Duration::from_secs(5));
    }
}

fn set_log_error(st: &Shared, e: String) {
    let mut s = st.lock().unwrap();
    s.log_ok = false;
    s.log_err = e;
}

// ------------------------------------------------------------------------------------------ Halogen HTTP client --

/// Connect to Halogen: the socket, its `host:port` and the base path of the URL. Plain HTTP only (Halogen listens on
/// HTTP; put TLS in front of it, not here).
fn connect(cfg: &Config) -> Result<(TcpStream, String, String), String> {
    let rest = cfg
        .halogen
        .strip_prefix("http://")
        .ok_or("only http:// Halogen URLs are supported")?;
    let (hostport, base) = match rest.split_once('/') {
        Some((h, b)) => (h, format!("/{b}")),
        None => (rest, String::new()),
    };
    let addr = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    let sa = addr
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no address")?;
    let s = TcpStream::connect_timeout(&sa, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    Ok((s, hostport.to_string(), base))
}

/// GET `path` on Halogen with a 3 s budget.
fn http_get(cfg: &Config, path: &str) -> Result<String, String> {
    let (mut s, hostport, base) = connect(cfg)?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    s.set_write_timeout(Some(Duration::from_secs(3))).ok();
    let auth = cfg
        .key
        .as_ref()
        .map(|k| format!("Authorization: Bearer {k}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "GET {base}{path} HTTP/1.0\r\nHost: {hostport}\r\n{auth}Accept: */*\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    s.take(4 << 20).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n").ok_or("malformed HTTP answer")?;
    let code = head.split_whitespace().nth(1).unwrap_or("?");
    if code != "200" {
        return Err(format!("HTTP {code} on {path}"));
    }
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        return Ok(dechunk(body));
    }
    Ok(body.to_string())
}

fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let n = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        if n == 0 || rest.len() < n {
            break;
        }
        out.push_str(&rest[..n]);
        body = rest[n..].trim_start_matches("\r\n");
    }
    out
}

/// Scalar fields of a JSON document, flattened with dotted paths (`disk.bytes`); arrays are skipped.
/// String escapes are kept as is. Enough for Halogen's /health and /cache, without a JSON crate.
fn json_flat(text: &str) -> HashMap<String, String> {
    fn ws(b: &[u8], mut i: usize) -> usize {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    }
    fn string(t: &str, i: usize) -> Option<(String, usize)> {
        let b = t.as_bytes();
        let mut j = i + 1;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'"' => return Some((t[i + 1..j].to_string(), j + 1)),
                _ => j += 1,
            }
        }
        None
    }
    /// Parses the value at `i`; records scalars under `path` when `keep`. Returns the index after the value.
    fn value(
        t: &str,
        i: usize,
        path: &str,
        keep: bool,
        out: &mut HashMap<String, String>,
        depth: u32,
    ) -> Option<usize> {
        let b = t.as_bytes();
        let i = ws(b, i);
        if depth > 32 {
            return None;
        }
        match *b.get(i)? {
            b'{' => {
                let mut j = ws(b, i + 1);
                if b.get(j) == Some(&b'}') {
                    return Some(j + 1);
                }
                loop {
                    let (k, after) = string(t, ws(b, j))?;
                    let c = ws(b, after);
                    if b.get(c) != Some(&b':') {
                        return None;
                    }
                    let sub = if path.is_empty() { k } else { format!("{path}.{k}") };
                    j = ws(b, value(t, c + 1, &sub, keep, out, depth + 1)?);
                    match b.get(j)? {
                        b',' => j += 1,
                        b'}' => return Some(j + 1),
                        _ => return None,
                    }
                }
            }
            b'[' => {
                let mut j = ws(b, i + 1);
                if b.get(j) == Some(&b']') {
                    return Some(j + 1);
                }
                loop {
                    j = ws(b, value(t, j, path, false, out, depth + 1)?);
                    match b.get(j)? {
                        b',' => j += 1,
                        b']' => return Some(j + 1),
                        _ => return None,
                    }
                }
            }
            b'"' => {
                let (v, j) = string(t, i)?;
                if keep {
                    out.insert(path.to_string(), v);
                }
                Some(j)
            }
            _ => {
                let end = t[i..].find([',', '}', ']']).map(|e| i + e).unwrap_or(t.len());
                if keep {
                    out.insert(path.to_string(), t[i..end].trim().to_string());
                }
                Some(end)
            }
        }
    }
    let mut out = HashMap::new();
    value(text, 0, "", true, &mut out, 0);
    out
}

fn sampler(cfg: &Config, st: &Shared) {
    let mut sensors = Sensors::detect();
    let mut n = 0u64;
    loop {
        let host = sensors.read();
        let metrics = http_get(cfg, "/metrics");
        let cache = http_get(cfg, "/cache").ok();
        let health = if n.is_multiple_of(5) {
            http_get(cfg, "/health").ok()
        } else {
            None
        };
        n += 1;
        let t = now();
        let mut s = st.lock().unwrap();
        match metrics {
            Ok(text) => {
                let (m, b) = parse::parse_metrics(&text);
                s.metrics = m;
                if !b.is_empty() {
                    s.build = b;
                }
                s.metrics_ok = true;
                s.metrics_err.clear();
            }
            Err(e) => {
                s.metrics_ok = false;
                s.metrics_err = e;
            }
        }
        if let Some(h) = health {
            s.health = json_flat(&h);
        }
        s.cache = cache.map(|c| json_flat(&c)).unwrap_or_default();
        let avg = |v: Vec<f64>| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
        let sample = Sample {
            t,
            gpu_busy: avg(host.gpus.iter().filter_map(|g| g.busy_pct).collect()),
            gpu_temp: host.gpus.iter().filter_map(|g| g.temp_c).reduce(f64::max),
            power: {
                let p: Vec<f64> = host.gpus.iter().filter_map(|g| g.power_w).collect();
                (!p.is_empty()).then(|| p.iter().sum())
            },
            cpu: host.cpu_pct,
            mem_used: host.mem_used,
            gpu_mem_used: {
                let m: Vec<u64> = host.gpus.iter().filter_map(|g| g.mem_used).collect();
                (!m.is_empty()).then(|| m.iter().sum())
            },
            pool: s.metrics.get("llamacpp:kv_cache_usage_ratio").map(|x| x * 100.0),
        };
        s.samples.push_back(sample);
        while s.samples.len() > 300 {
            s.samples.pop_front();
        }
        s.host = host;
        drop(s);
        thread::sleep(Duration::from_secs(2));
    }
}

// ------------------------------------------------------------------------------------------------------ JSON API --

fn req_json(r: &Request) -> String {
    let mut o = Obj::new();
    o.num("t", r.t)
        .str("mode", &r.mode)
        .int("prompt", r.prompt)
        .int("cached", r.cached)
        .int("new", r.new_tokens)
        .int("output", r.output)
        .opt("prefill_tps", r.prefill_tps())
        .num("decode_tps", r.decode_tps)
        .num("prefill_s", r.prefill_s)
        .num("decode_s", r.decode_s)
        .num("duration_s", r.duration_s())
        .opt("commit", r.commit_per_round)
        .opt("pool_used", r.pool_used.map(|x| x as f64))
        .opt("pool_total", r.pool_total.map(|x| x as f64));
    match r.think {
        Some(b) => o.bool("think", b),
        None => o.null("think"),
    };
    o.end()
}

fn state_json(cfg: &Config, st: &Shared, all: bool) -> String {
    let s = st.lock().unwrap();
    let t = now();
    let m = |k: &str| s.metrics.get(k).copied();
    let engine_up = m("halogen:engine_up").map(|x| x > 0.0).unwrap_or(s.metrics_ok);
    let processing = m("llamacpp:requests_processing").unwrap_or(0.0);
    let deferred =
        m("llamacpp:requests_deferred").unwrap_or(0.0) + m("halogen:kv_pool_waiting_for_room").unwrap_or(0.0);
    let busy_for = m("halogen:busy_for_seconds").unwrap_or(0.0);
    let live = s.live.as_ref().filter(|(_, lt)| t - lt < 90.0);

    let mut status = Obj::new();
    let phase = if !s.metrics_ok {
        "unreachable"
    } else if !engine_up {
        "down"
    } else if processing > 0.0 || busy_for > 0.0 || live.is_some() {
        match live {
            Some((Progress::Prefill { done, total, .. }, _)) if done < total => "reading",
            _ => "generating",
        }
    } else {
        "idle"
    };
    status
        .str("phase", phase)
        .bool("queued", deferred > 0.0)
        .num("waiting", deferred)
        .num("busy_for", busy_for);
    match live {
        Some((Progress::Prefill { req, done, total, secs }, lt)) => {
            status
                .int("req", *req)
                .int("done", *done)
                .int("total", *total)
                .num("secs", *secs)
                .num("seen", *lt);
        }
        Some((Progress::Generated { req, tokens, secs }, lt)) => {
            status
                .int("req", *req)
                .int("generated", *tokens)
                .num("secs", *secs)
                .num("seen", *lt);
        }
        None => {}
    }

    let mut metrics = Obj::new();
    for k in [
        "halogen:requests_total",
        "llamacpp:prompt_tokens_total",
        "llamacpp:tokens_predicted_total",
        "llamacpp:tokens_predicted_seconds_total",
        "llamacpp:prompt_seconds_total",
        "llamacpp:kv_cache_tokens",
        "llamacpp:kv_cache_usage_ratio",
        "halogen:kv_pool_positions",
        "halogen:cache_hits_total",
        "halogen:cache_misses_total",
        "halogen:cache_prompt_tokens_saved_total",
        "halogen:prompt_tokens_cached_total",
        "halogen:cache_entries",
        "halogen:cache_bytes",
        "halogen:cache_disk_bytes",
        "halogen:cache_disk_budget_bytes",
        "halogen:draft_tokens_total",
        "halogen:draft_tokens_accepted_total",
    ] {
        metrics.opt(k.split(':').nth(1).unwrap_or(k), m(k));
    }

    let mut model = Obj::new();
    let build = |k: &str| s.build.get(k).cloned();
    model
        .opt_str("name", build("model").or_else(|| s.health.get("model").cloned()))
        .opt_str("version", build("version").or_else(|| s.version.clone()))
        .opt("context", s.health.get("context").and_then(|x| x.parse().ok()));

    let mut host = Obj::new();
    let h = &s.host;
    let gpus: Vec<String> = h
        .gpus
        .iter()
        .map(|g| {
            let mut o = Obj::new();
            o.str("name", &g.name)
                .opt("busy", g.busy_pct)
                .opt("temp", g.temp_c)
                .opt("power", g.power_w)
                .opt("power_cap", g.power_cap_w)
                .opt("clock", g.clock_mhz)
                .opt("mem_used", g.mem_used.map(|x| x as f64))
                .opt("mem_total", g.mem_total.map(|x| x as f64));
            o.end()
        })
        .collect();
    host.raw("gpus", &json::array(&gpus))
        .opt("cpu", h.cpu_pct)
        .int("threads", h.cpu_threads as u64)
        .opt("mem_used", h.mem_used.map(|x| x as f64))
        .opt("mem_total", h.mem_total.map(|x| x as f64));

    let samples: Vec<String> = s
        .samples
        .iter()
        .map(|x| {
            let mut o = Obj::new();
            o.num("t", x.t)
                .opt("gpu", x.gpu_busy)
                .opt("temp", x.gpu_temp)
                .opt("power", x.power)
                .opt("cpu", x.cpu)
                .opt("mem", x.mem_used.map(|v| v as f64))
                .opt("gpu_mem", x.gpu_mem_used.map(|v| v as f64))
                .opt("pool", x.pool);
            o.end()
        })
        .collect();

    let mut cache = Obj::new();
    for k in [
        "entries",
        "max_entries",
        "bytes",
        "last_entry_bytes",
        "hits",
        "misses",
        "stores",
        "evicted",
        "superseded",
        "dropped",
        "refused",
        "prompt_tokens_saved",
        "store_ms_total",
        "restore_ms_total",
        "last_store_ms",
        "last_restore_ms",
        "hit_rate",
        "token_hit_rate",
        "disk.on",
        "disk.records",
        "disk.lineages",
        "disk.bytes",
        "disk.budget_bytes",
        "disk.hits",
        "disk.misses",
        "disk.evicted",
        "pool.used",
        "pool.positions",
        "pool.held_regions",
        "pool.waiting_for_room",
        "pool.relocated",
    ] {
        let v = s.cache.get(k).and_then(|x| match x.as_str() {
            "true" => Some(1.0),
            "false" => Some(0.0),
            x => x.parse::<f64>().ok(),
        });
        cache.opt(&k.replace('.', "_"), v);
    }
    let reused = s.reqs.iter().filter(|r| r.cached > 0).count() as u64;

    let n = if all { s.reqs.len() } else { s.reqs.len().min(20) };
    let reqs: Vec<String> = s.reqs.iter().rev().take(n).map(req_json).collect();

    let mut o = Obj::new();
    o.str("title", &cfg.title)
        .str("ui_version", VERSION)
        .num("now", t)
        .raw("status", &status.end())
        .raw("model", &model.end())
        .raw("metrics", &metrics.end())
        .raw("host", &host.end())
        .raw("samples", &json::array(&samples))
        .raw("requests", &json::array(&reqs))
        .raw("cache", &cache.end())
        .int("requests_kept", s.reqs.len() as u64)
        .int("requests_reused", reused)
        .opt("started", s.started)
        .int("disconnects", s.disconnects)
        .bool("log_ok", s.log_ok)
        .str("log_error", &s.log_err)
        .int("log_lines", s.log_lines)
        .bool("metrics_ok", s.metrics_ok)
        .str("metrics_error", &s.metrics_err);
    o.end()
}

// ------------------------------------------------------------------------------------------------- HTTP server --

/// Response head without the body framing: the caller adds `Content-Length`, or streams until the connection closes.
fn response_head(code: &str, ctype: &str, cache: bool, extra: &str) -> String {
    let csp = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; \
               base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
    let cache_hdr = if cache { "public, max-age=300" } else { "no-store" };
    format!(
        "HTTP/1.1 {code}\r\nContent-Type: {ctype}\r\nCache-Control: {cache_hdr}\r\n{extra}\
         Content-Security-Policy: {csp}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         X-Frame-Options: DENY\r\nConnection: close\r\n"
    )
}

fn respond(stream: &mut TcpStream, code: &str, ctype: &str, body: &[u8], cache: bool) {
    let head = response_head(code, ctype, cache, &format!("Content-Length: {}\r\n", body.len()));
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(b"\r\n");
    let _ = stream.write_all(body);
}

fn handle(mut stream: TcpStream, cfg: &Config, st: &Shared) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let Some(head) = chat::read_head(&mut stream) else {
        return;
    };
    let (path, query) = head.target.split_once('?').unwrap_or((head.target.as_str(), ""));
    if cfg.chat && path == "/api/chat" && head.method == "POST" {
        return chat::handle(stream, cfg, &head);
    }
    if cfg.chat && path == "/api/chat-login" {
        match head.method.as_str() {
            "POST" => return chat::login(stream, cfg, &head),
            "GET" => return chat::auth_state(stream, cfg, &head),
            _ => {}
        }
    }
    if head.method != "GET" && head.method != "HEAD" {
        let msg: &[u8] = if cfg.chat {
            b"read-only, except the chat\n"
        } else {
            b"read-only\n"
        };
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", msg, false);
    }
    match path {
        "/" | "/index.html" => {
            let link = if cfg.chat {
                r#"<a class="btn" href="chat">Chat</a>"#
            } else {
                ""
            };
            let page = INDEX
                .replace("{{TITLE}}", &json::html_escape(&cfg.title))
                .replace("{{CHAT_LINK}}", link);
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                page.as_bytes(),
                false,
            )
        }
        "/chat" | "/chat.html" if cfg.chat => {
            let page = CHAT_HTML.replace("{{TITLE}}", &json::html_escape(&cfg.title));
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                page.as_bytes(),
                false,
            )
        }
        "/chat.js" if cfg.chat => respond(
            &mut stream,
            "200 OK",
            "text/javascript; charset=utf-8",
            CHAT_JS.as_bytes(),
            true,
        ),
        "/chat.css" if cfg.chat => respond(
            &mut stream,
            "200 OK",
            "text/css; charset=utf-8",
            CHAT_CSS.as_bytes(),
            true,
        ),
        "/app.js" => respond(
            &mut stream,
            "200 OK",
            "text/javascript; charset=utf-8",
            APP_JS.as_bytes(),
            true,
        ),
        "/style.css" => respond(&mut stream, "200 OK", "text/css; charset=utf-8", STYLE.as_bytes(), true),
        "/favicon.svg" => respond(&mut stream, "200 OK", "image/svg+xml", FAVICON.as_bytes(), true),
        "/api/state" => {
            let all = query.split('&').any(|kv| kv == "all=1");
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                state_json(cfg, st, all).as_bytes(),
                false,
            )
        }
        "/healthz" => respond(&mut stream, "200 OK", "text/plain", b"ok\n", false),
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found\n", false),
    }
}

fn main() {
    let cfg = Arc::new(config());
    let st: Shared = Arc::new(Mutex::new(State::default()));
    let listener = TcpListener::bind(&cfg.listen).unwrap_or_else(|e| {
        eprintln!("halogen-ui: cannot listen on {}: {e}", cfg.listen);
        std::process::exit(1)
    });
    {
        let (c, s) = (cfg.clone(), st.clone());
        thread::spawn(move || follow_log(&c, &s));
    }
    {
        let (c, s) = (cfg.clone(), st.clone());
        thread::spawn(move || sampler(&c, &s));
    }
    eprintln!(
        "halogen-ui {VERSION}: http://{}  (Halogen {}, log: {})",
        cfg.listen, cfg.halogen, cfg.log_cmd
    );
    for stream in listener.incoming().flatten() {
        let (c, s) = (cfg.clone(), st.clone());
        thread::spawn(move || handle(stream, &c, &s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_json() {
        let h = json_flat(
            r#"{"status":"ok","model":"halogen-qwen3.8","endpoints":["/v1/x","/y"],"context":262144,"rope_scaling":null,"nested":{"a":"b"}}"#,
        );
        assert_eq!(h["status"], "ok");
        assert_eq!(h["model"], "halogen-qwen3.8");
        assert_eq!(h["context"], "262144");
        assert_eq!(h["rope_scaling"], "null");
        assert_eq!(h["nested.a"], "b");
        assert!(!h.contains_key("endpoints"));
        let c = json_flat(r#"{"entries":24,"disk":{"on":true,"bytes":113035388800},"snapshot_places":["a","b"]}"#);
        assert_eq!(c["disk.bytes"], "113035388800");
        assert_eq!(c["disk.on"], "true");
        assert_eq!(c["entries"], "24");
    }

    #[test]
    fn chunked() {
        assert_eq!(dechunk("4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n"), "Wikipedia");
    }
}
