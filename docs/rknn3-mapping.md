# RKNN3 runtime → `Backend` trait mapping

Plan for the `pcie-vendor` backend, written before the kit arrives. Source: Forlinx [RK1820/RK1828 AI Accelerator Development Guide](https://docs.forlinx.net/ai-accelerator/rk1820_rk1828/RK1820_RK1828_AI_Accelerator_Development_Guide.html) (RKNN3 SDK V1.0.0). The guide lists API names only, no signatures; everything below marked **(verify)** must be checked against `rknn3_api.h` on the kit.

## Vendor stack on the host

| Piece | What it is | Notes |
| --- | --- | --- |
| `pcie-rkep.ko` | Kernel driver for the PCIe endpoint | Loaded with `insmod`; `dmesg \| grep pcie-rkep` shows firmware load |
| `rknn3_rk182x_m2_installer_arm64.tgz` | Installs runtime + service | **arm64 only** in the guide: x86 support is still an open question |
| `rknn3.service` | Systemd service | Guide suggests `ExecStartPre=/bin/sleep 3` if `rknn-smi` fails at boot |
| `rknn3_transfer_proxy` | Host ↔ device transport over **PCIe or USB** | The USB path matters for TokenStick (see below) |
| `librknn3_api.so` + `rknn3_api.h` | C runtime we bind to | FFI target |
| `rknn-smi` | `info`, `set -t work_mode` (0 low power, 1 balanced, 2 performance) | Source for device list and telemetry until the API equivalent is known |
| `rkllm3-server` | Vendor OpenAI-compatible server | Useful as a reference to benchmark our server against |

## Model files

A converted LLM is not one file. The runtime needs:

| File | Runs on | Purpose |
| --- | --- | --- |
| `<name>.rknn` | Device (NPU) | Compiled graph |
| `<name>.weight` | Device | Compiled weights, required with `.rknn` |
| `<name>.tokenizer.gguf` | Host | Tokenizer (the C++ demo uses llama.cpp's GGUF tokenizer) |
| `<name>.embed.bin` | **Host** | FP16 embedding table, kept on the host because device DRAM is limited |

Conversion is done with `rknn3-toolkit` (x86_64 Ubuntu 20.04/22.04, Python 3.10), `target_platform='RK1828'`, `w4a16` for LLMs. RKNN3 models are not compatible with RKNN-Toolkit2.

**Consequence for host RAM:** the embedding table lives on the host. For Qwen2.5-7B that is about 152k vocab × 3584 dims × 2 bytes ≈ 1.1 GB of host memory per loaded model. A 4 GB Pi 5 is tight; 8 GB is the practical minimum host.

### Proposed model directory layout

```
~/.local/share/tokenstick/models/<model-id>/
  manifest.json            # id, family, params_b, quantization, context_len, chat_template, files
  <name>.rknn
  <name>.weight
  <name>.tokenizer.gguf
  <name>.embed.bin
```

`Backend::models()` for `pcie-vendor` = scan this directory and read each `manifest.json`. tokenstick-models produces exactly this layout.

## Call mapping

| `Backend` method | RKNN3 calls | Notes |
| --- | --- | --- |
| `devices()` | `rknn3_query` **(verify)**, fallback: parse `rknn-smi info` | Device id = PCI bus id (`0000:01:00.0`) |
| `telemetry(device)` | `rknn-smi info` fields **(verify which exist)** | Temperature and power are optional in `Telemetry` for this reason |
| `load_model(device, model)` | `rknn3_init` → `rknn3_load_model_from_path` (`.rknn` + `.weight`) → `rknn3_model_init` → `rknn3_session_init` → `rknn3_session_set_chat_template` → `rknn3_session_set_callback` | Also load `.embed.bin` and tokenizer into host memory. Measure time: this is `load_ms` |
| `loaded_model(device)` | Our own state | Runtime handle kept per device |
| `unload_model(device)` | `rknn3_session_destroy` → `rknn3_destroy` (+ `rknn3_destroy_mem`) | Free host embed table too |
| `generate(device, req, sink)` | `rknn3_session_clear_kvcache` → `rknn3_session_run` | Blocking run fits our blocking trait |
| `sink` returns `Break` | `rknn3_session_stop` | Cancellation path for client disconnect |

### Callbacks

The session uses five callbacks; the backend implements them in Rust behind `extern "C"` trampolines:

| Callback | Required | Our implementation |
| --- | --- | --- |
| `result_callback` | Yes | Decode token → text, push into `TokenSink`; count tokens; stamp first-token time for TTFT |
| `embed_callback` | Yes, runs on host | Look up token ids in the host `.embed.bin` table |
| `tokenizer_callback` | Optional | GGUF tokenizer (bind llama.cpp's tokenizer or reimplement in Rust; decide on kit) |
| `output_callback` | Optional | Not needed for Phase 0 |
| `sampling_callback` | Optional | Leave to runtime; later maps `temperature` / `top_p` if the runtime does not |

`result_userdata` carries a pointer to the per-call Rust state (sink, counters, cancel flag). The trampoline must catch panics (`std::panic::catch_unwind`) so nothing unwinds across the FFI boundary.

### Conversation state

Phase 0: clear the KV cache on every request and send the full message list (simple, stateless, matches the OpenAI API). Later: keep the KV cache when a request extends the previous conversation on the same device (prefix match), which cuts TTFT for multi-turn chat.

## Trait changes this implies

Not urgent, noted so they land before 0d:

1. **Power mode:** `rknn-smi set -t work_mode` 0/1/2. Add `set_power_mode(device, mode)` and expose it in CLI/server; benchmarks must record the mode (vendor numbers are in performance mode).
2. **Core mask:** `core_mask` selects NPU cores (e.g. `0xff`). Keep internal for now, default to all cores.
3. **Host memory in `ModelInfo`:** add an estimated host RAM figure (embed table) so the CLI can warn on small hosts.

## USB transport

The guide states the RK182X co-processor connects to the host over **PCIe or USB**, and `rknn3_transfer_proxy` handles both. If the vendor runtime already speaks USB, the TokenStick `usb` backend may reuse the vendor stack instead of a protocol we design ourselves. Questions for Rockchip and the kit:

- Which USB mode and speed (USB 3.0 device mode?), and which VID/PID enumerates?
- Is the USB path in `rknn3_transfer_proxy` available in the public SDK, and on which host architectures?
- Is firmware loaded by the host over USB the same way as over PCIe?

## Reference numbers (vendor, PCIe, performance mode, 128 in / 128 out)

Our benchmark harness should reproduce these before we trust our own stack:

| Model | Chip | TTFT | Decode |
| --- | --- | --- | --- |
| Qwen2.5-3B | RK182X | 83.44 ms | 102.01 tok/s |
| Qwen2.5-7B | RK1828 | 158.06 ms | 70.26 tok/s |
| Qwen3-8B | RK1828 | 177.87 ms | 61.11 tok/s |

## Open questions to answer on the kit

1. Exact signatures of `rknn3_*` functions and the `rknn3_llm_param` / `RKLLMCallback` structs.
2. Can one process drive several RK182X devices (multi-card)? The guide covers a single card.
3. Which telemetry the API or `rknn-smi` exposes (temperature, power, utilisation).
4. Does `rknn3_session_run` block until done, and is `rknn3_session_stop` safe from inside `result_callback`?
5. Is the runtime thread-safe across devices, or must all calls go through one thread?
6. Does the runtime/driver build for x86_64 (installer is arm64 in the guide)?
