//! Manual pairing tool for testing the protocol between real machines.
//!
//!   nocomputer-pair whoami
//!   nocomputer-pair host [--listen ADDR] [--name NAME]     (stdin: clients, revoke FP)
//!   nocomputer-pair pair HOST[:PORT] [--name NAME]
//!   nocomputer-pair connect HOST[:PORT] [--count N] [--name NAME]
//!   nocomputer-pair watch HOST[:PORT] [--seconds N] [--name NAME]   (video statistics)
//!   nocomputer-pair clients | revoke FINGERPRINT | forget HOST[:PORT]
//!
//! State lives in NOCOMPUTER_STATE_DIR, or the per-user app data folder. The private
//! key is kept in DPAPI (Windows) or the Keychain (macOS).

use nocomputer_proto::{
    load_or_create_identity, platform_keystore, Approver, Fingerprint, Host, HostConfig, HostEvent, Identity, KnownHosts, Outcome, PairedClients,
    paths, Viewer, DEFAULT_PORT,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

type BoxError = Box<dyn std::error::Error>;

fn state_dir() -> PathBuf {
    paths::default_state_dir()
}

fn identity() -> Result<Identity, BoxError> {
    let store = platform_keystore(&state_dir(), "device")?;
    Ok(load_or_create_identity(store.as_ref())?)
}

fn default_name() -> String {
    let raw = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "nocomputer device".into());
    raw.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string()
}

fn option(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn with_port(address: &str) -> String {
    paths::with_default_port(address)
}

async fn resolve(address: &str) -> Result<SocketAddr, BoxError> {
    Ok(paths::resolve(address).await?)
}

async fn read_line(lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>) -> String {
    lines.next_line().await.ok().flatten().unwrap_or_default()
}

fn yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("whoami") => whoami(),
        Some("host") => host(&args).await,
        Some("pair") if args.len() >= 2 => pair(&args).await,
        Some("connect") if args.len() >= 2 => connect(&args).await,
        Some("watch") if args.len() >= 2 => watch(&args).await,
        Some("clients") => clients(),
        Some("revoke") if args.len() >= 2 => revoke(&args[1..].join("")),
        Some("forget") if args.len() >= 2 => forget(&args[1]),
        _ => Err("usage: nocomputer-pair whoami | host [--listen ADDR] [--name NAME] | pair HOST[:PORT] [--name NAME] | connect HOST[:PORT] [--count N] [--name NAME] | watch HOST[:PORT] [--seconds N] [--name NAME] | clients | revoke FINGERPRINT | forget HOST[:PORT]".into()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn whoami() -> Result<(), BoxError> {
    println!("this device: {}", identity()?.fingerprint());
    println!("state: {}", state_dir().display());
    Ok(())
}

async fn host(args: &[String]) -> Result<(), BoxError> {
    let identity = identity()?;
    let listen: SocketAddr = option(args, "--listen").unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}")).parse()?;
    let name = option(args, "--name").unwrap_or_else(default_name);

    // One stdin reader: it answers a pending approval, otherwise runs a command.
    let pending: Arc<Mutex<Option<oneshot::Sender<bool>>>> = Arc::default();
    let approver: Approver = {
        let pending = pending.clone();
        Arc::new(move |p| {
            let (tx, rx) = oneshot::channel();
            *pending.lock().unwrap() = Some(tx);
            println!();
            println!("Pairing request from device \"{}\"", p.device_name);
            println!("  device fingerprint: {}", p.device_fingerprint);
            println!("  this host:          {}", p.host_fingerprint);
            println!("Check that the device shows exactly these two fingerprints.");
            println!("Approve? [y/N]");
            Box::pin(async move { rx.await.unwrap_or(false) })
        })
    };
    let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let mut config = HostConfig::new(name.clone());
    config.events = Some(events_tx);
    let host = Arc::new(Host::bind(listen, &identity, config, PairedClients::at(state_dir().join("paired-clients.json")), approver)?);

    println!("host \"{name}\" listening on {}", host.local_addr()?);
    println!("this host: {}", host.fingerprint());
    println!("commands: clients | revoke FINGERPRINT | quit");

    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                HostEvent::Session { device, name, reported_name } if reported_name != name => {
                    println!("session: \"{name}\" {device} (calls itself \"{reported_name}\", unverified)")
                }
                HostEvent::Session { device, name, .. } => println!("session: \"{name}\" {device}"),
                HostEvent::SessionEnded { device } => println!("session ended: {device}"),
                HostEvent::Paired { device, name } => println!("paired: \"{name}\" {device}"),
                HostEvent::PairingDenied { device } => println!("pairing denied or timed out: {device}"),
                HostEvent::Rejected { device: Some(d), reason } => println!("rejected {d}: {reason}"),
                HostEvent::Rejected { device: None, reason } => println!("rejected: {reason}"),
                // This host has no capture or input injection; input is never printed.
                HostEvent::KeyframeNeeded | HostEvent::Input(_) => {}
            }
        }
    });
    let runner = host.clone();
    tokio::spawn(async move { runner.run().await });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => break,
        };
        if let Some(tx) = pending.lock().unwrap().take() {
            if tx.send(yes(&line)).is_err() {
                println!("(that request already timed out)");
            }
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["clients"] => print_clients(&host.paired_clients()?),
            ["revoke", fp @ ..] => match fp.join("").parse::<Fingerprint>() {
                Ok(fp) => println!("{}", if host.revoke(&fp)? { "revoked; live connections closed" } else { "no such device" }),
                Err(e) => println!("{e}"),
            },
            ["quit"] => break,
            [] => {}
            _ => println!("commands: clients | revoke FINGERPRINT | quit"),
        }
    }
    host.close();
    Ok(())
}

