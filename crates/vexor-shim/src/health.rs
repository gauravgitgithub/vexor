//! HTTP and TCP health checks (docs/compatibility.md). Only loopback
//! targets are allowed (`HealthCheckSpec::target`), so plain HTTP/1.1 over
//! a local socket is all that's needed.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use vexor_core::HealthTarget;

/// Largest response head read while looking for the status line.
const MAX_HEAD: usize = 8 * 1024;

/// Run one check. `Err` says why it failed.
pub async fn probe(target: &HealthTarget, timeout: Duration) -> Result<(), String> {
    match tokio::time::timeout(timeout, check(target)).await {
        Ok(r) => r,
        Err(_) => Err(format!("no answer within {} ms", timeout.as_millis())),
    }
}

async fn check(t: &HealthTarget) -> Result<(), String> {
    let mut stream = TcpStream::connect((t.host.as_str(), t.port))
        .await
        .map_err(|e| format!("connect to {}:{}: {e}", t.host, t.port))?;
    let Some(path) = &t.path else {
        return Ok(());
    };
    let host = if t.host.contains(':') {
        format!("[{}]:{}", t.host, t.port)
    } else {
        format!("{}:{}", t.host, t.port)
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: vexor-health\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("send request: {e}"))?;
    let mut head = Vec::with_capacity(256);
    let mut buf = [0u8; 1024];
    while !head.contains(&b'\n') && head.len() < MAX_HEAD {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| format!("read response: {e}"))?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = String::from_utf8_lossy(&head);
    let line = line.lines().next().unwrap_or("");
    let status: u16 = line
        .strip_prefix("HTTP/1.")
        .and_then(|r| r.get(2..5))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            format!(
                "not an HTTP response: {:?}",
                line.chars().take(80).collect::<String>()
            )
        })?;
    if (200..400).contains(&status) {
        Ok(())
    } else {
        Err(format!("HTTP {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn server(response: &'static str) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = l.local_addr().expect("addr").port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(response.as_bytes()).await;
            }
        });
        port
    }

    fn http(port: u16) -> HealthTarget {
        HealthTarget {
            host: "127.0.0.1".into(),
            port,
            path: Some("/health".into()),
        }
    }

    #[tokio::test]
    async fn statuses() {
        let t = Duration::from_secs(2);
        let ok = server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
        assert!(probe(&http(ok), t).await.is_ok());
        let redirect = server("HTTP/1.0 302 Found\r\n\r\n").await;
        assert!(probe(&http(redirect), t).await.is_ok());
        let bad = server("HTTP/1.1 503 Service Unavailable\r\n\r\n").await;
        assert_eq!(probe(&http(bad), t).await, Err("HTTP 503".into()));
        let junk = server("hello\r\n").await;
        assert!(
            probe(&http(junk), t)
                .await
                .expect_err("fails")
                .contains("not an HTTP")
        );
    }

    #[tokio::test]
    async fn tcp_and_failures() {
        let t = Duration::from_millis(500);
        let port = server("").await;
        let tcp = HealthTarget {
            path: None,
            ..http(port)
        };
        assert!(probe(&tcp, t).await.is_ok());
        // Nothing listening. Not a freed ephemeral port: tests running in
        // parallel bind port 0 and could be handed that one.
        let closed = 1;
        assert!(
            probe(&http(closed), t)
                .await
                .expect_err("fails")
                .contains("connect")
        );
        // Accepts but never answers.
        let silent = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let sp = silent.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = silent.accept().await {
                held.push(s);
            }
        });
        assert!(
            probe(&http(sp), t)
                .await
                .expect_err("fails")
                .contains("no answer")
        );
    }
}
