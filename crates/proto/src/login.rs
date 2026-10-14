//! Account login after pairing (protocol v4).
//!
//! Pairing proves which device is connecting; the login proves that its user knows the
//! password of the account the host runs for. The viewer sends `Login` once per attempt
//! on the control stream, which is already inside the mutually authenticated QUIC/TLS
//! connection. Until a login succeeds the host gives no video, input, clipboard or
//! session.
//!
//! Failures share one generic answer and a growing delay, held per device and for the
//! whole host; the longer of the two applies. An attempt made before the delay has
//! passed is refused without checking the password. Neither side stores or logs the
//! password; buffers holding it are zeroized where this crate controls them.

use crate::identity::Fingerprint;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub const MAX_PASSWORD: usize = 1024;
pub const MAX_USERNAME: usize = 255;

/// A login password. `Debug` never shows it, there is no `Display`, and the memory is
/// zeroized on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(Zeroizing<String>);

impl Password {
    /// `None` if the password is empty, longer than [`MAX_PASSWORD`] bytes or contains
    /// U+0000. The string is moved in, not copied.
    pub fn new(password: String) -> Option<Self> {
        let password = Password(Zeroizing::new(password));
        password.is_valid().then_some(password)
    }

    pub(crate) fn is_valid(&self) -> bool {
        !self.0.is_empty() && self.0.len() <= MAX_PASSWORD && !self.0.contains('\0')
    }

    /// For the platform check only. Never log or store it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(redacted)")
    }
}

impl Serialize for Password {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Password {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Zeroizing::new(String::deserialize(d)?);
        // An invalid password is kept, so the host answers it like any wrong one.
        Ok(Password(raw))
    }
}

/// Usernames are compared by the platform check; here they are only bounded.
pub fn valid_username(name: &str) -> bool {
    (1..=MAX_USERNAME).contains(&name.len()) && !name.chars().any(char::is_control)
}

/// What the host's verifier is asked. `Debug` hides the password.
#[derive(Debug)]
pub struct LoginAttempt {
    /// The device that authenticated this connection.
    pub device: Fingerprint,
    pub username: String,
    pub password: Password,
}

/// The growing delay after failed logins.
#[derive(Clone, Copy, Debug)]
pub struct LoginDelay {
    /// The delay after each of the first two failures, and the unit it doubles from.
    pub base: Duration,
    pub cap: Duration,
}

impl Default for LoginDelay {
    fn default() -> Self {
        LoginDelay { base: Duration::from_secs(1), cap: Duration::from_secs(300) }
    }
}

impl LoginDelay {
    /// 1, 1, 2, 4, 8 ... times `base` after the 1st, 2nd, 3rd ... consecutive failure.
    fn after(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(2).min(20);
        self.base.saturating_mul(1 << doublings).min(self.cap)
    }
}

#[derive(Default, Clone, Copy)]
struct Backoff {
    failures: u32,
    until: Option<Instant>,
}

impl Backoff {
    fn fail(&mut self, delay: &LoginDelay, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.until = Some(now + delay.after(self.failures));
    }
}

/// Failure counts per device and host-wide. In memory: they reset when the host
/// restarts, which needs the host's own user.
#[derive(Default)]
pub(crate) struct LoginGate {
    devices: HashMap<Fingerprint, Backoff>,
    host: Backoff,
}

impl LoginGate {
    /// How long `device` must still wait before an attempt is checked.
    pub(crate) fn wait(&self, device: &Fingerprint, now: Instant) -> Option<Duration> {
        let until = [self.devices.get(device).and_then(|b| b.until), self.host.until].into_iter().flatten().max()?;
        (until > now).then(|| until - now)
    }

    /// Records a failure; returns the device's consecutive failures and its wait.
    pub(crate) fn fail(&mut self, device: Fingerprint, delay: &LoginDelay, now: Instant) -> (u32, Duration) {
        let entry = self.devices.entry(device).or_default();
        entry.fail(delay, now);
        let failures = entry.failures;
        self.host.fail(delay, now);
        (failures, self.wait(&device, now).unwrap_or_default())
    }

    pub(crate) fn succeed(&mut self, device: &Fingerprint) {
        self.devices.remove(device);
        self.host = Backoff::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(byte: &str) -> Fingerprint {
        byte.repeat(32).parse().unwrap()
    }

    #[test]
    fn delay_grows_and_is_capped() {
        let d = LoginDelay { base: Duration::from_secs(1), cap: Duration::from_secs(300) };
        let secs: Vec<u64> = (1..=12).map(|n| d.after(n).as_secs()).collect();
        assert_eq!(secs, [1, 1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        assert_eq!(d.after(u32::MAX), Duration::from_secs(300));
    }

    #[test]
    fn gate_holds_per_device_and_host_wide() {
        let d = LoginDelay::default();
        let (a, b) = (fp("01"), fp("02"));
        let now = Instant::now();
        let mut gate = LoginGate::default();
        assert_eq!(gate.wait(&a, now), None);
        for _ in 0..3 {
            gate.fail(a, &d, now);
        }
        assert_eq!(gate.wait(&a, now), Some(Duration::from_secs(2)));
        // Another device waits for the host-wide delay too.
        assert_eq!(gate.wait(&b, now), Some(Duration::from_secs(2)));
        assert_eq!(gate.wait(&a, now + Duration::from_secs(3)), None);
        // A success elsewhere clears the host-wide count but not the device's.
        gate.succeed(&b);
        assert_eq!(gate.wait(&b, now), None);
        assert_eq!(gate.fail(a, &d, now), (4, Duration::from_secs(4)));
    }

    #[test]
    fn password_is_bounded_and_never_shown() {
        assert!(Password::new(String::new()).is_none());
        assert!(Password::new("a\0b".into()).is_none());
        assert!(Password::new("x".repeat(MAX_PASSWORD + 1)).is_none());
        let p = Password::new("hunter2".into()).unwrap();
        assert_eq!(format!("{p:?}"), "Password(redacted)");
        let attempt = LoginAttempt { device: fp("00"), username: "u".into(), password: p };
        assert!(!format!("{attempt:?}").contains("hunter2"));
    }
}
