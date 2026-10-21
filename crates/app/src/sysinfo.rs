//! RAM do próprio processo (VmRSS de `/proc/self/status`).

use std::time::{Duration, Instant};

/// RSS em bytes, se o `/proc` estiver disponível.
pub fn rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_vm_rss(&status)
}

fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Lê a RAM no máximo a cada 2 s (só quando a UI redesenha: parado, não acorda).
#[derive(Default)]
pub struct RamMeter {
    last: Option<(Instant, Option<u64>)>,
}

impl RamMeter {
    pub fn bytes(&mut self) -> Option<u64> {
        match self.last {
            Some((at, value)) if at.elapsed() < Duration::from_secs(2) => value,
            _ => {
                let value = rss_bytes();
                self.last = Some((Instant::now(), value));
                value
            }
        }
    }

    pub fn label(&mut self) -> String {
        self.bytes()
            .map_or_else(|| "?".to_owned(), |b| format!("{} MB", b / (1024 * 1024)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status() {
        let status = "Name:\torquestra\nVmPeak:\t  100 kB\nVmRSS:\t   42000 kB\n";
        assert_eq!(parse_vm_rss(status), Some(42000 * 1024));
        assert_eq!(parse_vm_rss("nada"), None);
        assert!(rss_bytes().is_some());
    }
}
