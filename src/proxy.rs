//! A transparent TCP proxy in front of Halogen, so the live token counter covers every client, not only the chat.
//!
//! Point the reverse proxy (or any client) at `--proxy-listen` instead of Halogen: bytes go through unchanged in both
//! directions, authentication and TLS stay where they are, and the answers are watched for server-sent events to count
//! the tokens as they stream by. It reads no prompt and keeps nothing.
//!
//! If halogen-ui stops, the clients lose this path: keep Halogen as a fallback in the reverse proxy (nginx `backup`).

use super::{Config, chat, connect};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

/// Accept connections on `addr` and relay each to Halogen.
pub fn serve(addr: &str, cfg: Arc<Config>) {
    let listener = TcpListener::bind(addr).unwrap_or_else(|e| {
        eprintln!("halogen-ui: cannot listen on {addr} for the proxy: {e}");
        std::process::exit(1)
    });
    // Halogen's own API may have no authentication (the reverse proxy in front adds it): never expose this door
    if !listener.local_addr().is_ok_and(|a| a.ip().is_loopback()) {
        eprintln!("halogen-ui: the proxy only listens on a loopback address (127.0.0.1, ::1), not on {addr}");
        std::process::exit(1)
    }
    for client in listener.incoming().flatten() {
        let cfg = cfg.clone();
        thread::spawn(move || relay(client, &cfg));
    }
}

fn relay(client: TcpStream, cfg: &Config) {
    let Ok((up, _, _)) = connect(cfg) else {
        // Halogen does not answer: closing at once lets the caller fall back or report it, as it would without us
        return;
    };
    client.set_nodelay(true).ok();
    up.set_nodelay(true).ok();
    let (Ok(client2), Ok(up2)) = (client.try_clone(), up.try_clone()) else {
        return;
    };
    // request direction: bytes as they come; the end of the caller's side ends the whole exchange, which is how
    // Halogen learns that the caller left and stops generating
    let request = thread::spawn(move || {
        let (mut from, mut to) = (client2, up2);
        let mut buf = [0u8; 16 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(k) => {
                    if to.write_all(&buf[..k]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to.shutdown(Shutdown::Both);
        let _ = from.shutdown(Shutdown::Both);
    });
    // answer direction: the same, counting tokens on the way
    let (mut from, mut to) = (up, client);
    let (live, mut counter) = (chat::Live::start(), chat::Counter::default());
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                live.add(counter.feed(&buf[..k]));
                if to.write_all(&buf[..k]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
    let _ = request.join();
}
