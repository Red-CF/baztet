//! Bazel process runner: `bazel info` parsing plus helpers for text stdout and varint-framed `--output=streamed_proto` decoding.

use std::process::Stdio;

use camino::Utf8PathBuf;
use prost::Message;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::workspace::BazelInfo;

#[derive(Debug, Error)]
pub enum BazelError {
    #[error("bazel workspace not found above {0}")]
    NoWorkspace(Utf8PathBuf),
    #[error("bazel command '{0}' failed with exit code {1}:\n{2}")]
    Failure(String, i32, String),
    #[error("failed to spawn bazel: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("failed to decode proto stream: {0}")]
    Decode(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone)]
pub struct BazelRunner {
    pub binary: String,
    pub workspace_root: Utf8PathBuf,
    pub info: BazelInfo,
    /// extra bazel flags appended to build/query invocations (e.g.
    /// `--noexperimental_check_output_files` behind `--no-output-check`).
    pub tuning_flags: Vec<String>,
    /// exec commands capture bazel's console output instead of streaming
    /// it (quiet UI, `--raw` disables). `run` stays interactive regardless.
    pub quiet: bool,
}

impl BazelRunner {
    /// Create a runner: run `bazel info`, parse workspace details.
    pub async fn new(binary: &str, workspace_root: Utf8PathBuf) -> Result<Self, BazelError> {
        let info = Self::run_info(binary, &workspace_root).await?;
        Ok(Self {
            binary: binary.to_string(),
            workspace_root,
            info,
            tuning_flags: Vec::new(),
            quiet: false,
        })
    }

    /// Runner pre-built from a cached `bazel info` text (snapshots carry it);
    /// caller sets tuning_flags.
    pub fn from_cached_info(binary: &str, workspace_root: Utf8PathBuf, info: BazelInfo) -> Self {
        Self {
            binary: binary.to_string(),
            workspace_root,
            info,
            tuning_flags: Vec::new(),
            quiet: false,
        }
    }

    /// Convenience: discover the workspace upward from `start` and probe bazel.
    pub async fn discover(binary: &str, start: &Utf8PathBuf) -> Result<Self, BazelError> {
        let workspace_root = crate::workspace::find_workspace_root(start)
            .ok_or_else(|| BazelError::NoWorkspace(start.clone()))?;
        Self::new(binary, workspace_root).await
    }

    async fn run_info(binary: &str, workspace_root: &Utf8PathBuf) -> Result<BazelInfo, BazelError> {
        let out = Command::new(binary)
            .current_dir(workspace_root)
            .arg("info")
            .kill_on_drop(true)
            .output()
            .await?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            return Err(BazelError::Failure(
                "info".into(),
                out.status.code().unwrap_or(-1),
                format!("{text}\n{}", String::from_utf8_lossy(&out.stderr)),
            ));
        }
        let mut info = crate::workspace::parse_info(&text);
        info.text = Some(text);
        Ok(info)
    }

    /// Blocking variant of [`run_simple`] for short helper commands.
    pub fn run_simple_blocking(
        &self,
        command: &str,
        args: &[String],
    ) -> Result<(i32, String, String), BazelError> {
        std::process::Command::new(&self.binary)
            .current_dir(&self.workspace_root)
            .arg(command)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map(|out| {
                (
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                )
            })
            .map_err(BazelError::Spawn)
    }

    /// Run a bazel command to completion; return (exit_code, stdout, stderr).
    pub async fn run_simple(
        &self,
        command: &str,
        args: &[String],
    ) -> Result<(i32, String, String), BazelError> {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.workspace_root)
            .arg(command)
            .args(args)
            .args(&self.tuning_flags)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let out = cmd.output().await?;
        Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    /// Run a bazel command whose stdout is a stream of length-delimited
    /// protobufs (`--output=streamed_proto` framing). Returns the decoded
    /// protos plus the command's exit code and stderr text.
    ///
    /// Errors keep going at the bazel level, so we always return the partial
    /// stream; the caller decides what a nonzero exit code means.
    pub async fn run_streaming_protos<M: Message + Default>(
        &self,
        command: &str,
        args: &[String],
    ) -> Result<(Vec<M>, i32, String), BazelError> {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.workspace_root)
            .arg(command)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| BazelError::Decode("child stdout not captured".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| BazelError::Decode("child stderr not captured".into()))?;

        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            buf
        });

        let protos = read_length_delimited(&mut stdout).await?;
        let status = child.wait().await?;
        Ok((
            protos,
            status.code().unwrap_or(-1),
            String::from_utf8_lossy(&stderr_task.await.unwrap_or_default()).into_owned(),
        ))
    }
}

/// Read length-delimited (varint-prefixed) protobufs until EOF.
async fn read_length_delimited<M: Message + Default>(
    reader: &mut (impl AsyncReadExt + Unpin + Send),
) -> Result<Vec<M>, BazelError> {
    let mut protos = Vec::new();
    loop {
        // read a varint
        let mut len: u64 = 0;
        let mut shift = 0u32;
        let mut eof = false;
        loop {
            let mut byte = [0u8; 1];
            match reader.read_exact(&mut byte).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    eof = true;
                    break;
                }
                Err(other) => return Err(BazelError::Decode(other.to_string())),
            }
            len |= u64::from(byte[0] & 0x7f) << shift;
            if byte[0] & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 64 {
                return Err(BazelError::Decode("varint too long".into()));
            }
        }
        if eof {
            return Ok(protos);
        }
        let len = usize::try_from(len).map_err(|_| BazelError::Decode("varint overflow".into()))?;
        let mut msg_buf = vec![0u8; len];
        reader
            .read_exact(&mut msg_buf)
            .await
            .map_err(|e| BazelError::Decode(format!("truncated proto: {e}")))?;
        let decoded =
            M::decode(msg_buf.as_slice()).map_err(|e| BazelError::Decode(e.to_string()))?;
        protos.push(decoded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[derive(Clone, PartialEq, prost::Message)]
    struct TestMsg {
        #[prost(int32, tag = "1")]
        pub value: i32,
    }

    /// read_length_delimited round-trips a stream of framed messages.
    #[tokio::test]
    async fn framed_roundtrip() {
        let mut stream = Vec::new();
        for value in [1, 2, -5] {
            let msg = TestMsg { value };
            msg.encode_length_delimited(&mut stream).unwrap();
        }
        let decoded = read_length_delimited::<TestMsg>(&mut stream.as_slice())
            .await
            .unwrap();
        assert_eq!(
            decoded,
            [
                TestMsg { value: 1 },
                TestMsg { value: 2 },
                TestMsg { value: -5 }
            ]
        );
    }

    /// A zero-byte stream decodes to zero messages.
    #[tokio::test]
    async fn empty_stream() {
        let decoded = read_length_delimited::<TestMsg>(&mut Vec::new().as_slice())
            .await
            .unwrap();
        assert!(decoded.is_empty());
    }
}
