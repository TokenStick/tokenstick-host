# tokenstick-host

Host software for [TokenStick](https://github.com/TokenStick), the open-source USB AI accelerator for local LLM inference on Rockchip RK1828.

> **Status: Phase 0.** Only the `mock` backend works today. It streams fake tokens so the CLI and API can be built before hardware is available. The `pcie-vendor` backend (RK1828 over PCIe via Rockchip RKNN3) comes next; the `usb` backend follows the TokenStick board.

## What is in here

| Crate | Purpose |
| --- | --- |
| `libtokenstick` | Core library. Every caller reaches hardware only through the `Backend` trait. |
| `tokenstick-server` | OpenAI-compatible HTTP API (`/v1/models`, `/v1/chat/completions` with streaming), management API (`/api/v1/*`) and the embedded web UI. |
| `tokenstick-cli` | The `tokenstick` binary: `status`, `models`, `run`, `bench`, `serve`. |

Backends:

| Backend | Talks to | State |
| --- | --- | --- |
| `mock` | Nothing, fake tokens and telemetry | Working |
| `pcie-vendor` | RK1828 on PCIe through the RKNN3 runtime | Planned (Phase 0d) |
| `usb` | TokenStick over USB 3.0 | Planned (own board) |

## Quick start

```sh
cargo build --release

./target/release/tokenstick status
./target/release/tokenstick models
./target/release/tokenstick run qwen2.5-1.5b-instruct-w4a16 -p "Hello"
./target/release/tokenstick bench qwen2.5-3b-instruct-w4a16 --runs 3
./target/release/tokenstick serve --addr 127.0.0.1:8080
```

Open `http://127.0.0.1:8080/` for the web UI (Dashboard, Playground, Server), or point any OpenAI client at `http://127.0.0.1:8080/v1`:

```sh
curl -N http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"qwen2.5-1.5b-instruct-w4a16","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

## Web UI

`tokenstick serve` also serves a web UI at `/`. It is plain HTML/CSS/JS compiled into the binary (`crates/tokenstick-server/ui/`), so headless hosts like the RK3576 kit or a Raspberry Pi serve it with no extra files and the build needs no Node toolchain. The same UI will be wrapped by Tauri for the desktop tray app.

- **Dashboard**: devices, live temperature / power / NPU load, last 5 min chart, throughput per request, recent requests
- **Playground**: streaming chat with model picker, system prompt, temperature, top-p, max tokens
- **Server**: endpoint URL and copyable curl / Python / JavaScript snippets

Management API used by the UI:

| Endpoint | Returns |
| --- | --- |
| `GET /api/v1/info` | Backend, version, uptime |
| `GET /api/v1/devices` | Devices with current telemetry |
| `GET /api/v1/stats` | Request counters and the last 50 requests |
| `GET /api/v1/events` | Server-sent events: a `snapshot` (devices + stats) every second |

Set the `x-tokenstick-client` header on `/v1/chat/completions` to label your requests on the dashboard.

Environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `TOKENSTICK_BACKEND` | `mock` | Backend to open |
| `TOKENSTICK_ADDR` | `127.0.0.1:8080` | Server listen address |
| `TOKENSTICK_MOCK_DELAY_MS` | `25` | Per-token delay of the mock backend |
| `RUST_LOG` | `info` | Log filter |

## Cross-compiling for the dev kit

The Phase 0 host is an RK3576 board (aarch64 Debian):

```sh
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --release --target aarch64-unknown-linux-gnu
```

## Docs

- [Architecture](docs/architecture.md)

## License

Apache-2.0. See [LICENSE](LICENSE).
