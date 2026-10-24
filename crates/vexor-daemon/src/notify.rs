//! Alert delivery (docs/plan.md 6b.4): email over SMTP and signed webhooks.
//!
//! Deliveries run on their own thread with blocking I/O and timeouts, so a
//! slow or unreachable server never holds up supervision or alert
//! evaluation. A failed delivery is retried with backoff (5 s, 30 s, 2 min,
//! 10 min, 30 min), then given up and recorded. Secrets (SMTP password,
//! webhook signing key) are never logged, shown or serialized.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

const TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(1800),
];

/// A value that must never be shown: serialized and debug-printed as
/// `<redacted>`.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("<redacted>")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tls {
    /// Plain connection upgraded with STARTTLS (port 587).
    #[default]
    Starttls,
    /// TLS from the first byte (port 465).
    Implicit,
    /// No encryption: only for a mail server on this host.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Email {
    pub host: String,
    pub port: u16,
    pub tls: Tls,
    pub username: Option<String>,
    pub password: Option<Secret>,
    pub from: String,
    pub to: Vec<String>,
    /// Extra CA certificate (PEM) to trust, e.g. a private mail server's.
    pub ca_file: Option<PathBuf>,
    /// Skip certificate checks. For testing only: anyone on the path can
    /// read the mail and the password.
    pub accept_invalid_certs: bool,
    pub levels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// The alert as JSON, signed when a secret is set.
    #[default]
    Json,
    /// `{"text": ...}`, for Slack (and compatible) incoming webhooks.
    Slack,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Webhook {
    pub url: String,
    pub format: Format,
    pub secret: Option<Secret>,
    pub ca_file: Option<PathBuf>,
    pub levels: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Channels {
    pub email: Option<Email>,
    pub webhooks: Vec<Webhook>,
}

impl Channels {
    pub fn is_empty(&self) -> bool {
        self.email.is_none() && self.webhooks.is_empty()
    }

    /// Settings that weaken security, for warnings.
    pub fn risks(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(e) = &self.email {
            if e.accept_invalid_certs {
                out.push(format!(
                    "email: accept_invalid_certs is on: the connection to {} is not authenticated",
                    e.host
                ));
            }
            if e.tls == Tls::None && e.password.is_some() {
                out.push(format!(
                    "email: the password is sent to {} unencrypted (tls: none)",
                    e.host
                ));
            }
        }
        out
    }
}

// ------------------------------------------------------------ config file

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelsFile {
    email: Option<EmailFile>,
    #[serde(default)]
    webhooks: Vec<WebhookFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmailFile {
    host: String,
    port: Option<u16>,
    #[serde(default)]
    tls: Tls,
    username: Option<String>,
    password: Option<String>,
    password_file: Option<PathBuf>,
    from: String,
    to: Vec<String>,
    ca_file: Option<PathBuf>,
    #[serde(default)]
    accept_invalid_certs: bool,
    #[serde(default)]
    levels: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookFile {
    url: String,
    #[serde(default)]
    format: Format,
    secret: Option<String>,
    secret_file: Option<PathBuf>,
    ca_file: Option<PathBuf>,
    #[serde(default)]
    levels: Vec<String>,
}

/// A secret given inline or in a file (first line, trimmed).
fn secret(
    inline: Option<String>,
    file: Option<&Path>,
    what: &str,
) -> Result<Option<Secret>, String> {
    match (inline, file) {
        (Some(_), Some(_)) => Err(format!("{what}: give the value or a file, not both")),
        (Some(s), None) => Ok(Some(Secret(s))),
        (None, Some(f)) => std::fs::read_to_string(f)
            .map(|t| Some(Secret(t.lines().next().unwrap_or("").trim().to_owned())))
            .map_err(|e| format!("{what}: cannot read {}: {e}", f.display())),
        (None, None) => Ok(None),
    }
}

fn check_levels(levels: &[String], what: &str) -> Result<(), String> {
    for l in levels {
        if !matches!(l.as_str(), "warning" | "danger" | "recovered" | "flapping") {
            return Err(format!(
                "{what}.levels: {l:?} is not warning, danger, recovered or flapping"
            ));
        }
    }
    Ok(())
}

impl ChannelsFile {
    pub fn resolve(self) -> Result<Channels, String> {
        let email = match self.email {
            None => None,
            Some(e) => {
                if e.to.is_empty() {
                    return Err("channels.email.to: at least one address".into());
                }
                for a in std::iter::once(&e.from).chain(&e.to) {
                    if !a.contains('@') || a.contains(['\r', '\n', '<', '>']) {
                        return Err(format!("channels.email: {a:?} is not an email address"));
                    }
                }
                check_levels(&e.levels, "channels.email")?;
                let password = secret(
                    e.password,
                    e.password_file.as_deref(),
                    "channels.email.password",
                )?;
                if password.is_some() && e.username.is_none() {
                    return Err("channels.email: a password needs a username".into());
                }
                Some(Email {
                    port: e.port.unwrap_or(match e.tls {
                        Tls::Implicit => 465,
                        Tls::Starttls => 587,
                        Tls::None => 25,
                    }),
                    host: e.host,
                    tls: e.tls,
                    username: e.username,
                    password,
                    from: e.from,
                    to: e.to,
                    ca_file: e.ca_file,
                    accept_invalid_certs: e.accept_invalid_certs,
                    levels: e.levels,
                })
            }
        };
        let mut webhooks = Vec::new();
        for (i, w) in self.webhooks.into_iter().enumerate() {
            let what = format!("channels.webhooks[{i}]");
            Url::parse(&w.url).map_err(|e| format!("{what}.url: {e}"))?;
            check_levels(&w.levels, &what)?;
            webhooks.push(Webhook {
                secret: secret(
                    w.secret,
                    w.secret_file.as_deref(),
                    &format!("{what}.secret"),
                )?,
                url: w.url,
                format: w.format,
                ca_file: w.ca_file,
                levels: w.levels,
            });
        }
        Ok(Channels { email, webhooks })
    }
}

// ---------------------------------------------------------------- alerts

/// What is delivered.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Alert {
    pub app: String,
    pub instance: Option<u32>,
    pub rule: String,
    /// `warning`, `danger`, `recovered` or `flapping`.
    pub level: String,
    pub value: f64,
    pub threshold: Option<f64>,
    pub value_text: String,
    pub ts_ms: u64,
    pub host: String,
}

impl Alert {
    pub fn summary(&self) -> String {
        let who = match self.instance {
            Some(i) => format!("{} #{i}", self.app),
            None => self.app.clone(),
        };
        format!(
            "{}: {who} {} ({}) on {}",
            self.level.to_uppercase(),
            self.rule,
            self.value_text,
            self.host
        )
    }
}

/// This machine's name, for alert texts.
pub fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

// -------------------------------------------------------------- delivery

/// Delivery status of each channel, for warnings in `vexor alerts`.
pub type Status = Arc<Mutex<BTreeMap<String, Result<u64, String>>>>;

struct Job {
    channel: Channel,
    alert: Alert,
    attempt: usize,
    due: Instant,
}

#[derive(Clone)]
enum Channel {
    Email(Arc<Email>),
    Webhook(Arc<Webhook>),
}

impl Channel {
    fn name(&self) -> String {
        match self {
            Channel::Email(e) => format!("email to {}", e.to.join(", ")),
            Channel::Webhook(w) => format!(
                "webhook {}",
                Url::parse(&w.url).map_or_else(|_| w.url.clone(), |u| u.host)
            ),
        }
    }

    fn levels(&self) -> &[String] {
        match self {
            Channel::Email(e) => &e.levels,
            Channel::Webhook(w) => &w.levels,
        }
    }

    fn deliver(&self, alert: &Alert) -> Result<(), String> {
        match self {
            Channel::Email(e) => send_email(e, alert),
            Channel::Webhook(w) => send_webhook(w, alert),
        }
    }
}

/// Handle to the delivery thread. Cheap to clone.
#[derive(Clone)]
pub struct Notifier {
    tx: SyncSender<Job>,
    pub status: Status,
}

/// What the delivery thread reports back (failures given up on).
pub type OnGiveUp = Box<dyn Fn(&str, &Alert, &str) + Send>;

impl Notifier {
    pub fn start(on_give_up: OnGiveUp) -> std::io::Result<Notifier> {
        let (tx, rx) = sync_channel(1000);
        let status: Status = Arc::default();
        let st = status.clone();
        std::thread::Builder::new()
            .name("vexor-notify".into())
            .spawn(move || deliver_loop(&rx, &st, &on_give_up))?;
        Ok(Notifier { tx, status })
    }

    /// Queue `alert` for every channel that wants its level.
    pub fn notify(&self, channels: &Channels, alert: &Alert) {
        let all = channels
            .email
            .iter()
            .map(|e| Channel::Email(Arc::new(e.clone())))
            .chain(
                channels
                    .webhooks
                    .iter()
                    .map(|w| Channel::Webhook(Arc::new(w.clone()))),
            );
        for channel in all {
            let levels = channel.levels();
            if !levels.is_empty() && !levels.contains(&alert.level) {
                continue;
            }
            let _ = self.tx.try_send(Job {
                channel,
                alert: alert.clone(),
                attempt: 0,
                due: Instant::now(),
            });
        }
    }
}

/// Deliver `alert` to every channel now, whatever its levels; for
/// `vexor alerts test`. Returns each channel's outcome.
pub fn deliver_now(channels: &Channels, alert: &Alert) -> Vec<(String, Result<(), String>)> {
    channels
        .email
        .iter()
        .map(|e| Channel::Email(Arc::new(e.clone())))
        .chain(
            channels
                .webhooks
                .iter()
                .map(|w| Channel::Webhook(Arc::new(w.clone()))),
        )
        .map(|c| (c.name(), c.deliver(alert)))
        .collect()
}

fn deliver_loop(rx: &Receiver<Job>, status: &Status, on_give_up: &OnGiveUp) {
    let mut queue: Vec<Job> = Vec::new();
    loop {
        let next_due = queue.iter().map(|j| j.due).min();
        let wait = next_due.map(|d| d.saturating_duration_since(Instant::now()));
        let received = match wait {
            Some(w) => rx.recv_timeout(w),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(job) => queue.push(job),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) if queue.is_empty() => return,
            Err(RecvTimeoutError::Disconnected) => {}
        }
        let now = Instant::now();
        let (due, later): (Vec<Job>, Vec<Job>) = queue.drain(..).partition(|j| j.due <= now);
        queue = later;
        for mut job in due {
            let name = job.channel.name();
            let result = job.channel.deliver(&job.alert);
            let mut st = status.lock().unwrap_or_else(PoisonError::into_inner);
            match result {
                Ok(()) => {
                    st.insert(name, Ok(job.alert.ts_ms));
                }
                Err(e) => {
                    st.insert(name.clone(), Err(e.clone()));
                    drop(st);
                    match BACKOFF.get(job.attempt) {
                        Some(wait) => {
                            job.attempt += 1;
                            job.due = Instant::now() + *wait;
                            queue.push(job);
                        }
                        None => on_give_up(&name, &job.alert, &e),
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------- TLS

fn roots() -> &'static rustls::RootCertStore {
    static ROOTS: OnceLock<rustls::RootCertStore> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut store = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = store.add(cert);
        }
        store
    })
}

fn tls_config(
    ca_file: Option<&Path>,
    accept_invalid: bool,
) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = if accept_invalid {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
            .with_no_client_auth()
    } else {
        let mut store = roots().clone();
        if let Some(f) = ca_file {
            let certs = CertificateDer::pem_file_iter(f)
                .map_err(|e| format!("cannot read {}: {e}", f.display()))?;
            for c in certs {
                let c = c.map_err(|e| format!("{}: {e}", f.display()))?;
                store.add(c).map_err(|e| format!("{}: {e}", f.display()))?;
            }
        }
        if store.is_empty() {
            return Err("no trusted CA certificates found on this system (set ca_file)".into());
        }
        builder.with_root_certificates(store).with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// `accept_invalid_certs`: checks signatures, trusts any certificate.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A plain or TLS connection.
enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Conn {
    fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
        use std::net::ToSocketAddrs;
        let addrs = (host, port)
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {host}: {e}"))?;
        let mut last = format!("no address for {host}");
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, TIMEOUT) {
                Ok(s) => {
                    s.set_read_timeout(Some(TIMEOUT))
                        .map_err(|e| e.to_string())?;
                    s.set_write_timeout(Some(TIMEOUT))
                        .map_err(|e| e.to_string())?;
                    return Ok(s);
                }
                Err(e) => last = format!("cannot connect to {host}:{port}: {e}"),
            }
        }
        Err(last)
    }

    fn tls(
        stream: TcpStream,
        host: &str,
        config: Arc<rustls::ClientConfig>,
    ) -> Result<Conn, String> {
        let name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|e| format!("{host}: {e}"))?;
        let conn = rustls::ClientConnection::new(config, name).map_err(|e| e.to_string())?;
        let mut tls = rustls::StreamOwned::new(conn, stream);
        // Handshake now, so a certificate problem is reported as such.
        while tls.conn.is_handshaking() {
            tls.conn
                .complete_io(&mut tls.sock)
                .map_err(|e| format!("TLS with {host}: {e}"))?;
        }
        Ok(Conn::Tls(Box::new(tls)))
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

// ------------------------------------------------------------------ SMTP

struct Smtp {
    r: BufReader<Conn>,
}

impl Smtp {
    /// Read one (possibly multi-line) reply: its code and text lines.
    fn reply(&mut self) -> Result<(u16, Vec<String>), String> {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = self
                .r
                .by_ref()
                .take(4096)
                .read_line(&mut line)
                .map_err(|e| format!("SMTP read: {e}"))?;
            if n == 0 {
                return Err("SMTP server closed the connection".into());
            }
            let line = line.trim_end();
            let code: u16 = line
                .get(..3)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| format!("SMTP: unexpected reply {line:?}"))?;
            lines.push(line.get(4..).unwrap_or("").to_owned());
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, lines));
            }
        }
    }

    /// Send a command and expect a reply code in `ok`. `shown` replaces
    /// the command in error messages (it may hold credentials).
    fn cmd(&mut self, line: &str, ok: &[u16], shown: &str) -> Result<Vec<String>, String> {
        let w = self.r.get_mut();
        w.write_all(line.as_bytes())
            .and_then(|()| w.write_all(b"\r\n"))
            .and_then(|()| w.flush())
            .map_err(|e| format!("SMTP write: {e}"))?;
        let (code, text) = self.reply()?;
        if ok.contains(&code) {
            Ok(text)
        } else {
            Err(format!("SMTP {shown}: {code} {}", text.join(" ")))
        }
    }

    fn expect(&mut self, ok: &[u16], what: &str) -> Result<Vec<String>, String> {
        let (code, text) = self.reply()?;
        if ok.contains(&code) {
            Ok(text)
        } else {
            Err(format!("SMTP {what}: {code} {}", text.join(" ")))
        }
    }
}

