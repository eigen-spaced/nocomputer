//! --bench-ipc: how quickly video-sized messages get from Rust into the page.
//!
//! Each message is due at start + seq * interval. Its first 8 bytes carry the due time
//! (microseconds on a monotonic clock local to this process, f64 LE) and the next 4 its
//! sequence number; the rest is filler. The page records arrival on its own monotonic
//! clock (performance.now()), mapped onto ours by an offset it calibrates NTP-style with
//! `bench_now`, so latency = arrival - due and includes any backlog. No wall clock is used.
//! Two transports are compared:
//!   push: a Tauri Channel. Raw payloads over 1 KiB are announced by eval and then
//!         fetched by the page, i.e. one extra round trip per message.
//!   pull: the page invokes `bench_pull` for each sequence number; the command waits
//!         until that message is due and returns it as the invoke response.

use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tauri::ipc::{Channel, InvokeResponseBody, Response};

const MAX_SIZE: usize = 4 * 1024 * 1024;
const MAX_FRAMES: u32 = 100_000;

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

fn mono_us(t: Instant) -> f64 {
    t.saturating_duration_since(epoch()).as_secs_f64() * 1e6
}

#[derive(Clone, Copy)]
struct Schedule {
    start: Instant,
    interval: Duration,
}

impl Schedule {
    fn new(fps: u32) -> Self {
        // A short lead so the first message is not already late.
        let lead = Duration::from_millis(50);
        Schedule { start: Instant::now() + lead, interval: Duration::from_secs(1) / fps.max(1) }
    }

    /// Sleeps until message `seq` is due, then builds it.
    fn message(&self, seq: u32, size: usize) -> Vec<u8> {
        let due = self.start + self.interval * seq;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let due_us = mono_us(due);
        let mut msg = vec![0x5a; size.max(12)];
        msg[..8].copy_from_slice(&due_us.to_le_bytes());
        msg[8..12].copy_from_slice(&seq.to_le_bytes());
        msg
    }
}

fn check(size: usize, frames: u32, fps: u32) -> Result<(), String> {
    if size > MAX_SIZE || frames > MAX_FRAMES || fps == 0 || fps > 1000 {
        return Err("size, frames or fps out of range".into());
    }
    Ok(())
}

#[derive(Default)]
pub struct PullState(std::sync::Mutex<Option<(Schedule, usize, u32)>>);

#[tauri::command]
pub fn bench_now() -> f64 {
    mono_us(Instant::now())
}

#[tauri::command]
pub fn bench_push(channel: Channel<InvokeResponseBody>, size: usize, frames: u32, fps: u32) -> Result<(), String> {
    check(size, frames, fps)?;
    let schedule = Schedule::new(fps);
    std::thread::spawn(move || {
        for seq in 0..frames {
            if channel.send(InvokeResponseBody::Raw(schedule.message(seq, size))).is_err() {
                break;
            }
        }
    });
    Ok(())
}

#[tauri::command]
pub fn bench_pull_start(state: tauri::State<'_, PullState>, size: usize, frames: u32, fps: u32) -> Result<(), String> {
    check(size, frames, fps)?;
    *state.0.lock().unwrap() = Some((Schedule::new(fps), size, frames));
    Ok(())
}

#[tauri::command]
pub async fn bench_pull(state: tauri::State<'_, PullState>, seq: u32) -> Result<Response, String> {
    let (schedule, size, frames) = state.0.lock().unwrap().ok_or("bench_pull_start not called")?;
    if seq >= frames {
        return Err("past the last message".into());
    }
    // Sleep off the async runtime's worker threads.
    let msg = tauri::async_runtime::spawn_blocking(move || schedule.message(seq, size)).await.map_err(|e| e.to_string())?;
    Ok(Response::new(msg))
}
