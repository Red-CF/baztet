//! Live console capture: the drain writes here as bazel runs; the dashboard
//! reads progress from it and the post-run presentation reads the failure
//! tail. Bazel buffers the BEP sidecar and writes it whole at exit, so this
//! — not the sidecar — is the only streaming data source.

use std::sync::Arc;
use tokio::io::AsyncReadExt;

/// Bounded shared buffer for one command's console output.
#[derive(Clone, Default)]
pub struct ConsoleCapture {
    inner: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl ConsoleCapture {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Append keeping the tail at a line boundary; a single line bigger
    /// than the cap falls to a cut at a character boundary and the dump
    /// presents it whole.
    pub fn append(&self, chunk: &[u8], cap: usize) {
        let mut buf = self.inner.lock().unwrap();
        buf.extend_from_slice(chunk);
        if buf.len() > cap * 2 {
            let cut = buf.len() - cap;
            let mut cut = buf[cut..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| buf.len() - cap + p + 1)
                .unwrap_or(cut);
            // `cut` must not split a UTF-8 char: walk past continuation bytes
            while cut > 0 && (buf[cut] & 0xC0) == 0x80 {
                cut += 1;
            }
            buf.drain(..cut);
        }
    }

    /// Snapshot of the whole retained tail.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.inner.lock().unwrap()).into_owned()
    }

    /// The most recent non-empty bazel progress line.
    pub fn last_progress(&self) -> String {
        String::from_utf8_lossy(&self.inner.lock().unwrap())
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty() && !l.contains("Computing main repo mapping"))
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

/// Drain one piped stream into `dst` for as long as bazel runs.
pub async fn drain<S: tokio::io::AsyncRead + Unpin>(mut stream: S, dst: ConsoleCapture) {
    const DRAIN_CAP: usize = 256 * 1024;
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => dst.append(&chunk[..n], DRAIN_CAP),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_tail_keeps_lines_and_last_progress() {
        let c = ConsoleCapture::new();
        let line = b"[1/3] Compiling //a:one ...ok\n".to_vec();
        let mut noise = Vec::new();
        for i in 0..500 {
            noise.extend(format!("line {i} filler padding to grow the buffer\n").as_bytes());
        }
        c.append(&line, 512);
        c.append(&noise, 512);
        // buffer stays bounded well under 2x cap
        assert!(c.text().len() < 2 * 512 + 1024);
        // the tail starts at a line boundary
        let tail = c.text();
        assert!(
            tail.starts_with("line ") || tail.starts_with("\n"),
            "tail: {tail:?}"
        );
        let c2 = ConsoleCapture::new();
        c2.append(b"Computing main repo mapping: \n", 1024);
        c2.append(b"[2/3] Building //a:two\n", 1024);
        assert_eq!(c2.last_progress(), "[2/3] Building //a:two");
        // whitespace-only tails are skipped
        c2.append(b"\n", 1024);
        assert_eq!(c2.last_progress(), "[2/3] Building //a:two");
    }

    #[test]
    fn giant_single_line_cuts_at_a_char_boundary() {
        let c = ConsoleCapture::new();
        let big = "x".repeat(600); // no newline anywhere
        c.append(big.as_bytes(), 128);
        assert!(c.text().len() < 128 * 2 + 16);
        // the retained tail ends inside the big line, intact chars
        assert!(c.text().ends_with('x'));
    }
}