async fn pair(args: &[String]) -> Result<(), BoxError> {
    let address = with_port(&args[1]);
    let name = option(args, "--name").unwrap_or_else(default_name);
    let viewer = Viewer::new(identity()?, name)?;
    let known = KnownHosts::at(state_dir().join("known-hosts.json"));
    if let Some(h) = known.get(&address)? {
        return Err(format!("{address} is already pinned to \"{}\" {}; run `forget {address}` first", h.name, h.fingerprint).into());
    }
    let Outcome::FirstContact(first) = viewer.connect(resolve(&address).await?, None).await? else {
        unreachable!("unpinned connections always return first contact")
    };
    println!("host \"{}\" at {address}", first.host_name);
    println!("  host fingerprint: {}", first.host_fingerprint);
    println!("  this device:      {}", viewer.fingerprint());
    println!("Check that the host shows exactly these two fingerprints. Continue? [y/N]");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    if !yes(&read_line(&mut lines).await) {
        return Err("not confirmed; nothing was saved".into());
    }
    let (host_fingerprint, host_name, already) = (first.host_fingerprint, first.host_name.clone(), first.already_paired);
    if !already {
        println!("waiting for approval on the host...");
    }
    if !first.request_pairing().await? {
        return Err("the host did not approve".into());
    }
    known.add(&address, host_fingerprint, &host_name)?;
    println!("paired with \"{host_name}\"; pinned {host_fingerprint}");
    Ok(())
}

async fn connect(args: &[String]) -> Result<(), BoxError> {
    let address = with_port(&args[1]);
    let count: u32 = option(args, "--count").map(|c| c.parse()).transpose()?.unwrap_or(5);
    let known = KnownHosts::at(state_dir().join("known-hosts.json"));
    let pinned = known.get(&address)?.ok_or_else(|| format!("{address} is not paired; run `pair {address}` first"))?;
    let viewer = Viewer::new(identity()?, option(args, "--name").unwrap_or_else(default_name))?;
    let Outcome::Ready(mut session) = viewer.connect(resolve(&address).await?, Some(pinned.fingerprint)).await? else {
        unreachable!("pinned connections return a session or an error")
    };
    println!("connected to \"{}\" {}", session.host_name, session.host_fingerprint);
    for _ in 0..count {
        println!("ping {:.2} ms", session.ping().await?.as_secs_f64() * 1000.0);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    session.close().await;
    Ok(())
}

/// Receives video and prints per-second statistics; frames are counted, never decoded
/// or stored.
async fn watch(args: &[String]) -> Result<(), BoxError> {
    let address = with_port(&args[1]);
    let seconds: u64 = option(args, "--seconds").map(|c| c.parse()).transpose()?.unwrap_or(10);
    let known = KnownHosts::at(state_dir().join("known-hosts.json"));
    let pinned = known.get(&address)?.ok_or_else(|| format!("{address} is not paired; run `pair {address}` first"))?;
    let viewer = Viewer::new(identity()?, option(args, "--name").unwrap_or_else(default_name))?;
    let Outcome::Ready(session) = viewer.connect(resolve(&address).await?, Some(pinned.fingerprint)).await? else {
        unreachable!("pinned connections return a session or an error")
    };
    println!("connected to \"{}\" {}", session.host_name, session.host_fingerprint);
    let mut video = session.video();
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut second = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    let (mut frames, mut keyframes, mut bytes, mut largest, mut max_gap_us, mut last_us) = (0u32, 0u32, 0usize, 0usize, 0u64, None::<u64>);
    let mut codec = String::new();
    loop {
        tokio::select! {
            frame = video.next() => {
                let f = frame?;
                frames += 1;
                keyframes += f.keyframe as u32;
                bytes += f.data.len();
                largest = largest.max(f.data.len());
                // Host clock differences only, never compared with this clock.
                if let Some(last) = last_us {
                    max_gap_us = max_gap_us.max(f.host_us.saturating_sub(last));
                }
                last_us = Some(f.host_us);
                if *f.codec != codec {
                    codec = f.codec.to_string();
                }
            }
            _ = tokio::time::sleep_until(second) => {
                println!("{frames:3} frames ({keyframes} key), {:6.1} kB/s, largest {:5.1} kB, max host gap {:5.1} ms, {codec}", bytes as f64 / 1000.0, largest as f64 / 1000.0, max_gap_us as f64 / 1000.0);
                (frames, keyframes, bytes, largest, max_gap_us) = (0, 0, 0, 0, 0);
                second += std::time::Duration::from_secs(1);
                if second > end {
                    break;
                }
            }
        }
    }
    session.close().await;
    Ok(())
}

fn print_clients(clients: &[nocomputer_proto::PairedClient]) {
    if clients.is_empty() {
        println!("no paired devices");
    }
    for c in clients {
        println!("{}{} \"{}\"", c.fingerprint, if c.revoked { " [revoked]" } else { "" }, c.name);
    }
}

fn clients() -> Result<(), BoxError> {
    print_clients(&PairedClients::at(state_dir().join("paired-clients.json")).list()?);
    Ok(())
}

fn revoke(fp: &str) -> Result<(), BoxError> {
    let fp: Fingerprint = fp.parse()?;
    let revoked = PairedClients::at(state_dir().join("paired-clients.json")).revoke(&fp)?;
    println!("{}", if revoked { "revoked (a running host refuses its next connection; use the host's own `revoke` to also close live ones)" } else { "no such device" });
    Ok(())
}

fn forget(address: &str) -> Result<(), BoxError> {
    let address = with_port(address);
    let removed = KnownHosts::at(state_dir().join("known-hosts.json")).forget(&address)?;
    println!("{}", if removed { "forgotten" } else { "not pinned" });
    Ok(())
}
