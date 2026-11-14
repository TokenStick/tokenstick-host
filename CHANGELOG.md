# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org).

## [Unreleased]

## [0.1.0] - 2026-11-14

Phase 0a: everything that can be built before RK1828 hardware arrives. Only the `mock` backend is functional.

### Added
- `libtokenstick`: `Backend` trait with device discovery, telemetry, model lifecycle (`loaded_model`, `load_model`, `unload_model`), power modes (`power_mode`, `set_power_mode`) and streaming `generate`; `resolve_device` and `ensure_loaded` helpers.
- Mock backend: simulated RK1828 with token and load delays, power modes, telemetry, and `Busy` errors when jobs overlap.
- Local model store (`model_store`): per-model directories with `manifest.json` for the RKNN3 file set (`.rknn`, `.weight`, `.tokenizer.gguf`, `.embed.bin`), host RAM estimate per model.
- `tokenstick-server`: OpenAI-compatible `/v1/models` and `/v1/chat/completions` (SSE streaming, text content parts, `max_completion_tokens`), per-device FIFO queue with `503` back-pressure, client-disconnect cancellation, graceful shutdown on Ctrl-C and SIGTERM.
- Management API: `/api/v1/info`, `/api/v1/devices`, `/api/v1/stats`, `/api/v1/events` (SSE), plus `/v1/tokenstick/status`.
- Embedded web UI at `/`: Dashboard, Playground (streaming chat with stop), Server page with client snippets.
- `tokenstick` CLI: `status`, `models`, `run`, `bench` (load time, TTFT, tok/s, power mode, JSON output), `power`, `store path|list`, `serve`.
- Packaging: systemd unit, `/etc/default/tokenstick`, `scripts/install.sh` (local or `--version` download with checksum).
- CI (fmt, clippy, tests, aarch64 cross-build) and tag-driven release workflow for x86_64 and aarch64 tarballs.
- Docs: architecture, RKNN3 runtime mapping for the upcoming `pcie-vendor` backend, contributing guide with branch model and versioning.

[Unreleased]: https://github.com/TokenStick/tokenstick-host/compare/v0.1.0...development
[0.1.0]: https://github.com/TokenStick/tokenstick-host/releases/tag/v0.1.0
