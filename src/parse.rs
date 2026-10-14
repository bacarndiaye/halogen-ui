//! Parsing of Halogen's log lines and of its Prometheus `/metrics` page.
//!
//! Halogen prints one summary line per finished request, for example:
//!
//! ```text
//! serve_api: mtp 180 tok in 3.12s = 57.61 t/s | 75 rounds, commit 2.41/round | prompt 108508 (108096 cached, 99.6%),
//!   prefill 0.58s (412 new) | detok 14us/tok | pool 454144/655360 69% | think on
//! ```
//!
//! and progress lines while a request runs:
//!
//! ```text
//! flash_serve: req 532 prefill 84480/108114 tokens, 32.4 s
//! flash_serve: req 532 generated 2 tokens, 0.1 s
//! ```
//!
//! Lines may carry a timestamp prefix: `journalctl -o short-unix` (`1791936422.242338 host unit[pid]: …`) or
//! `docker logs -t` / `podman logs -t` (`2026-10-14T02:07:56.123456789Z …`).

use std::collections::HashMap;

/// One finished request, from a `serve_api: <mode> N tok in …` line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Request {
    pub t: f64,
    pub mode: String,
    pub output: u64,
    pub decode_s: f64,
    pub decode_tps: f64,
    pub prompt: u64,
    pub cached: u64,
    pub new_tokens: u64,
    pub prefill_s: f64,
    /// Prefill speed when Halogen prints it (`prefill 5.98s = 1862 t/s`) instead of the new-token count.
    pub prefill_rate: Option<f64>,
    pub commit_per_round: Option<f64>,
    pub pool_used: Option<u64>,
    pub pool_total: Option<u64>,
    pub think: Option<bool>,
}

impl Request {
    pub fn prefill_tps(&self) -> Option<f64> {
        if self.prefill_rate.is_some() {
            self.prefill_rate
        } else if self.prefill_s > 0.0 && self.new_tokens > 0 {
            Some(self.new_tokens as f64 / self.prefill_s)
        } else {
            None
        }
    }
    pub fn duration_s(&self) -> f64 {
        self.prefill_s + self.decode_s
    }
}

