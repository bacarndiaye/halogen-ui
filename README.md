# halogen-ui

A live web monitor for a [Halogen](https://github.com/peonist-ai/halogen-flash-server) inference server: speed per
request, prefill progress, context and KV pool, prompt cache, GPU load / memory / temperature / power, and the list of
recent requests.

One small static binary written in Rust, **no dependencies**. The monitor is read-only; an optional [chat page](#chat)
lets you talk to the model from the browser. Unofficial: not affiliated with the Halogen authors.

| Monitor | Chat (`--chat`) |
| --- | --- |
| ![halogen-ui monitor](docs/screenshot.png) | ![halogen-ui chat](docs/screenshot-chat.jpg) |

## What it shows

| Panel | Source |
| --- | --- |
| Model state: idle, reading the prompt (with progress), generating, queued, down | `/metrics` + log progress lines |
| Speed of the last request: decode and prefill tokens/s, with history | log, one summary line per request |
| GPU load, memory (VRAM + GTT on unified-memory chips), temperature, power, clock | amdgpu sysfs, or `nvidia-smi` |
| KV pool fill, CPU load, system RAM | `/metrics`, `/proc` |
| Context of the last request against the model window | log + `/health` |
| Recent requests: prompt, reused, output, prefill t/s, decode t/s, thinking, pool, duration | log |
| Prompt cache: cached conversations, disk cache, reuse, hits / misses, evictions, restore time | `/cache` |
| Totals since Halogen started: requests, tokens read and reused, tokens written, average speeds | `/metrics` |

The page refreshes twice a second and values glide to their new reading: host sensors are read every 500 ms,
Halogen's `/metrics` every second (the graphs get a point per second, 10 minutes kept in the browser). Halogen only
reports a request's token count when the request ends, so the **live token counter** ("Generating · 412 tokens · 52
tok/s") covers the requests that go through the [chat](#chat), and through the [proxy](#live-tokens-for-every-client) if you
set it up; without it, other clients show the busy time until their request ends.

No prompt or answer text is ever read: Halogen's log only carries counts. The page can be shared publicly.

## Install

Build a static binary (Rust 1.85+, edition 2024):

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
sudo install -m 755 target/x86_64-unknown-linux-musl/release/halogen-ui /usr/local/bin/halogen-ui
```

Run it on the machine that runs Halogen, so it can read the GPU sensors:

```bash
# Halogen as a systemd service (for example a Podman Quadlet):
# (reading from Halogen's start gives the whole history and the "since" time)
halogen-ui --listen 0.0.0.0:8790 \
  --log-cmd 'journalctl -u halogen.service -f -o short-unix --since "$(systemctl show -p ActiveEnterTimestamp --value halogen.service)"'

# Halogen in Docker or Podman (container named "halogen"):
halogen-ui --listen 0.0.0.0:8790 --log-cmd "docker logs -f -t --tail 3000 halogen"
```

Then open `http://<host>:8790`. A hardened systemd unit is in [`deploy/halogen-ui.service`](deploy/halogen-ui.service)
(dynamic user, member of `systemd-journal` only).

Behind a reverse proxy, any path prefix works (the page only uses relative URLs), for example with nginx:

```nginx
location = /halogen { return 301 /halogen/; }
location /halogen/ { proxy_pass http://127.0.0.1:8790/; limit_except GET { deny all; } }
```

## Options

| Flag | Variable | Default |
| --- | --- | --- |
| `--listen ADDR` | `HUI_LISTEN` | `127.0.0.1:8790` |
| `--halogen URL` | `HUI_HALOGEN` | `http://127.0.0.1:8731` |
| `--log-cmd CMD` | `HUI_LOG_CMD` | `docker logs -f -t --tail 3000 halogen` |
| `--title TEXT` | `HUI_TITLE` | `Halogen` |
| `--keep N` | `HUI_KEEP` | `500` finished requests kept in memory |
| `--chat` | `HUI_CHAT=1` | off: also serve the chat page, see [Chat](#chat) |
| | `HUI_HALOGEN_KEY` | API key, if Halogen requires one (never on the command line) |
| | `HUI_CHAT_TOKEN` | password of the chat (never on the command line), see [Chat](#chat) |
| `--proxy-listen ADDR` | `HUI_PROXY_LISTEN` | off: transparent proxy to Halogen, see [below](#live-tokens-for-every-client) |

The log command should print timestamps (`-o short-unix` for journalctl, `-t` for docker / podman); without them, the
backlog is stamped with the time it was read.

## Chat

Halogen has no interface of its own. With `--chat` (or `HUI_CHAT=1`), halogen-ui also serves a chat page at `/chat`
(a "Chat" button appears in the monitor header):

- streamed answers with Markdown (tables, code blocks with a copy button, lists), the model's thinking in a collapsible
  block, and a live tokens / tokens-per-second counter while the answer is written (Halogen sends one token per event);
- attach images (sent to Halogen's vision input, downscaled to 1920 px) and text files; paste or drop them too;
- stop, regenerate, copy, new chat, download the conversation as Markdown;
- settings: system prompt, thinking level (off / low / medium / high / max), temperature, max answer tokens.

The conversation lives in the browser (`localStorage`); the server stores and logs nothing. The page talks to
`POST /api/chat`, which relays the request to Halogen's `/v1/chat/completions` and streams the answer back, adding the
API key (`HUI_HALOGEN_KEY`) on the way. Closing the tab or pressing Stop hangs up on Halogen, which then stops generating.

**The chat is off by default** because it changes what the page is: the monitor is read-only and can be shared, while
anyone who can open `/chat` can use your model. Turn it on for a trusted network, or set a password:
`HUI_CHAT_TOKEN=…` makes `/api/chat` refuse everyone until the page has been unlocked once with it (session cookie,
kept in memory, one try per second); the monitor itself stays open. Scripts can send `Authorization: Bearer <password>`.
Over plain HTTP the password travels in clear: use TLS in front of it outside a trusted LAN.
The nginx example above keeps working as a read-only monitor because of `limit_except GET`; to allow the chat, let `POST`
through for `/api/chat` only, with buffering off so the answer streams:

```nginx
location = /halogen/api/chat { proxy_pass http://127.0.0.1:8790/api/chat; proxy_buffering off; proxy_read_timeout 3700s; }
```

## Live tokens for every client

Halogen reports a request's tokens only when it ends. To see them climb live for all your clients (not only the chat),
let halogen-ui sit in the path: `--proxy-listen 127.0.0.1:18732` opens a transparent TCP proxy to Halogen. Bytes go
through unchanged in both directions, so authentication, TLS and the API stay exactly where they are; the answers are
only watched for server-sent events (chat completions, completions, Anthropic messages and OpenAI responses) to count the
tokens as they go by. Nothing is read from the prompts or stored. It only listens on a loopback address.

Point your reverse proxy at it, **with Halogen as a fallback** so that stopping halogen-ui never cuts your API:

```nginx
upstream halogen_live {
    server 127.0.0.1:18732;
    server 127.0.0.1:18731 backup;   # Halogen itself
}
# ... proxy_pass http://halogen_live;
```

A request that does not stream (no `stream: true`) has no live count: it shows up when it ends, as before.

## API

- `GET /api/state`: everything the page shows, as JSON (`?all=1` for every kept request).
- `GET /healthz`: `ok`.
- With `--chat`: `GET /chat` (the page) and `POST /api/chat` (an OpenAI chat-completions request, relayed to Halogen).

## Security

- Read-only: only `GET` and `HEAD` are answered. The one exception is `POST /api/chat`, and only with `--chat`: it
  refuses cross-site requests and anything that is not JSON, limits itself to 8 streams at once and 48 MB per request,
  and can only reach Halogen's `/v1/chat/completions`.
- Strict Content-Security-Policy (no inline script, no third-party resource), `X-Frame-Options: DENY`,
  `Referrer-Policy: no-referrer`.
- The Halogen API key is read from the environment and never sent to the browser.
- Put it behind a reverse proxy with TLS before exposing it to the Internet.

## Development

```bash
cargo test
cargo clippy --all-targets
```

## License

MIT
