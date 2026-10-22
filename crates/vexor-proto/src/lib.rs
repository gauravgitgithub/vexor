//! Wire protocol between the daemon and shims (docs/architecture.md §7).
//!
//! Newline-delimited JSON over a per-app Unix socket, one message per line.
//! The daemon opens the connection and sends [`ToShim::Hello`]; the shim
//! answers with [`FromShim::Welcome`], replays any buffered events the daemon
//! missed, then streams new ones.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped on any incompatible change.
pub const PROTOCOL: u32 = 1;

/// Longest accepted message line, in bytes.
pub const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToShim {
    /// First message on every connection. With `resume`, the shim replays
    /// buffered events after `last_seq`, provided it is the same shim
    /// incarnation (`shim_start`). Otherwise, or without `resume`, it
    /// replays everything it still has buffered.
    Hello {
        protocol: u32,
        resume: Option<Resume>,
    },
    /// Stop every instance gracefully, then exit.
    Stop,
    /// Restart every instance (also revives stopped or errored ones).
    Restart,
    /// Reload every instance: start-first where the app's reload policy
    /// allows it, otherwise a restart.
    Reload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resume {
    pub shim_start: u64,
    pub last_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FromShim {
    Welcome {
        protocol: u32,
        app: String,
        shim_pid: u32,
        /// Process start time of the shim (see `Identity`).
        shim_start: u64,
        /// Sequence number the next new event will get.
        next_seq: u64,
        /// Latest `state` event of each instance.
        snapshot: Vec<serde_json::Value>,
    },
    Event {
        seq: u64,
        event: serde_json::Value,
    },
    /// Events `from..=to` were dropped from the shim's bounded buffer before
    /// the daemon could read them.
    Gap {
        from: u64,
        to: u64,
    },
    /// The connection was rejected (e.g. protocol mismatch); the shim closes it.
    Refused {
        reason: String,
    },
}

// ----------------------------------------------------------- CLI ↔ daemon

/// One request per connection, from the CLI to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRequest {
    pub protocol: u32,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Add an app and start it. Idempotent for an identical spec that is
    /// already running.
    Start {
        spec: Box<vexor_core::AppSpec>,
        /// False: add the app but leave it stopped (PM2 `autostart: false`).
        #[serde(default = "yes")]
        start: bool,
    },
    /// Start apps that already exist.
    StartExisting {
        target: Target,
    },
    Stop {
        target: Target,
    },
    Restart {
        target: Target,
    },
    Reload {
        target: Target,
    },
    /// Stop and forget.
    Delete {
        target: Target,
    },
    List,
    /// Write every app's configuration and desired state to
    /// `$VEXOR_HOME/dump.json` (PM2 `save`).
    Save,
    /// Restore apps from `dump.json` (PM2 `resurrect`): add missing apps and
    /// start those saved as running.
    Resurrect,
    /// One app's configuration, live status and recent events.
    Describe {
        name: String,
    },
    /// Stored event history, newest `limit` events, oldest first.
    Events {
        app: Option<String>,
        since_ms: Option<u64>,
        limit: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    All,
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Ok {
        apps: Vec<AppStatus>,
        /// Problems the operator should know about (e.g. history reset).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    Events {
        events: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    Describe {
        status: AppStatus,
        spec: Box<vexor_core::AppSpec>,
        /// Most recent events, oldest first.
        events: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    /// An app with this name exists with a different spec.
    Conflict,
    InvalidSpec,
    /// Some targets succeeded, some failed (the message says which).
    Partial,
    Protocol,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppStatus {
    pub name: String,
    pub desired: Desired,
    /// Present while the daemon is connected to the app's shim.
    pub shim_pid: Option<u32>,
    pub instances: u32,
    /// Latest `state` event per instance, as reported by the shim.
    pub snapshot: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Desired {
    Running,
    Stopped,
}

fn yes() -> bool {
    true
}

/// Write one message as a JSON line.
pub async fn send<W, T>(w: &mut W, msg: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(msg).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

/// Read one message. `Ok(None)` means the peer closed the connection.
/// Malformed or overlong lines are errors: the protocol has no recovery.
pub async fn recv<R, T>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    buf.clear();
    match read_bounded_line(r, buf, MAX_LINE).await? {
        LineRead::Eof => Ok(None),
        LineRead::TooLong => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message exceeds MAX_LINE",
        )),
        LineRead::Line => serde_json::from_slice(buf)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LineRead {
    Line,
    TooLong,
    Eof,
}

/// Read one `\n`-terminated line into `buf` without letting it grow past
/// `max`. An overlong line is consumed and reported as `TooLong`.
pub async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> io::Result<LineRead> {
    let mut too_long = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(LineRead::Eof);
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !too_long {
            if buf.len() + take > max + 1 {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        r.consume(take);
        if done {
            return Ok(if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn round_trip() {
        let mut wire = Vec::new();
        let msgs = [
            ToShim::Hello {
                protocol: PROTOCOL,
                resume: Some(Resume {
                    shim_start: 7,
                    last_seq: 41,
                }),
            },
            ToShim::Stop,
        ];
        for m in &msgs {
            send(&mut wire, m).await.expect("send");
        }
        let mut r = BufReader::new(&wire[..]);
        let mut buf = Vec::new();
        for m in &msgs {
            let got: ToShim = recv(&mut r, &mut buf).await.expect("recv").expect("msg");
            assert_eq!(&got, m);
        }
        assert!(
            recv::<_, ToShim>(&mut r, &mut buf)
                .await
                .expect("eof")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_unknown_messages() {
        let mut r = BufReader::new(&b"{\"type\":\"launch_missiles\"}\n"[..]);
        let mut buf = Vec::new();
        assert!(recv::<_, ToShim>(&mut r, &mut buf).await.is_err());
    }

    #[tokio::test]
    async fn overlong_lines_are_skipped_without_buffering_them() {
        let mut input = vec![b'x'; 200];
        input.extend_from_slice(b"\nok\n");
        let mut r = BufReader::with_capacity(16, &input[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_bounded_line(&mut r, &mut buf, 64).await.expect("read"),
            LineRead::TooLong
        );
        assert!(buf.len() <= 65);
        buf.clear();
        assert_eq!(
            read_bounded_line(&mut r, &mut buf, 64).await.expect("read"),
            LineRead::Line
        );
        assert_eq!(buf, b"ok\n");
        buf.clear();
        assert_eq!(
            read_bounded_line(&mut r, &mut buf, 64).await.expect("read"),
            LineRead::Eof
        );
    }
}