pub fn send_email(e: &Email, alert: &Alert) -> Result<(), String> {
    let stream = Conn::connect(&e.host, e.port)?;
    let config = || tls_config(e.ca_file.as_deref(), e.accept_invalid_certs);
    let conn = match e.tls {
        Tls::Implicit => Conn::tls(stream, &e.host, config()?)?,
        _ => Conn::Plain(stream),
    };
    let mut s = Smtp {
        r: BufReader::new(conn),
    };
    s.expect(&[220], "greeting")?;
    let me = hostname();
    let mut caps = s.cmd(&format!("EHLO {me}"), &[250], "EHLO")?;
    if e.tls == Tls::Starttls {
        if !caps.iter().any(|c| c.eq_ignore_ascii_case("STARTTLS")) {
            return Err(format!("{} does not offer STARTTLS", e.host));
        }
        s.cmd("STARTTLS", &[220], "STARTTLS")?;
        let Conn::Plain(plain) = s.r.into_inner() else {
            return Err("STARTTLS on a TLS connection".into());
        };
        s = Smtp {
            r: BufReader::new(Conn::tls(plain, &e.host, config()?)?),
        };
        caps = s.cmd(&format!("EHLO {me}"), &[250], "EHLO")?;
    }
    if let (Some(user), Some(pass)) = (&e.username, &e.password) {
        let auth = caps
            .iter()
            .find(|c| c.to_ascii_uppercase().starts_with("AUTH"))
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or_default();
        if auth.contains("PLAIN") || !auth.contains("LOGIN") {
            let token = base64(format!("\0{user}\0{}", pass.0).as_bytes());
            s.cmd(&format!("AUTH PLAIN {token}"), &[235], "AUTH PLAIN")?;
        } else {
            s.cmd("AUTH LOGIN", &[334], "AUTH LOGIN")?;
            s.cmd(&base64(user.as_bytes()), &[334], "AUTH LOGIN user")?;
            s.cmd(&base64(pass.0.as_bytes()), &[235], "AUTH LOGIN password")?;
        }
    }
    s.cmd(&format!("MAIL FROM:<{}>", e.from), &[250], "MAIL FROM")?;
    for to in &e.to {
        s.cmd(
            &format!("RCPT TO:<{to}>"),
            &[250, 251],
            &format!("RCPT TO {to}"),
        )?;
    }
    s.cmd("DATA", &[354], "DATA")?;
    let message = email_message(e, alert);
    s.cmd(&format!("{}\r\n.", dot_stuff(&message)), &[250], "message")?;
    let _ = s.cmd("QUIT", &[221], "QUIT");
    Ok(())
}

