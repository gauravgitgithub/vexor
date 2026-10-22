//! Starting and signalling one app process.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::process::Stdio;

use command_fds::{CommandFdExt, FdMapping};
use rustix::process::{Pid, Signal, kill_process, kill_process_group, test_kill_process_group};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use vexor_core::AppSpec;

use crate::logs::Sink;
use vexor_proto::{LineRead, read_bounded_line};

/// File descriptor number Node expects its IPC channel on.
const NODE_CHANNEL_FD: i32 = 3;
/// Longest IPC message accepted from an app. Longer lines are discarded.
const MAX_IPC_LINE: usize = 64 * 1024;

pub struct Spawned {
    pub child: Child,
    pub pid: u32,
    /// Our end of the Node IPC channel, if one was requested.
    pub ipc: Option<UnixStream>,
    pub log_tasks: Vec<JoinHandle<()>>,
}

/// Start instance `index` of `spec` in fork mode. The process gets exactly
/// `spec.env` (plus the instance variable and, for Node apps, the IPC
/// channel), its own process group, `/dev/null` as stdin, and stdout/stderr
/// appended to the spec's log files.
pub fn spawn(spec: &AppSpec, index: u32, out: &Sink, err: &Sink) -> io::Result<Spawned> {
    spawn_with(spec, &spec.args, Some(index), spec.node_ipc, out, err)
}

/// Like [`spawn`], with explicit arguments, an optional instance variable,
/// and whether to open a Node IPC channel (the cluster primary uses this).
pub fn spawn_with(
    spec: &AppSpec,
    args: &[String],
    index: Option<u32>,
    node_ipc: bool,
    out: &Sink,
    err: &Sink,
) -> io::Result<Spawned> {
    let mut cmd = Command::new(&spec.exec);
    cmd.args(args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env);
    if let Some(i) = index {
        cmd.env(&spec.instance_var, i.to_string());
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(false);

    let mut parent_end = None;
    if node_ipc {
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

    let mut child = cmd.spawn()?;
    // Dropping `cmd` closes our copy of the child's end of the IPC socket, so
    // we see EOF once the app exits.
    drop(cmd);

    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("child exited before its pid was read"))?;
    let mut log_tasks = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        log_tasks.push(tokio::spawn(out.clone().pump(stdout)));
    }
    if let Some(stderr) = child.stderr.take() {
        log_tasks.push(tokio::spawn(err.clone().pump(stderr)));
    }
    let ipc = match parent_end {
        Some(s) => {
            s.set_nonblocking(true)?;
            Some(UnixStream::from_std(s)?)
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
    pub fn new(stream: UnixStream) -> Self {
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
}
