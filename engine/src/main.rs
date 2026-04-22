//! flowlab-engine binary.
//!
//! Pulls events from a Source, runs them through the analytics pipeline,
//! and publishes telemetry frames over TCP for the Go API to consume.

use clap::Parser;
use flowlab_engine::backpressure;
use flowlab_engine::engine::{Engine, EngineConfig};
use flowlab_engine::ich::{Framing as IchFraming, IchSource, Pace as IchPace};
use flowlab_engine::server::TelemetryServer;
use flowlab_engine::source::Source;
use flowlab_engine::synthetic::SyntheticSource;
use flowlab_engine::wire::{Codec, TelemetryFrame};
use std::sync::Arc;
use tracing::info;

#[derive(Parser, Debug)]
#[command(version, about = "flowlab realtime engine", long_about = None)]
struct Cli {
    /// Source kind: synthetic | ich | binance
    #[arg(long, default_value = "synthetic")]
    source: String,

    /// Source argument: file path for ich, symbol for binance, ignored for synthetic.
    #[arg(long, default_value = "")]
    source_arg: String,

    /// Deterministic seed for synthetic source.
    #[arg(long, default_value_t = 0xC0FFEE_u64)]
    seed: u64,

    /// ITCH replay pacing: "max" (drain as fast as possible) or a numeric
    /// speedup factor (e.g. "1.0" = real time, "10.0" = 10× faster).
    #[arg(long, default_value = "max")]
    ich_pace: String,

    /// ITCH framing: "binaryfile" (NASDAQ official `*.ITCH50` daily dumps,
    /// 2-byte BE length prefix per message — the default) or "unframed"
    /// (raw concatenated messages, e.g. internal fixtures).
    #[arg(long, default_value = "binaryfile")]
    ich_framing: String,

    /// Fast-forward the ITCH replay to this seconds-since-midnight ET.
    /// Default 34200 (= 09:30 ET, regular-hours open). Use 0 to start at
    /// the very first message of the file (pre-market noise).
    #[arg(long, default_value_t = 34_200_u32)]
    ich_skip_to_secs: u32,

    /// TCP listen address for telemetry consumers.
    #[arg(long, default_value = "127.0.0.1:9090")]
    listen: String,

    /// Wire codec: bincode | json. JSON is for human debugging only.
    #[arg(long, default_value = "bincode")]
    wire: String,

    /// Tick publish rate (Hz) — dashboard refresh cadence.
    #[arg(long, default_value_t = 50)]
    tick_hz: u32,

    /// Book ladder publish rate (Hz).
    #[arg(long, default_value_t = 10)]
    book_hz: u32,

    /// mpsc channel capacity (drops on overflow, never blocks).
    #[arg(long, default_value_t = 8192)]
    capacity: usize,

    /// Pin the engine to a specific ITCH stock_locate (1-based). If omitted,
    /// the engine auto-locks to the first liquid ticker in the feed.
    #[arg(long)]
    symbol: Option<u32>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_target(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,flowlab_engine=debug")),
        )
        .init();

    let cli = Cli::parse();
    let codec = Codec::parse(&cli.wire);

    info!(
        source = %cli.source,
        source_arg = %cli.source_arg,
        listen = %cli.listen,
        ?codec,
        capacity = cli.capacity,
        "starting flowlab-engine"
    );

    // Build the Source.
    let mut source: Box<dyn Source> = match cli.source.as_str() {
        "synthetic" => Box::new(SyntheticSource::new(cli.seed)),
        "ich" => {
            if cli.source_arg.is_empty() {
                return Err("--source ich requires --source-arg <path-to-itch-file>".into());
            }
            let pace = match cli.ich_pace.as_str() {
                "max" => IchPace::AsFastAsPossible,
                s => match s.parse::<f64>() {
                    Ok(x) if x > 0.0 => IchPace::Realtime { speedup: x },
                    _ => return Err(format!("invalid --ich-pace: {s}").into()),
                },
            };
            let framing = match cli.ich_framing.as_str() {
                "binaryfile" | "binary" | "framed" => IchFraming::BinaryFile,
                "unframed" | "raw" => IchFraming::Unframed,
                other => return Err(format!("invalid --ich-framing: {other}").into()),
            };
            let start_at_ns = if cli.ich_skip_to_secs > 0 {
                Some(cli.ich_skip_to_secs as u64 * 1_000_000_000)
            } else {
                None
            };
            let s = IchSource::open_with(&cli.source_arg, pace, framing, start_at_ns)?;
            info!(
                file = %cli.source_arg,
                bytes = s.bytes_total(),
                ?pace,
                ?framing,
                "ITCH source opened"
            );
            Box::new(s)
        }
        "binance" => unimplemented!("BinanceSource lands in the step after ich"),
        other => return Err(format!("unknown source: {other}").into()),
    };

    // Bounded channel — see backpressure.rs.
    let (producer, consumer) = backpressure::channel::<TelemetryFrame>(cli.capacity);
    let consumer = Arc::new(consumer);

    // Telemetry server on its own thread.
    let server = TelemetryServer::bind(&cli.listen, codec)?;
    let cons_for_srv = consumer.clone();
    let _srv_handle = std::thread::Builder::new()
        .name("flowlab-telemetry".into())
        .spawn(move || {
            if let Err(e) = server.serve(cons_for_srv) {
                tracing::error!(target: "engine", error = %e, "telemetry server died");
            }
        })?;

    // Engine drives the Source on the main thread.
    let cfg = EngineConfig {
        tick_publish_hz: cli.tick_hz,
        book_publish_hz: cli.book_hz,
        track_instrument: cli.symbol,
        ..Default::default()
    };
    let engine = Engine::new(cfg);
    engine.run(source.as_mut(), producer);

    Ok(())
}