fn email_message(e: &Email, a: &Alert) -> String {
    let body = format!(
        "{}\r\n\r\nApp:       {}\r\nInstance:  {}\r\nRule:      {}\r\nLevel:     {}\r\nValue:     {}\r\nThreshold: {}\r\nHost:      {}\r\n\r\n`vexor alerts` on {} shows open incidents.\r\n",
        a.summary(),
        a.app,
        a.instance.map_or_else(|| "-".into(), |i| i.to_string()),
        a.rule,
        a.level,
        a.value_text,
        a.threshold.map_or_else(|| "-".into(), |t| t.to_string()),
        a.host,
        a.host,
    );
    format!(
        "From: {}\r\nTo: {}\r\nSubject: [vexor] {}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\nX-Vexor-Rule: {}\r\n\r\n{body}",
        e.from,
        e.to.join(", "),
        a.summary(),
        a.rule,
    )
}

/// Lines starting with "." get another (RFC 5321 4.5.2); bare LF become CRLF.
fn dot_stuff(message: &str) -> String {
    message
        .split("\r\n")
        .flat_map(|l| l.split('\n'))
        .map(|l| {
            if l.starts_with('.') {
                format!(".{l}")
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// --------------------------------------------------------------- webhook

#[derive(Debug, PartialEq)]
struct Url {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

impl Url {
    fn parse(url: &str) -> Result<Url, String> {
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(format!("{url:?} is not an http:// or https:// URL"));
        };
        let (authority, path) = rest
            .find('/')
            .map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
        if authority.is_empty() || authority.contains('@') {
            return Err(format!(
                "{url:?}: missing host (credentials in URLs are not supported)"
            ));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => {
                (h, p.parse().map_err(|_| format!("{url:?}: invalid port"))?)
            }
            _ => (authority, if https { 443 } else { 80 }),
        };
        Ok(Url {
            https,
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            port,
            path: path.to_owned(),
        })
    }
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn webhook_body(w: &Webhook, a: &Alert) -> String {
    match w.format {
        Format::Json => json!({ "type": "alert", "alert": a, "text": a.summary() }).to_string(),
        Format::Slack => json!({ "text": format!("[vexor] {}", a.summary()) }).to_string(),
    }
}

pub fn send_webhook(w: &Webhook, alert: &Alert) -> Result<(), String> {
    let url = Url::parse(&w.url)?;
    let body = webhook_body(w, alert);
    let stream = Conn::connect(&url.host, url.port)?;
    let mut conn = if url.https {
        Conn::tls(stream, &url.host, tls_config(w.ca_file.as_deref(), false)?)?
    } else {
        Conn::Plain(stream)
    };
    let mut head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: vexor/{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nX-Vexor-Event: alert\r\n",
        url.path,
        url.host,
        env!("CARGO_PKG_VERSION"),
        body.len()
    );
    if let Some(s) = &w.secret {
        // Signed over "<timestamp>.<body>", so a captured request can't be
        // replayed later with a new timestamp.
        let ts = alert.ts_ms / 1000;
        let sig = hmac_sha256(s.0.as_bytes(), format!("{ts}.{body}").as_bytes());
        head.push_str(&format!(
            "X-Vexor-Timestamp: {ts}\r\nX-Vexor-Signature: sha256={}\r\n",
            hex(&sig)
        ));
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes())
        .and_then(|()| conn.write_all(body.as_bytes()))
        .and_then(|()| conn.flush())
        .map_err(|e| format!("webhook {}: {e}", url.host))?;
    let mut status = String::new();
    BufReader::new(conn)
        .take(1024)
        .read_line(&mut status)
        .map_err(|e| format!("webhook {}: {e}", url.host))?;
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| {
            format!(
                "webhook {}: unexpected response {:?}",
                url.host,
                status.trim()
            )
        })?;
    if (200..300).contains(&code) {
        Ok(())
    } else {
        Err(format!("webhook {}: HTTP {code}", url.host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn alert() -> Alert {
        Alert {
            app: "web".into(),
            instance: Some(0),
            rule: "crash_loop".into(),
            level: "danger".into(),
            value: 1.0,
            threshold: Some(1.0),
            value_text: "yes".into(),
            ts_ms: 1_700_000_000_000,
            host: "box".into(),
        }
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn hmac_matches_rfc4231() {
        // Test cases 1 and 2.
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block.
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn urls_parse() {
        let u = Url::parse("https://hooks.example.com/services/T/B?x=1").expect("url");
        assert_eq!(
            u,
            Url {
                https: true,
                host: "hooks.example.com".into(),
                port: 443,
                path: "/services/T/B?x=1".into()
            }
        );
        assert_eq!(Url::parse("http://127.0.0.1:8080").expect("url").port, 8080);
        assert_eq!(Url::parse("http://[::1]:9000/h").expect("url").host, "::1");
        for bad in [
            "ftp://x",
            "https://",
            "https://user:pw@host/",
            "http://h:port/",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn dot_stuffing_and_line_endings() {
        assert_eq!(dot_stuff("a\n.b\r\n..c"), "a\r\n..b\r\n...c");
    }

    #[test]
    fn secrets_never_show() {
        let s = Secret("hunter2".into());
        assert_eq!(format!("{s:?}"), "<redacted>");
        assert_eq!(serde_json::to_string(&s).expect("json"), "\"<redacted>\"");
    }

    #[test]
    fn channel_config_validation() {
        let parse = |v: serde_json::Value| -> Result<Channels, String> {
            serde_json::from_value::<ChannelsFile>(v)
                .map_err(|e| e.to_string())?
                .resolve()
        };
        let c = parse(json!({
            "email": { "host": "mail.example.com", "username": "u", "password": "p",
                       "from": "vexor@example.com", "to": ["ops@example.com"], "levels": ["danger"] },
            "webhooks": [{ "url": "https://example.com/h", "format": "slack" }]
        }))
        .expect("valid");
        let e = c.email.expect("email");
        assert_eq!((e.port, e.tls), (587, Tls::Starttls));
        assert_eq!(c.webhooks[0].format, Format::Slack);
        for bad in [
            json!({ "email": { "host": "h", "from": "a@b", "to": [] } }),
            json!({ "email": { "host": "h", "from": "nope", "to": ["a@b"] } }),
            json!({ "email": { "host": "h", "from": "a@b", "to": ["x@y>\r\nRCPT TO:<z@w"] } }),
            json!({ "email": { "host": "h", "from": "a@b", "to": ["a@b"], "password": "p" } }),
            json!({ "webhooks": [{ "url": "ftp://x" }] }),
            json!({ "webhooks": [{ "url": "http://x", "levels": ["info"] }] }),
            json!({ "webhooks": [{ "url": "http://x", "secret": "a", "secret_file": "/f" }] }),
            json!({ "sms": {} }),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad}");
        }
    }

    /// A scripted SMTP server: answers each command, records what it got.
    fn fake_smtp(auth: &'static str) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().expect("accept");
            let mut r = BufReader::new(s.try_clone().expect("clone"));
            let mut w = s;
            let mut got = Vec::new();
            w.write_all(b"220 fake ESMTP\r\n").expect("greet");
            let mut data = false;
            // AUTH LOGIN: username, then password, each after a 334.
            let mut login = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).expect("read") == 0 {
                    break;
                }
                let line = line.trim_end().to_owned();
                if data {
                    got.push(line.clone());
                    if line == "." {
                        data = false;
                        w.write_all(b"250 queued\r\n").expect("w");
                    }
                    continue;
                }
                got.push(line.clone());
                let reply = match line.split(' ').next().unwrap_or("") {
                    "EHLO" => format!("250-fake\r\n250-AUTH {auth}\r\n250 8BITMIME\r\n"),
                    "AUTH" if line == "AUTH LOGIN" => {
                        login = 1;
                        "334 VXNlcm5hbWU6\r\n".into()
                    }
                    "AUTH" => "235 ok\r\n".into(),
                    _ if login == 1 => {
                        login = 2;
                        "334 UGFzc3dvcmQ6\r\n".into()
                    }
                    _ if login == 2 => {
                        login = 0;
                        "235 ok\r\n".into()
                    }
                    "MAIL" | "RCPT" => "250 ok\r\n".into(),
                    "DATA" => {
                        data = true;
                        "354 go\r\n".into()
                    }
                    "QUIT" => {
                        w.write_all(b"221 bye\r\n").expect("w");
                        break;
                    }
                    _ => "334 \r\n".into(),
                };
                w.write_all(reply.as_bytes()).expect("w");
            }
            got
        });
        (port, t)
    }

    fn email(port: u16) -> Email {
        Email {
            host: "127.0.0.1".into(),
            port,
            tls: Tls::None,
            username: Some("vexor".into()),
            password: Some(Secret("s3cret".into())),
            from: "vexor@example.com".into(),
            to: vec!["ops@example.com".into(), "dev@example.com".into()],
            ca_file: None,
            accept_invalid_certs: false,
            levels: Vec::new(),
        }
    }

    #[test]
    fn smtp_conversation_with_auth_plain() {
        let (port, server) = fake_smtp("PLAIN LOGIN");
        send_email(&email(port), &alert()).expect("sent");
        let got = server.join().expect("server");
        assert!(got[0].starts_with("EHLO "));
        assert_eq!(got[1], format!("AUTH PLAIN {}", base64(b"\0vexor\0s3cret")));
        assert_eq!(got[2], "MAIL FROM:<vexor@example.com>");
        assert_eq!(
            &got[3..5],
            ["RCPT TO:<ops@example.com>", "RCPT TO:<dev@example.com>"]
        );
        assert!(
            got.iter()
                .any(|l| l == "Subject: [vexor] DANGER: web #0 crash_loop (yes) on box")
        );
        assert_eq!(got.last().map(String::as_str), Some("QUIT"));
    }

    #[test]
    fn smtp_falls_back_to_auth_login() {
        let (port, server) = fake_smtp("LOGIN");
        send_email(&email(port), &alert()).expect("sent");
        let got = server.join().expect("server");
        assert_eq!(
            &got[1..4],
            ["AUTH LOGIN".to_owned(), base64(b"vexor"), base64(b"s3cret")]
        );
    }

    #[test]
    fn starttls_is_required_when_configured() {
        let (port, _server) = fake_smtp("PLAIN");
        let e = Email {
            tls: Tls::Starttls,
            ..email(port)
        };
        let err = send_email(&e, &alert()).expect_err("no STARTTLS offered");
        assert!(err.contains("does not offer STARTTLS"), "{err}");
        assert!(!err.contains("s3cret"));
    }

    #[test]
    fn webhook_is_signed_and_checks_the_status() {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for status in ["204 No Content", "500 Oops"] {
                let (mut s, _) = l.accept().expect("accept");
                let mut buf = vec![0u8; 8192];
                let mut got = Vec::new();
                // Read until the body (Content-Length) is complete.
                loop {
                    let n = s.read(&mut buf).expect("read");
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                        if body.len() >= len {
                            break;
                        }
                    }
                }
                write!(s, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").expect("w");
                requests.push(String::from_utf8_lossy(&got).to_string());
            }
            requests
        });
        let w = Webhook {
            url: format!("http://127.0.0.1:{port}/hook"),
            format: Format::Json,
            secret: Some(Secret("key".into())),
            ca_file: None,
            levels: Vec::new(),
        };
        send_webhook(&w, &alert()).expect("2xx");
        let err = send_webhook(&w, &alert()).expect_err("500");
        assert!(err.contains("HTTP 500"), "{err}");
        let req = &server.join().expect("server")[0];
        let (head, body) = req.split_once("\r\n\r\n").expect("request");
        assert!(head.starts_with("POST /hook HTTP/1.1\r\n"), "{head}");
        let ts = head
            .lines()
            .find_map(|l| l.strip_prefix("X-Vexor-Timestamp: "))
            .expect("timestamp");
        let sig = head
            .lines()
            .find_map(|l| l.strip_prefix("X-Vexor-Signature: sha256="))
            .expect("signature");
        assert_eq!(
            sig,
            hex(&hmac_sha256(b"key", format!("{ts}.{body}").as_bytes()))
        );
        let v: serde_json::Value = serde_json::from_str(body).expect("json body");
        assert_eq!(v["alert"]["rule"], "crash_loop");
    }

    #[test]
    fn failed_deliveries_are_retried_then_given_up() {
        let given_up = Arc::new(Mutex::new(Vec::new()));
        let g = given_up.clone();
        let n = Notifier::start(Box::new(move |name, a, e| {
            g.lock()
                .expect("lock")
                .push((name.to_owned(), a.rule.clone(), e.to_owned()));
        }))
        .expect("start");
        // Nothing listens on port 1: fails at once.
        let channels = Channels {
            email: None,
            webhooks: vec![Webhook {
                url: "http://127.0.0.1:1/".into(),
                format: Format::Slack,
                secret: None,
                ca_file: None,
                levels: vec!["danger".into()],
            }],
        };
        n.notify(
            &channels,
            &Alert {
                level: "warning".into(),
                ..alert()
            },
        );
        n.notify(&channels, &alert());
        for _ in 0..200 {
            if n.status.lock().expect("lock").values().any(Result::is_err) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = n.status.lock().expect("lock").clone();
        assert_eq!(status.len(), 1, "warning is filtered out: {status:?}");
        assert!(
            status
                .values()
                .all(|s| s.as_ref().is_err_and(|e| e.contains("127.0.0.1")))
        );
        assert!(
            given_up.lock().expect("lock").is_empty(),
            "retried later, not given up yet"
        );
    }
}
