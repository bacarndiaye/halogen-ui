//! Tiny JSON writer (the crate has no dependencies).

use std::fmt::Write;

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn number(v: f64) -> String {
    if !v.is_finite() {
        "null".into()
    } else if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{:.4}", v)
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

pub fn array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

#[derive(Default)]
pub struct Obj {
    fields: Vec<String>,
}

impl Obj {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn raw(&mut self, k: &str, v: &str) -> &mut Self {
        self.fields.push(format!("{}:{}", escape(k), v));
        self
    }
    pub fn str(&mut self, k: &str, v: &str) -> &mut Self {
        self.raw(k, &escape(v))
    }
    pub fn opt_str(&mut self, k: &str, v: Option<String>) -> &mut Self {
        match v {
            Some(v) => self.str(k, &v),
            None => self.null(k),
        }
    }
    pub fn num(&mut self, k: &str, v: f64) -> &mut Self {
        self.raw(k, &number(v))
    }
    pub fn int(&mut self, k: &str, v: u64) -> &mut Self {
        self.raw(k, &v.to_string())
    }
    pub fn opt(&mut self, k: &str, v: Option<f64>) -> &mut Self {
        match v {
            Some(v) => self.num(k, v),
            None => self.null(k),
        }
    }
    pub fn bool(&mut self, k: &str, v: bool) -> &mut Self {
        self.raw(k, if v { "true" } else { "false" })
    }
    pub fn null(&mut self, k: &str) -> &mut Self {
        self.raw(k, "null")
    }
    pub fn end(&self) -> String {
        format!("{{{}}}", self.fields.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_valid_json() {
        let mut o = Obj::new();
        o.str("a", "x\"<y>\n")
            .num("b", 1.5)
            .num("c", 3.0)
            .opt("d", None)
            .bool("e", true)
            .num("f", f64::NAN);
        let want = "{\"a\":\"x\\\"\\u003cy>\\n\",\"b\":1.5,\"c\":3,\"d\":null,\"e\":true,\"f\":null}";
        assert_eq!(o.end(), want);
    }
}
