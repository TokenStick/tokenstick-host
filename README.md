# tokenstick-host

Host software for [TokenStick](https://github.com/TokenStick), the open-source USB AI accelerator for local LLM inference on Rockchip RK1828.

> **Status: Phase 0.** Only the `mock` backend works today. It streams fake tokens so the CLI and API can be built before hardware is available. The `pcie-vendor` backend (RK1828 over PCIe via Rockchip RKNN3) comes next; the `usb` backend follows the TokenStick board.

## What is in here

| Crate | Purpose |
| --- | --- |
| `libtokenstick` | Core library. Every caller reaches hardware only through the `Backend` trait. |
| `tokenstick-server` | OpenAI-compatible HTTP API (`/v1/models`, `/v1/chat/completions` with streaming) plus `/v1/tokenstick/status`. Queues requests per device. |
| `tokenstick-cli` | The `tokenstick` binary: `status`, `models`, `run`, `bench`, `serve`. |

Models are loaded on demand: the first request for a model loads it onto a device (seconds on real hardware), later requests reuse it. Each device runs one job at a time; the server queues requests in FIFO order and answers `503` when more than 8 jobs are pending.

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

Then point any OpenAI client at `http://127.0.0.1:8080/v1`:

```sh
curl -N http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"qwen2.5-1.5b-instruct-w4a16","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

Environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `TOKENSTICK_BACKEND` | `mock` | Backend to open |
| `TOKENSTICK_DEVICE` | auto | Device id for `run` / `bench` |
| `TOKENSTICK_ADDR` | `127.0.0.1:8080` | Server listen address |
| `TOKENSTICK_MOCK_DELAY_MS` | `25` | Per-token delay of the mock backend |
| `TOKENSTICK_MOCK_LOAD_MS` | `300` | Model load time of the mock backend |
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
- [RKNN3 runtime mapping](docs/rknn3-mapping.md): plan for the `pcie-vendor` backend

## License

Apache-2.0. See [LICENSE](LICENSE).
