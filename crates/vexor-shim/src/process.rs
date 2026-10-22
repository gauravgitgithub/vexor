//! Starting and signalling one app process.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::process::Stdio;

use command_fds::{CommandFdExt, FdMapping};
use rustix::process::{Pid, Signal, kill_process, kill_process_group, test_kill_process_group};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use vexor_core::AppSpec;

/// File descriptor number Node expects its IPC channel on.
const NODE_CHANNEL_FD: i32 = 3;
/// Longest IPC message accepted from an app. Longer lines are discarded.
const MAX_IPC_LINE: usize = 64 * 1024;

pub struct Spawned {
    pub child: Child,
    pub pid: u32,
    pub ipc: Option<IpcChannel>,
    pub log_tasks: Vec<JoinHandle<()>>,
}

/// Start instance `index` of `spec`. The process gets exactly `spec.env`
/// (plus the instance variable and, for Node apps, the IPC channel), its own
/// process group, `/dev/null` as stdin, and stdout/stderr appended to the
/// spec's log files.
pub fn spawn(spec: &AppSpec, index: u32) -> io::Result<Spawned> {
    let mut cmd = Command::new(&spec.exec);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .env(&spec.instance_var, index.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(false);

    let mut parent_end = None;
    if spec.node_ipc {
        let (ours, theirs) = StdUnixStream::pair()?;
        cmd.env("NODE_CHANNEL_FD", NODE_CHANNEL_FD.to_string())
            .env("NODE_CHANNEL_SERIALIZATION_MODE", "json")
            .fd_mappings(vec![FdMapping {
                parent_fd: OwnedFd::from(theirs),
                child_fd: NODE_CHANNEL_FD,
            }])
            .map_err(io::Error::other)?;
        parent_end = Some(ours);
    }

    let out = open_log(&spec.logs.out)?;
    let err = open_log(&spec.logs.err)?;
    let mut child = cmd.spawn()?;
    // Dropping `cmd` closes our copy of the child's end of the IPC socket, so
    // we see EOF once the app exits.
    drop(cmd);

    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("child exited before its pid was read"))?;
    let mut log_tasks = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        log_tasks.push(tokio::spawn(copy_to_log(stdout, out)));
    }
    if let Some(stderr) = child.stderr.take() {
        log_tasks.push(tokio::spawn(copy_to_log(stderr, err)));
    }
    let ipc = match parent_end {
        Some(s) => {
            s.set_nonblocking(true)?;
            Some(IpcChannel::new(UnixStream::from_std(s)?))
        }
        None => None,
    };
    Ok(Spawned {
        child,
        pid,
        ipc,
        log_tasks,
    })
}

/// Open a log file for appending: owner read/write, group read, and never
/// through a symlink (docs/security.md T5).
fn open_log(path: &Path) -> io::Result<tokio::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o640)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("log file {}: {e}", path.display())))?;
    Ok(tokio::fs::File::from_std(f))
}

async fn copy_to_log(mut from: impl AsyncRead + Unpin, mut to: tokio::fs::File) {
    // Write errors are surfaced properly with log rotation (plan step 2.7).
    // Until then a failed write ends the copy; the app keeps running, and its
    // writes fail with EPIPE once the pipe closes.
    if let Err(e) = tokio::io::copy(&mut from, &mut to).await {
        eprintln!("vexor shim: log copy stopped: {e}");
    }
}

/// Send `sig` to the whole process group led by `pid`. A group that is
/// already gone is not an error.
pub fn signal_group(pid: u32, sig: Signal) -> io::Result<()> {
    match kill_process_group(to_pid(pid)?, sig) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Send `sig` to `pid` only. A process that is already gone is not an error.
pub fn signal_process(pid: u32, sig: Signal) -> io::Result<()> {
    match kill_process(to_pid(pid)?, sig) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// True while any process (including zombies) is in the group `pid` led.
pub fn group_alive(pid: u32) -> bool {
    match to_pid(pid) {
        Ok(p) => !matches!(test_kill_process_group(p), Err(rustix::io::Errno::SRCH)),
        Err(_) => false,
    }
}

fn to_pid(pid: u32) -> io::Result<Pid> {
    i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Parent end of a Node IPC channel (newline-delimited JSON).
pub struct IpcChannel {
    reader: BufReader<UnixStream>,
    buf: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IpcMessage {
    /// `process.send('ready')`.
    Ready,
    Other,
}

impl IpcChannel {
    fn new(stream: UnixStream) -> Self {
        IpcChannel {
            reader: BufReader::new(stream),
            buf: Vec::new(),
        }
    }

    /// Next message, or `None` once the app closes the channel.
    pub async fn recv(&mut self) -> Option<IpcMessage> {
        loop {
            self.buf.clear();
            match read_bounded_line(&mut self.reader, &mut self.buf, MAX_IPC_LINE).await {
                Ok(LineRead::Eof) | Err(_) => return None,
                Ok(LineRead::TooLong) => continue,
                Ok(LineRead::Line) => return Some(parse_ipc(&self.buf)),
            }
        }
    }
}

fn parse_ipc(line: &[u8]) -> IpcMessage {
    match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(serde_json::Value::String(s)) if s == "ready" => IpcMessage::Ready,
        _ => IpcMessage::Other,
    }
}

enum LineRead {
    Line,
    TooLong,
    Eof,
}

/// Read one `\n`-terminated line into `buf` without letting it grow past
/// `max`. An overlong line is consumed and reported as `TooLong`.
async fn read_bounded_line<R: AsyncBufReadExt + Unpin>(
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

    #[test]
    fn parses_ready_and_ignores_the_rest() {
        assert_eq!(parse_ipc(b"\"ready\"\n"), IpcMessage::Ready);
        assert_eq!(
            parse_ipc(b"{\"cmd\":\"NODE_HANDLE_ACK\"}\n"),
            IpcMessage::Other
        );
        assert_eq!(parse_ipc(b"\"ready!\"\n"), IpcMessage::Other);
        assert_eq!(parse_ipc(b"not json\n"), IpcMessage::Other);
    }

    #[tokio::test]
    async fn overlong_lines_are_skipped_without_buffering_them() {
        let mut input = vec![b'x'; 200];
        input.extend_from_slice(b"\n\"ready\"\n");
        let mut r = BufReader::with_capacity(16, &input[..]);
        let mut buf = Vec::new();
        assert!(matches!(
            read_bounded_line(&mut r, &mut buf, 64).await,
            Ok(LineRead::TooLong)
        ));
        assert!(buf.len() <= 65);
        buf.clear();
        assert!(matches!(
            read_bounded_line(&mut r, &mut buf, 64).await,
            Ok(LineRead::Line)
        ));
        assert_eq!(buf, b"\"ready\"\n");
        buf.clear();
        assert!(matches!(
            read_bounded_line(&mut r, &mut buf, 64).await,
            Ok(LineRead::Eof)
        ));
    }
}
