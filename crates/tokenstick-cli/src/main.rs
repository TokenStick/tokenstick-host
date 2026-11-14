use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use libtokenstick::{
    ensure_loaded, resolve_device, Backend, BackendKind, GenerationParams, GenerationRequest,
    GenerationStats, Message, Role,
};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "tokenstick",
    version,
    about = "TokenStick USB AI accelerator CLI"
)]
struct Cli {
    /// Backend to use: mock, pcie-vendor or usb.
    #[arg(
        long,
        global = true,
        env = "TOKENSTICK_BACKEND",
        default_value = "mock"
    )]
    backend: BackendKind,

    /// Device id to use (see `tokenstick status`). Defaults to the device that
    /// already has the model loaded, else the first device.
    #[arg(long, global = true, env = "TOKENSTICK_DEVICE")]
    device: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show devices, loaded models and telemetry.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List models the backend can run.
    Models {
        #[arg(long)]
        json: bool,
    },
    /// Chat with a model. Without --prompt, starts an interactive session.
    Run {
        model: String,
        #[arg(short, long)]
        prompt: Option<String>,
        #[arg(long, default_value_t = 256)]
        max_tokens: u32,
        #[arg(long)]
        system: Option<String>,
    },
    /// Measure model load time, TTFT and decode throughput.
    Bench {
        model: String,
        #[arg(long, default_value_t = 3)]
        runs: u32,
        #[arg(long, default_value_t = 128)]
        max_tokens: u32,
        #[arg(long, default_value = "Explain what an NPU is in three sentences.")]
        prompt: String,
        #[arg(long)]
        json: bool,
    },
    /// Start the OpenAI-compatible HTTP server.
    Serve {
        #[arg(long, env = "TOKENSTICK_ADDR", default_value = "127.0.0.1:8080")]
        addr: SocketAddr,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
    let backend: Arc<dyn Backend> = Arc::from(
        libtokenstick::open(cli.backend)
            .with_context(|| format!("opening backend '{}'", cli.backend))?,
    );
    let device = cli.device.as_deref();

    match cli.command {
        Command::Status { json } => status(backend.as_ref(), json),
        Command::Models { json } => models(backend.as_ref(), json),
        Command::Run {
            model,
            prompt,
            max_tokens,
            system,
        } => run(backend.as_ref(), device, &model, prompt, max_tokens, system),
        Command::Bench {
            model,
            runs,
            max_tokens,
            prompt,
            json,
        } => bench(
            backend.as_ref(),
            device,
            &model,
            runs,
            max_tokens,
            &prompt,
            json,
        ),
        Command::Serve { addr } => {
            tokio::runtime::Runtime::new()?.block_on(tokenstick_server::serve(backend, addr))
        }
    }
}

/// Pick the device and make sure the model is on it. Returns the device id and
/// the load time in milliseconds (0 when it was already loaded).
fn prepare(
    backend: &dyn Backend,
    device: Option<&str>,
    model: &str,
    quiet: bool,
) -> Result<(String, f64)> {
    if !backend.models()?.iter().any(|m| m.id == model) {
        anyhow::bail!("model not found: {model} (see `tokenstick models`)");
    }
    let device = resolve_device(backend, device, model)?;
    if !quiet && backend.loaded_model(&device)?.as_deref() != Some(model) {
        eprintln!("Loading {model} on {device}...");
    }
    let took = ensure_loaded(backend, &device, model)?;
    let load_ms = took.map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    if !quiet && took.is_some() {
        eprintln!("Loaded in {load_ms:.0} ms.");
    }
    Ok((device, load_ms))
}

fn status(backend: &dyn Backend, json: bool) -> Result<()> {
    let mut rows = Vec::new();
    for dev in backend.devices()? {
        let tel = backend.telemetry(&dev.id)?;
        let loaded = backend.loaded_model(&dev.id)?;
        rows.push((dev, tel, loaded));
    }
    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|(d, t, l)| serde_json::json!({ "device": d, "telemetry": t, "loaded_model": l }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No devices found (backend: {}).", backend.name());
    }
    for (dev, t, loaded) in rows {
        println!(
            "{}  {}  {} MiB  fw {}",
            dev.id,
            dev.chip,
            dev.memory_mib,
            dev.firmware.as_deref().unwrap_or("-")
        );
        println!("    model {}", loaded.as_deref().unwrap_or("(none loaded)"));
        println!(
            "    temp {}  power {}  npu {}  pd {}",
            fmt_opt(t.temperature_c, "°C"),
            fmt_opt(t.power_w, " W"),
            fmt_opt(t.npu_load_pct, "%"),
            t.pd_contract.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn fmt_opt(v: Option<f32>, unit: &str) -> String {
    v.map(|x| format!("{x:.1}{unit}"))
        .unwrap_or_else(|| "-".into())
}

fn models(backend: &dyn Backend, json: bool) -> Result<()> {
    let models = backend.models()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&models)?);
        return Ok(());
    }
    for m in models {
        println!(
            "{:<34} {:>5.1}B  {:<6} ctx {}",
            m.id, m.params_b, m.quantization, m.context_len
        );
    }
    Ok(())
}

