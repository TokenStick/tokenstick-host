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
| `telemetry(id)` | Temperature, power, NPU load, USB PD contract (all optional) |
| `models()` | Models the backend can run (`ModelInfo`) |
| `generate(req, sink)` | Streams text pieces into `sink`, returns `GenerationStats` (TTFT, tok/s, finish reason) |

Methods are blocking on purpose: the vendor runtime is a C library and USB transfers are synchronous. The server runs them on Tokio's blocking pool and bridges tokens to SSE through a channel. A closed channel makes the sink return `Break`, which cancels generation when a client disconnects.

## Why PCIe first

Every RK182X development platform available today connects RK1828 to its host over PCIe. Phase 0 therefore validates the runtime, models and API on PCIe (`pcie-vendor`). The USB transport is validated on the TokenStick board. Transport research notes from the kit go in this folder.

## Phase 0 plan

See the TokenStick Software Plan doc. In short: mock backend and skeletons (0a), kit bring-up (0b), benchmarks and transport research (0c), `pcie-vendor` backend and real-hardware CLI/server (0d).
