# Architecture

```
 OpenAI clients / Open WebUI / curl    web UI (browser, Tauri)    tokenstick CLI
                 |                       |  /api/v1/*, SSE             |
                 +-----------+-----------+                             |
                             |                                         |
                     tokenstick-server  <------------------------------+  (serve)
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
| `power_mode(device)` / `set_power_mode(device, mode)` | NPU power profile (RKNN3 `work_mode` 0/1/2); default impl reports `balanced` and refuses changes |
| `generate(device, req, sink)` | Streams text pieces into `sink`, returns `GenerationStats` (TTFT, tok/s, finish reason) |

Contract:

- Methods are blocking on purpose: the vendor runtime is a C library and USB transfers are synchronous. The server runs them on Tokio's blocking pool and bridges tokens to SSE through a channel. A closed channel makes the sink return `Break`, which cancels generation when a client disconnects.
- One job per device. Callers serialize `load_model`, `unload_model` and `generate` per device; a backend may return `Error::Busy` otherwise. The mock enforces this so tests catch mistakes.
- `generate` needs the model loaded first. Helpers: `resolve_device` (explicit id, else the device that already holds the model, else the first) and `ensure_loaded`.

## Server scheduling

Each device has a single-permit semaphore, which gives a FIFO queue. A request is admitted before any response is sent: the model is checked (404), a device is picked, and a pending slot is reserved (503 when `max_pending`, default 8, is reached). The job then waits for its device, loads the model if needed and generates. Streaming and non-streaming requests share this path, so errors and back-pressure behave the same.

## Web UI

One UI, two shells. `tokenstick-server` embeds a plain HTML/CSS/JS UI (`crates/tokenstick-server/ui/`) and serves it at `/`, next to the OpenAI API. A management API (`/api/v1/info`, `/devices`, `/stats`, `/events`) feeds it; `/events` is server-sent events with one snapshot per second, which is enough for telemetry and keeps the server simple. WebSocket comes only if the UI needs to push commands. The desktop tray app (Phase 3) wraps the same UI with Tauri.

The server keeps a small in-memory `Monitor` (active requests, totals, last 50 requests with their `GenerationStats`) so the dashboard can show throughput without polling the backend for history.

## Why PCIe first

Every RK182X development platform available today connects RK1828 to its host over PCIe. Phase 0 therefore validates the runtime, models and API on PCIe (`pcie-vendor`). The USB transport is validated on the TokenStick board. Transport research notes from the kit go in this folder.

## Phase 0 plan

See the TokenStick Software Plan doc. In short: mock backend and skeletons (0a), kit bring-up (0b), benchmarks and transport research (0c), `pcie-vendor` backend and real-hardware CLI/server (0d).

## Model store

`libtokenstick::model_store` reads converted models from `$TOKENSTICK_MODELS_DIR` (service default `/var/lib/tokenstick/models`, user default `~/.local/share/tokenstick/models`). One directory per model with a `manifest.json` listing the RKNN3 file set; `tokenstick store list` shows which models are complete. The `pcie-vendor` backend will build `models()` from this store.
