# Architecture

```
 OpenAI clients / Open WebUI / curl          tokenstick CLI
                 |                                  |
         tokenstick-server  <-----------------------+  (serve)
                 |
           libtokenstick ── Backend trait
                 |
     +-----------+-------------+-------------------+
     |                         |                   |
   mock                  pcie-vendor              usb
 (no hardware)      (RKNN3 runtime, PCIe)   (TokenStick, USB 3.0)
```

## Backend trait

`libtokenstick::Backend` is the single seam between software and hardware:

| Method | Returns |
| --- | --- |
| `devices()` | Visible accelerators (`DeviceInfo`) |
| `telemetry(device)` | Temperature, power, NPU load, USB PD contract (all optional) |
| `models()` | Models the backend can run (`ModelInfo`) |
| `loaded_model(device)` | Model resident on the device, if any |
| `load_model(device, model)` / `unload_model(device)` | Move a model onto / off the device (seconds for 7B) |
| `generate(device, req, sink)` | Streams text pieces into `sink`, returns `GenerationStats` (TTFT, tok/s, finish reason) |

Contract:

- Methods are blocking on purpose: the vendor runtime is a C library and USB transfers are synchronous. The server runs them on Tokio's blocking pool and bridges tokens to SSE through a channel. A closed channel makes the sink return `Break`, which cancels generation when a client disconnects.
- One job per device. Callers serialize `load_model`, `unload_model` and `generate` per device; a backend may return `Error::Busy` otherwise. The mock enforces this so tests catch mistakes.
- `generate` needs the model loaded first. Helpers: `resolve_device` (explicit id, else the device that already holds the model, else the first) and `ensure_loaded`.

## Server scheduling

Each device has a single-permit semaphore, which gives a FIFO queue. A request is admitted before any response is sent: the model is checked (404), a device is picked, and a pending slot is reserved (503 when `max_pending`, default 8, is reached). The job then waits for its device, loads the model if needed and generates. Streaming and non-streaming requests share this path, so errors and back-pressure behave the same.

## Why PCIe first

Every RK182X development platform available today connects RK1828 to its host over PCIe. Phase 0 therefore validates the runtime, models and API on PCIe (`pcie-vendor`). The USB transport is validated on the TokenStick board. Transport research notes from the kit go in this folder.

## Phase 0 plan

See the TokenStick Software Plan doc. In short: mock backend and skeletons (0a), kit bring-up (0b), benchmarks and transport research (0c), `pcie-vendor` backend and real-hardware CLI/server (0d).

## Management API and web UI

| Endpoint | Returns |
| --- | --- |
| `GET /api/v1/info` | Version, backend, uptime, queue limit |
| `GET /api/v1/devices` | Devices with telemetry, loaded model, busy flag |
| `GET /api/v1/stats` | Last generation (model, TTFT, tok/s), totals, pending jobs |
| `GET /api/v1/events` | SSE `tick` event once per second with `{devices, stats}` |

The web UI is one HTML file (`crates/tokenstick-server/src/ui/index.html`) compiled into the binary with `include_str!` and served at `/`. Plain HTML/JS/CSS, no Node build step, so CI and cross-compiling stay unchanged. The same page is what a Tauri tray app will wrap later. Screens today: Dashboard, Playground, Server.