/// Progress of the request being served.
#[derive(Clone, Debug, PartialEq)]
pub enum Progress {
    Prefill { req: u64, done: u64, total: u64, secs: f64 },
    Generated { req: u64, tokens: u64, secs: f64 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Done(Request),
    Progress(Progress),
    /// A client hung up before the end of its answer.
    Disconnect,
    /// Server version, from `serve_api: version X, engine version Y`.
    Version(String),
}

/// Splits an optional timestamp prefix from a log line. Returns (epoch seconds if known, message).
pub fn split_timestamp(line: &str) -> (Option<f64>, &str) {
    let line = line.trim_end_matches(['\r', '\n']);
    let (first, rest) = match line.split_once(' ') {
        Some(x) => x,
        None => return (None, line),
    };
    // journalctl -o short-unix: "<epoch> <host> <unit>[<pid>]: <message>"
    if let Ok(t) = first.parse::<f64>()
        && t > 1.0e9
    {
        let msg = rest
            .split_once("]: ")
            .map(|(_, m)| m)
            .or_else(|| rest.split_once(": ").map(|(_, m)| m));
        return (Some(t), msg.unwrap_or(rest));
    }
    // docker/podman logs -t: RFC 3339
    if let Some(t) = parse_rfc3339(first) {
        return (Some(t), rest);
    }
    (None, line)
}

/// Minimal RFC 3339 parser (`2026-10-14T02:07:56.123Z`, `…+02:00`), enough for container log timestamps.
pub fn parse_rfc3339(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't') || b[13] != b':' {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        num(0, 4)?,
        num(5, 7)?,
        num(8, 10)?,
        num(11, 13)?,
        num(14, 16)?,
        num(17, 19)?,
    );
    let mut i = 19;
    let mut frac = 0.0;
    if b.get(i) == Some(&b'.') {
        let start = i;
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac = format!("0{}", &s[start..i]).parse::<f64>().ok()?;
    }
    let off = match b.get(i)? {
        b'Z' | b'z' => 0,
        c @ (b'+' | b'-') => {
            let oh = num(i + 1, i + 3)?;
            let om = num(i + 4, i + 6)?;
            let v = oh * 3600 + om * 60;
            if *c == b'+' { v } else { -v }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    Some((days * 86400 + h * 3600 + mi * 60 + se - off) as f64 + frac)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// First number (integer or decimal) at the start of `s`, ignoring leading spaces.
fn lead_num(s: &str) -> Option<f64> {
    let s = s.trim_start();
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    s[..end].trim_end_matches('.').parse().ok()
}

/// Number that follows `key` in `s`.
fn after(s: &str, key: &str) -> Option<f64> {
    lead_num(&s[s.find(key)? + key.len()..])
}

/// Parses the message part of a log line (timestamp already removed).
pub fn parse_message(msg: &str, t: f64) -> Option<Event> {
    if let Some(rest) = msg.strip_prefix("serve_api: ") {
        if let Some(v) = rest.strip_prefix("version ") {
            return Some(Event::Version(v.split(',').next().unwrap_or(v).trim().to_string()));
        }
        if rest.starts_with("client disconnected mid-stream") {
            return Some(Event::Disconnect);
        }
        return parse_summary(rest, t).map(Event::Done);
    }
    if let Some(rest) = msg.strip_prefix("flash_serve: req ") {
        let req = lead_num(rest)? as u64;
        let rest = rest.split_once(' ')?.1;
        if let Some(p) = rest.strip_prefix("prefill ") {
            let (done, more) = p.split_once('/')?;
            let total = lead_num(more)? as u64;
            let secs = after(more, ", ").unwrap_or(0.0);
            return Some(Event::Progress(Progress::Prefill {
                req,
                done: lead_num(done)? as u64,
                total,
                secs,
            }));
        }
        if let Some(g) = rest.strip_prefix("generated ") {
            let tokens = lead_num(g)? as u64;
            let secs = after(g, ", ").unwrap_or(0.0);
            return Some(Event::Progress(Progress::Generated { req, tokens, secs }));
        }
    }
    None
}

/// `<mode> N tok in Xs = Y t/s | … | prompt P (C cached, x%), prefill Ss (N new) | … | pool U/T x% | think on`
fn parse_summary(rest: &str, t: f64) -> Option<Request> {
    let (mode, after_mode) = rest.split_once(' ')?;
    if !mode.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    let output = lead_num(after_mode)? as u64;
    let after_count = after_mode.trim_start().split_once(' ')?.1;
    if !after_count.starts_with("tok in ") {
        return None;
    }
    let decode_s = after(after_count, "tok in ")?;
    let decode_tps = after(after_count, "= ")?;
    let mut r = Request {
        t,
        mode: mode.to_string(),
        output,
        decode_s,
        decode_tps,
        ..Default::default()
    };
    for part in rest.split(" | ") {
        let part = part.trim();
        if let Some(p) = part.strip_prefix("prompt ") {
            r.prompt = lead_num(p).unwrap_or(0.0) as u64;
            if let Some(i) = p.find('(')
                && p[i..].contains(" cached")
            {
                r.cached = lead_num(&p[i + 1..]).unwrap_or(0.0) as u64;
            }
            if let Some(s) = after(p, "prefill ") {
                r.prefill_s = s;
            }
            if let Some(i) = p.rfind('(')
                && p[i..].contains(" new)")
            {
                r.new_tokens = lead_num(&p[i + 1..]).unwrap_or(0.0) as u64;
            } else if let Some(i) = p.find("prefill ")
                && p[i..].contains("t/s")
            {
                // long prefill: `prefill 5.98s = 1862 t/s`, no new-token count
                r.prefill_rate = after(&p[i..], "s = ");
                r.new_tokens = r.prompt.saturating_sub(r.cached);
            }
        } else if let Some(p) = part.strip_prefix("pool ") {
            if let Some((u, tot)) = p.split_once('/') {
                r.pool_used = lead_num(u).map(|x| x as u64);
                r.pool_total = lead_num(tot).map(|x| x as u64);
            }
        } else if let Some(p) = part.strip_prefix("think ") {
            r.think = Some(p.starts_with("on"));
        } else if part.contains("/round") {
            r.commit_per_round = after(part, "commit ");
        }
    }
    Some(r)
}

/// Prometheus text format: `name{labels} value` -> map of `name` (labels dropped) to value, plus the labels of
/// `halogen:build_info`.
pub fn parse_metrics(text: &str) -> (HashMap<String, f64>, HashMap<String, String>) {
    let mut values = HashMap::new();
    let mut build = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name_labels, value) = match line.rsplit_once(' ') {
            Some(x) => x,
            None => continue,
        };
        let (name, labels) = match name_labels.split_once('{') {
            Some((n, l)) => (n, Some(l.trim_end_matches('}'))),
            None => (name_labels, None),
        };
        if name.ends_with("build_info")
            && let Some(l) = labels
        {
            for kv in l.split(',') {
                if let Some((k, v)) = kv.split_once('=') {
                    build.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
                }
            }
        }
        if let Ok(v) = value.parse::<f64>() {
            values.insert(name.to_string(), v);
        }
    }
    (values, build)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTP: &str = "serve_api: mtp 180 tok in 3.12s = 57.61 t/s | 75 rounds, commit 2.41/round | prompt 108508 (108096 cached, 99.6%), prefill 0.58s (412 new) | detok 14us/tok | pool 454144/655360 69% | think on";

    #[test]
    fn summary_line() {
        let Some(Event::Done(r)) = parse_message(MTP, 5.0) else {
            panic!("not parsed")
        };
        assert_eq!(r.mode, "mtp");
        assert_eq!(r.output, 180);
        assert_eq!(r.decode_s, 3.12);
        assert_eq!(r.decode_tps, 57.61);
        assert_eq!(r.prompt, 108508);
        assert_eq!(r.cached, 108096);
        assert_eq!(r.new_tokens, 412);
        assert_eq!(r.prefill_s, 0.58);
        assert_eq!(r.commit_per_round, Some(2.41));
        assert_eq!(r.pool_used, Some(454144));
        assert_eq!(r.pool_total, Some(655360));
        assert_eq!(r.think, Some(true));
        assert!((r.prefill_tps().unwrap() - 710.34).abs() < 0.1);
    }

    #[test]
    fn summary_with_prefill_rate() {
        let l = "serve_api: mtp 310 tok in 4.50s = 68.91 t/s | 102 rounds, commit 3.05/round | prompt 29829 (18688 cached, 62.7%), prefill 5.98s = 1862 t/s | detok 17us/tok | pool 596992/655360 91% | think on | closest earlier: 18736 tokens shared with a request 814s ago, then its prompt differs";
        let Some(Event::Done(r)) = parse_message(l, 0.0) else {
            panic!()
        };
        assert_eq!(
            (r.prompt, r.cached, r.new_tokens, r.prefill_s),
            (29829, 18688, 11141, 5.98)
        );
        assert_eq!(r.prefill_tps(), Some(1862.0));
        assert_eq!(r.think, Some(true));
    }

    #[test]
    fn summary_without_cache_and_pld() {
        let l = "serve_api: mtp 400 tok in 5.16s = 77.51 t/s | 119 rounds, commit 3.35/round | prompt 31, prefill 0.33s (31 new) | detok 15us/tok | pld 35 rounds, 2.09 acc/round | pool 512/655360 0% | think off";
        let Some(Event::Done(r)) = parse_message(l, 0.0) else {
            panic!()
        };
        assert_eq!((r.prompt, r.cached, r.new_tokens, r.think), (31, 0, 31, Some(false)));
    }

    #[test]
    fn progress_lines() {
        assert_eq!(
            parse_message("flash_serve: req 532 prefill 84480/108114 tokens, 32.4 s", 0.0),
            Some(Event::Progress(Progress::Prefill {
                req: 532,
                done: 84480,
                total: 108114,
                secs: 32.4
            }))
        );
        assert_eq!(
            parse_message("flash_serve: req 533 generated 2 tokens, 0.0 s", 0.0),
            Some(Event::Progress(Progress::Generated {
                req: 533,
                tokens: 2,
                secs: 0.0
            }))
        );
    }

    #[test]
    fn other_lines_ignored() {
        for l in [
            "serve_api: request defaults HALOGEN_MAX_TOKENS_DEFAULT=16384, HALOGEN_TEMPERATURE=0.8",
            "serve_api: a request whose client had left was stopped (no retry came within 20s), 158 items were made",
            "serve_api: chat template /models/tokenizer/chat_template.jinja sha256 c3cf9e34abf4f9e3",
            "startup [   3.1 s] opening the socket",
        ] {
            assert_eq!(parse_message(l, 0.0), None, "{l}");
        }
        assert_eq!(
            parse_message("serve_api: version 0.18.2, engine version 0.18.2", 0.0),
            Some(Event::Version("0.18.2".into()))
        );
    }

    #[test]
    fn timestamps() {
        let (t, m) =
            split_timestamp("1791936422.242338 gpu-box halogen[1668305]: flash_serve: req 1 generated 2 tokens, 0.1 s");
        assert_eq!(t, Some(1791936422.242338));
        assert!(m.starts_with("flash_serve: req 1"));
        let (t, m) = split_timestamp("2026-10-14T02:07:56.5Z serve_api: version 0.18.2");
        assert_eq!(t, Some(1791943676.5));
        assert_eq!(m, "serve_api: version 0.18.2");
        let (t, _) = split_timestamp("2026-10-14T04:07:56+02:00 x");
        assert_eq!(t, Some(1791943676.0));
        assert_eq!(split_timestamp("serve_api: version 1").0, None);
    }

    #[test]
    fn metrics() {
        let (v, b) = parse_metrics(
            "# HELP x\nllamacpp:kv_cache_usage_ratio 0.69375\nhalogen:engine_up 1\nhalogen:build_info{version=\"0.18.2\",model=\"m\"} 1\n",
        );
        assert_eq!(v["llamacpp:kv_cache_usage_ratio"], 0.69375);
        assert_eq!(v["halogen:engine_up"], 1.0);
        assert_eq!(b["version"], "0.18.2");
        assert_eq!(b["model"], "m");
    }
}
