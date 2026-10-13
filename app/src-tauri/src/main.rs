//! WebCodecs viewer: plays one local Annex B .h264 file, or live video from a paired host.
//!
//! Usage: nocomputer-viewer [--check] [--require-hardware | --software] <file.h264>
//!        nocomputer-viewer [--check] [--require-hardware | --software] --connect HOST[:PORT]
//!
//! The page can only read the file named on the command line. In --check mode the
//! page decodes every access unit as fast as the decoder accepts them (live: the first
//! 300 frames), then this process prints a one-line JSON summary (no pixel data) and
//! exits 0 on pass. Live mode needs the host paired first (see live.rs).
//!
//! nocomputer-viewer --bench-ipc [file.h264] measures Rust-to-page message latency instead,
//! optionally while decoding the file in a loop (see bench.rs).

mod annexb;
mod bench;
mod live;

use serde::Serialize;
use std::path::PathBuf;
use tauri::ipc::Response;
use tauri::{Manager, State};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Decode {
    /// WebCodecs `no-preference`.
    Default,
    /// WebCodecs `prefer-hardware`; Chromium fails configuration rather than falling back.
    RequireHardware,
    /// WebCodecs `prefer-software`.
    Software,
}

#[derive(Debug, Clone, Serialize)]
struct Launch {
    check: bool,
    decode: Decode,
    #[serde(skip)]
    path: PathBuf,
    file_name: String,
    /// Live mode: the host address as given.
    connect: Option<String>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Launch, String> {
    let mut check = false;
    let mut decode = Decode::Default;
    let mut path = None;
    let mut connect = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--connect" => connect = Some(args.next().ok_or("--connect needs HOST[:PORT]")?),
            "--check" => check = true,
            "--require-hardware" => decode = Decode::RequireHardware,
            "--software" => decode = Decode::Software,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s if path.is_none() => path = Some(PathBuf::from(s)),
            _ => return Err("expected exactly one .h264 file".into()),
        }
    }
    if let Some(host) = connect {
        if path.is_some() {
            return Err("give either a file or --connect, not both".into());
        }
        return Ok(Launch { check, decode, path: PathBuf::new(), file_name: String::new(), connect: Some(host) });
    }
    let path = path.ok_or("usage: nocomputer-viewer [--check] [--require-hardware | --software] <file.h264>")?;
    if path.extension().and_then(|e| e.to_str()) != Some("h264") {
        return Err("input must be a .h264 file".into());
    }
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > annexb::MAX_STREAM_BYTES as u64 {
        return Err(format!("{} exceeds {} bytes", path.display(), annexb::MAX_STREAM_BYTES));
    }
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(Launch { check, decode, path, file_name, connect: None })
}

#[tauri::command]
fn launch_options(launch: State<'_, Launch>) -> Launch {
    launch.inner().clone()
}

/// Layout: u32 LE header length, JSON header {codec, units: [{key, len}]}, then the
/// access units back to back. Raw bytes avoid JSON-encoding the video.
#[tauri::command]
fn load_stream(launch: State<'_, Launch>) -> Result<Response, String> {
    let data = std::fs::read(&launch.path).map_err(|e| e.to_string())?;
    let stream = annexb::parse(&data).map_err(|e| e.to_string())?;
    let header = serde_json::json!({
        "codec": stream.codec,
        "units": stream.units.iter().map(|u| serde_json::json!({ "key": u.key, "len": u.data.len() })).collect::<Vec<_>>(),
    })
    .to_string();
    let mut out = Vec::with_capacity(4 + header.len() + data.len());
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    for u in &stream.units {
        out.extend_from_slice(&u.data);
    }
    Ok(Response::new(out))
}

#[tauri::command]
fn finish_check(app: tauri::AppHandle, summary: serde_json::Value, pass: bool) {
    println!("{summary}");
    app.exit(if pass { 0 } else { 1 });
}

#[tauri::command]
fn set_fullscreen(window: tauri::WebviewWindow, on: bool) -> Result<(), String> {
    // Borderless fullscreen on the current monitor; never changes display mode.
    window.set_fullscreen(on).map_err(|e| e.to_string())
}

fn main() {
    // --bench-ipc [file.h264]: with a file, the page also decodes and draws it in a loop as load.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let bench_ipc = args.first().map(String::as_str) == Some("--bench-ipc");
    if bench_ipc {
        args.remove(0);
    }
    let launch = if bench_ipc && args.is_empty() {
        None
    } else {
        match parse_args(args.into_iter()) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
    };
    tauri::Builder::default()
        .manage(bench::PullState::default())
        .setup(move |app| {
            let page = if bench_ipc { "bench.html" } else { "index.html" };
            if let Some(launch) = launch {
                if let Some(host) = &launch.connect {
                    app.manage(live::Live::new(host));
                }
                app.manage(launch);
            }
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App(page.into()))
                .title("nocomputer viewer")
                .inner_size(960.0, 600.0)
                .build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            launch_options,
            load_stream,
            finish_check,
            set_fullscreen,
            live::live_next,
            live::live_request_keyframe,
            live::live_info,
            bench::bench_now,
            bench::bench_push,
            bench::bench_pull_start,
            bench::bench_pull,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(live) = app.try_state::<live::Live>() {
                    live.close(std::time::Duration::from_secs(2));
                }
            }
        });
}
