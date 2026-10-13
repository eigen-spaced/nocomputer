//! Where a device keeps its state, and host address normalization, shared by the tools
//! and the viewer app.

use std::path::PathBuf;

/// `NOCOMPUTER_STATE_DIR`, or the per-user app data folder: `%APPDATA%\nocomputer`,
/// `~/Library/Application Support/nocomputer` or `$XDG_CONFIG_HOME/nocomputer`.
pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NOCOMPUTER_STATE_DIR") {
        return dir.into();
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.unwrap_or_else(|| PathBuf::from(".")).join("nocomputer")
}

/// "host" -> "host:47900"; an explicit port is kept.
pub fn with_default_port(address: &str) -> String {
    let has_port = address.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !address.ends_with(']');
    if has_port {
        address.to_string()
    } else {
        format!("{address}:{}", crate::DEFAULT_PORT)
    }
}

/// Resolves, preferring IPv4: tailnet and LAN hosts are reliably reachable on it.
pub async fn resolve(address: &str) -> std::io::Result<std::net::SocketAddr> {
    let addrs: Vec<_> = tokio::net::lookup_host(address).await?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("{address} did not resolve")))
}