fn generate_to_stdout(
    backend: &dyn Backend,
    device: &str,
    req: &GenerationRequest,
) -> Result<(String, GenerationStats)> {
    let mut out = io::stdout().lock();
    let mut text = String::new();
    let stats = backend.generate(device, req, &mut |t| {
        text.push_str(t);
        // Stop generating if stdout is closed (e.g. piped into `head`).
        match out.write_all(t.as_bytes()).and_then(|_| out.flush()) {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(()),
        }
    })?;
    writeln!(out)?;
    Ok((text, stats))
}

fn print_stats(s: &GenerationStats) {
    eprintln!(
        "[{} tokens, ttft {:.0} ms, {:.1} tok/s, {:?}]",
        s.completion_tokens, s.ttft_ms, s.tokens_per_s, s.finish_reason
    );
}

fn run(
    backend: &dyn Backend,
    device: Option<&str>,
    model: &str,
    prompt: Option<String>,
    max_tokens: u32,
    system: Option<String>,
) -> Result<()> {
    let (device, _) = prepare(backend, device, model, false)?;
    let mut messages = Vec::new();
    if let Some(s) = system {
        messages.push(Message::new(Role::System, s));
    }
    let params = GenerationParams {
        max_tokens,
        ..Default::default()
    };

    if let Some(p) = prompt {
        messages.push(Message::new(Role::User, p));
        let req = GenerationRequest {
            model: model.into(),
            messages,
            params,
        };
        let (_, stats) = generate_to_stdout(backend, &device, &req)?;
        print_stats(&stats);
        return Ok(());
    }

    eprintln!(
        "Chatting with {model} on {device} ({} backend). Ctrl-D to exit.",
        backend.name()
    );
    let stdin = io::stdin();
    loop {
        eprint!("> ");
        io::stderr().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            eprintln!();
            return Ok(());
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        messages.push(Message::new(Role::User, line));
        let req = GenerationRequest {
            model: model.into(),
            messages: messages.clone(),
            params: params.clone(),
        };
        let (reply, stats) = generate_to_stdout(backend, &device, &req)?;
        print_stats(&stats);
        messages.push(Message::new(Role::Assistant, reply));
    }
}

#[derive(Serialize)]
struct BenchReport {
    backend: String,
    device: String,
    model: String,
    load_ms: f64,
    runs: Vec<GenerationStats>,
    mean_ttft_ms: f64,
    mean_tokens_per_s: f64,
}

fn bench(
    backend: &dyn Backend,
    device: Option<&str>,
    model: &str,
    runs: u32,
    max_tokens: u32,
    prompt: &str,
    json: bool,
) -> Result<()> {
    anyhow::ensure!(runs > 0, "--runs must be at least 1");
    let (device, load_ms) = prepare(backend, device, model, json)?;
    let req = GenerationRequest {
        model: model.into(),
        messages: vec![Message::new(Role::User, prompt)],
        params: GenerationParams {
            max_tokens,
            ..Default::default()
        },
    };

    let mut results = Vec::new();
    for i in 1..=runs {
        let stats = backend.generate(&device, &req, &mut |_| ControlFlow::Continue(()))?;
        if !json {
            println!(
                "run {i}: {:>4} tokens  ttft {:>7.1} ms  {:>6.1} tok/s",
                stats.completion_tokens, stats.ttft_ms, stats.tokens_per_s
            );
        }
        results.push(stats);
    }

    let n = results.len() as f64;
    let report = BenchReport {
        backend: backend.name().into(),
        device,
        model: model.into(),
        load_ms,
        mean_ttft_ms: results.iter().map(|s| s.ttft_ms).sum::<f64>() / n,
        mean_tokens_per_s: results.iter().map(|s| s.tokens_per_s).sum::<f64>() / n,
        runs: results,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "mean:  ttft {:.1} ms  {:.1} tok/s  load {:.0} ms  ({} runs, {} on {})",
            report.mean_ttft_ms,
            report.mean_tokens_per_s,
            report.load_ms,
            runs,
            report.backend,
            report.device
        );
    }
    Ok(())
}
