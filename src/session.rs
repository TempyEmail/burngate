use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::lookup::MailboxLookup;
use crate::relay;
use crate::tls::TlsConfig;

/// Global counters for monitoring.
pub struct Metrics {
    pub accepted: AtomicU64,
    pub rejected: AtomicU64,
    pub connections: AtomicU64,
    pub relay_errors: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            accepted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            connections: AtomicU64::new(0),
            relay_errors: AtomicU64::new(0),
        }
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Shared SMTP session state (preserved across TLS upgrade).
struct SessionState {
    sender: Option<String>,
    recipients: HashSet<String>,
    ehlo_received: bool,
    /// Running count of RCPT TO commands in this session (not reset per transaction).
    recipient_count: usize,
}

impl SessionState {
    fn new() -> Self {
        Self {
            sender: None,
            recipients: HashSet::new(),
            ehlo_received: false,
            recipient_count: 0,
        }
    }

    fn reset_transaction(&mut self) {
        self.sender = None;
        self.recipients.clear();
    }
}

/// Handle a single SMTP session.
pub async fn handle_session(
    stream: tokio::net::TcpStream,
    peer_addr: std::net::SocketAddr,
    config: Arc<Config>,
    lookup: MailboxLookup,
    tls_config: Option<TlsConfig>,
    metrics: Arc<Metrics>,
) {
    metrics.connections.fetch_add(1, Ordering::Relaxed);
    info!(peer = %peer_addr, "new connection");

    let timeout = tokio::time::Duration::from_secs(config.connection_timeout_secs);

    let result = tokio::time::timeout(timeout, async {
        run_session(
            stream,
            peer_addr,
            config,
            lookup,
            tls_config,
            metrics.clone(),
        )
        .await
    })
    .await;

    match result {
        Ok(Ok(())) => debug!(peer = %peer_addr, "session completed"),
        Ok(Err(e)) => debug!(peer = %peer_addr, error = %e, "session error"),
        Err(_) => debug!(peer = %peer_addr, "session timed out"),
    }
}

async fn run_session(
    stream: tokio::net::TcpStream,
    peer_addr: std::net::SocketAddr,
    config: Arc<Config>,
    lookup: MailboxLookup,
    tls_config: Option<TlsConfig>,
    metrics: Arc<Metrics>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut reader = BufReader::new(stream);
    let mut state = SessionState::new();

    // Send banner
    send_line(
        reader.get_mut(),
        &format!("220 {} ESMTP burngate", config.server_name),
    )
    .await?;

    // Run SMTP loop on plain connection
    let result = smtp_loop(
        &mut reader,
        &mut state,
        peer_addr,
        &config,
        &lookup,
        &tls_config,
        &metrics,
        false,
    )
    .await;

    match result {
        LoopResult::Done(r) => r,
        LoopResult::StartTls => {
            let tls_cfg = tls_config.as_ref().unwrap();

            // Recover the raw TcpStream for TLS handshake
            let tcp_stream = reader.into_inner();
            let tls_stream = tls_cfg.accept(tcp_stream).await?;
            info!(peer = %peer_addr, "STARTTLS handshake completed");

            // Reset EHLO state per RFC 3207 — client must re-EHLO after STARTTLS
            state.ehlo_received = false;
            state.reset_transaction();

            let mut tls_reader = BufReader::new(tls_stream);

            // Continue SMTP on the TLS connection
            let result = smtp_loop(
                &mut tls_reader,
                &mut state,
                peer_addr,
                &config,
                &lookup,
                &tls_config,
                &metrics,
                true,
            )
            .await;

            match result {
                LoopResult::Done(r) => r,
                LoopResult::StartTls => {
                    // Already on TLS, shouldn't happen
                    Err("STARTTLS requested on already-TLS connection".into())
                }
            }
        }
    }
}

enum LoopResult {
    Done(Result<(), Box<dyn std::error::Error + Send + Sync>>),
    StartTls,
}

/// Write an SMTP response line.
async fn send_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    line: &str,
) -> Result<(), std::io::Error> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await?;
    Ok(())
}

/// Read a single line from the SMTP client with a hard byte limit.
///
/// Returns an error if the line exceeds `max_len` bytes before a newline is
/// found. This prevents memory exhaustion from newline-less input.
async fn read_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
    max_len: usize,
) -> Result<Option<String>, std::io::Error> {
    buf.clear();
    loop {
        let byte = match reader.read_u8().await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                if buf.is_empty() {
                    return Ok(None);
                }
                // Return what we have
                let s = String::from_utf8_lossy(buf).trim_end().to_string();
                return Ok(Some(s));
            }
            Err(e) => return Err(e),
        };
        if byte == b'\n' {
            let s = String::from_utf8_lossy(buf).trim_end().to_string();
            return Ok(Some(s));
        }
        buf.push(byte);
        if buf.len() > max_len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds maximum length",
            ));
        }
    }
}

/// Read the DATA portion of an SMTP message until a lone ".".
///
/// Passes raw wire format through to the backend — no dot-unstuffing.
/// The backend (or MDA) is responsible for dot-unstuffing per RFC 5321 §4.5.2.
async fn read_data<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max_size: usize,
) -> Result<Vec<u8>, std::io::Error> {
    let mut data = Vec::with_capacity(8192);
    let mut line_buf = Vec::with_capacity(1024);

    loop {
        line_buf.clear();
        let n = reader.read_until(b'\n', &mut line_buf).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed during DATA",
            ));
        }

        // Check for lone "." terminator (with optional \r before \n)
        let trimmed = if line_buf.ends_with(b"\r\n") {
            &line_buf[..line_buf.len() - 2]
        } else if line_buf.ends_with(b"\n") {
            &line_buf[..line_buf.len() - 1]
        } else {
            &line_buf[..]
        };
        if trimmed == b"." {
            break;
        }

        // Relay raw wire format — no dot-unstuffing
        data.extend_from_slice(&line_buf);

        if data.len() > max_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "message exceeds maximum size",
            ));
        }
    }

    Ok(data)
}

/// Main SMTP command loop, generic over the stream type.
///
/// Uses `BufReader<S>` where S implements both AsyncRead and AsyncWrite.
/// Writes go through `reader.get_mut()` since SMTP is half-duplex.
#[allow(clippy::too_many_arguments)]
async fn smtp_loop<S: tokio::io::AsyncRead + AsyncWrite + Unpin>(
    reader: &mut BufReader<S>,
    state: &mut SessionState,
    peer_addr: std::net::SocketAddr,
    config: &Config,
    lookup: &MailboxLookup,
    tls_config: &Option<TlsConfig>,
    metrics: &Metrics,
    tls_active: bool,
) -> LoopResult {
    let mut line_buf = Vec::with_capacity(1024);

    loop {
        let line = match read_line(reader, &mut line_buf, config.max_line_length).await {
            Ok(Some(line)) => line,
            Ok(None) => return LoopResult::Done(Ok(())),
            Err(e) => {
                debug!(peer = %peer_addr, error = %e, "read error");
                return LoopResult::Done(Ok(()));
            }
        };

        let (command, args) = parse_command(&line);

        match command.as_str() {
            "EHLO" | "HELO" => {
                state.ehlo_received = true;
                let mut caps = vec![
                    format!("250-{} Hello {}", config.server_name, args),
                    "250-SIZE 10485760".to_string(),
                    "250-8BITMIME".to_string(),
                    "250-PIPELINING".to_string(),
                    "250-ENHANCEDSTATUSCODES".to_string(),
                ];
                if tls_config.is_some() && !tls_active {
                    caps.push("250-STARTTLS".to_string());
                }
                if let Some(last) = caps.last_mut() {
                    *last = last.replacen("250-", "250 ", 1);
                }
                for cap in &caps {
                    if let Err(e) = send_line(reader.get_mut(), cap).await {
                        return LoopResult::Done(Err(e.into()));
                    }
                }
            }

            "STARTTLS" => {
                if tls_active {
                    if let Err(e) =
                        send_line(reader.get_mut(), "554 5.5.1 TLS already active").await
                    {
                        return LoopResult::Done(Err(e.into()));
                    }
                } else if tls_config.is_some() {
                    if let Err(e) =
                        send_line(reader.get_mut(), "220 2.0.0 Ready to start TLS").await
                    {
                        return LoopResult::Done(Err(e.into()));
                    }
                    return LoopResult::StartTls;
                } else if let Err(e) =
                    send_line(reader.get_mut(), "502 5.5.1 STARTTLS not available").await
                {
                    return LoopResult::Done(Err(e.into()));
                }
            }

            "MAIL" => {
                state.sender = extract_address(args);
                state.recipients.clear();
                if let Err(e) = send_line(reader.get_mut(), "250 2.1.0 OK").await {
                    return LoopResult::Done(Err(e.into()));
                }
            }

            "RCPT" => {
                let address = match extract_address(args) {
                    Some(addr) => addr,
                    None => {
                        if let Err(e) =
                            send_line(reader.get_mut(), "501 5.1.3 Bad recipient address syntax")
                                .await
                        {
                            return LoopResult::Done(Err(e.into()));
                        }
                        continue;
                    }
                };

                // Enforce per-session RCPT TO limit
                state.recipient_count += 1;
                if state.recipient_count > config.max_recipients {
                    warn!(
                        peer = %peer_addr,
                        count = state.recipient_count,
                        max = config.max_recipients,
                        "RCPT TO limit exceeded"
                    );
                    if let Err(e) =
                        send_line(reader.get_mut(), "452 4.5.3 Too many recipients").await
                    {
                        return LoopResult::Done(Err(e.into()));
                    }
                    continue;
                }

                let address_lower = address.to_lowercase();
                let domain = address_lower.rsplit('@').next().unwrap_or("");

                if !is_domain_accepted(domain, &config.accepted_domains) {
                    info!(
                        peer = %peer_addr,
                        address = %address_lower,
                        domain = domain,
                        "[MAIL-REJECTED] unknown domain"
                    );
                    metrics.rejected.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = send_line(reader.get_mut(), "550 5.1.2 Unknown domain").await {
                        return LoopResult::Done(Err(e.into()));
                    }
                    continue;
                }

                // Check Redis for mailbox existence — the key spam-filtering step
                if !lookup.should_accept(&address_lower).await {
                    info!(
                        peer = %peer_addr,
                        address = %address_lower,
                        "[MAIL-REJECTED] mailbox not found"
                    );
                    metrics.rejected.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = send_line(reader.get_mut(), "550 5.1.1 User unknown").await {
                        return LoopResult::Done(Err(e.into()));
                    }
                    continue;
                }

                info!(
                    peer = %peer_addr,
                    address = %address_lower,
                    "[RCPT-ACCEPTED] mailbox verified"
                );
                state.recipients.insert(address_lower);
                if let Err(e) = send_line(reader.get_mut(), "250 2.1.5 OK").await {
                    return LoopResult::Done(Err(e.into()));
                }
            }

            "DATA" => {
                if state.recipients.is_empty() {
                    if let Err(e) =
                        send_line(reader.get_mut(), "503 5.5.1 No valid recipients").await
                    {
                        return LoopResult::Done(Err(e.into()));
                    }
                    continue;
                }

                if let Err(e) = send_line(
                    reader.get_mut(),
                    "354 Start mail input; end with <CRLF>.<CRLF>",
                )
                .await
                {
                    return LoopResult::Done(Err(e.into()));
                }

                let data = match read_data(reader, config.max_message_size).await {
                    Ok(data) => data,
                    Err(e) => {
                        let _ = send_line(reader.get_mut(), "552 5.3.4 Message too large").await;
                        debug!(peer = %peer_addr, error = %e, "data read error");
                        continue;
                    }
                };

                let sender = state.sender.as_deref().unwrap_or("");
                let recipients: Vec<String> = state.recipients.iter().cloned().collect();

                match relay::relay_message(&config.backend_addr, sender, &recipients, &data).await {
                    Ok(()) => {
                        metrics
                            .accepted
                            .fetch_add(recipients.len() as u64, Ordering::Relaxed);
                        info!(
                            peer = %peer_addr,
                            sender = sender,
                            recipients = ?recipients,
                            size = data.len(),
                            "[MAIL-RELAYED] forwarded to backend"
                        );
                        if let Err(e) =
                            send_line(reader.get_mut(), "250 2.0.0 OK message accepted").await
                        {
                            return LoopResult::Done(Err(e.into()));
                        }
                    }
                    Err(e) => {
                        metrics.relay_errors.fetch_add(1, Ordering::Relaxed);
                        warn!(
                            peer = %peer_addr,
                            error = %e,
                            "[RELAY-ERROR] failed to forward to backend"
                        );
                        if let Err(e) = send_line(
                            reader.get_mut(),
                            "451 4.3.0 Temporary relay failure, try again later",
                        )
                        .await
                        {
                            return LoopResult::Done(Err(e.into()));
                        }
                    }
                }

                state.reset_transaction();
            }

            "RSET" => {
                state.reset_transaction();
                if let Err(e) = send_line(reader.get_mut(), "250 2.0.0 OK").await {
                    return LoopResult::Done(Err(e.into()));
                }
            }

            "NOOP" => {
                if let Err(e) = send_line(reader.get_mut(), "250 2.0.0 OK").await {
                    return LoopResult::Done(Err(e.into()));
                }
            }

            "QUIT" => {
                let _ = send_line(reader.get_mut(), "221 2.0.0 Bye").await;
                return LoopResult::Done(Ok(()));
            }

            "VRFY" => {
                if let Err(e) = send_line(reader.get_mut(), "252 2.5.2 Cannot verify user").await {
                    return LoopResult::Done(Err(e.into()));
                }
            }

            "" => {}

            _ => {
                if let Err(e) =
                    send_line(reader.get_mut(), "502 5.5.2 Command not recognized").await
                {
                    return LoopResult::Done(Err(e.into()));
                }
            }
        }
    }
}

/// Check if a domain (or its parent) is in the accepted set.
/// Supports subdomain matching: `abc.tempy.email` matches if `tempy.email` is accepted.
pub fn is_domain_accepted(domain: &str, accepted: &std::collections::HashSet<String>) -> bool {
    accepted.contains(domain)
        || domain
            .find('.')
            .and_then(|i| domain.get(i + 1..))
            .map(|parent| accepted.contains(parent))
            .unwrap_or(false)
}

/// Parse the first word (command) and the rest (arguments) from an SMTP line.
/// The command is uppercased for case-insensitive matching per RFC 5321.
pub fn parse_command(line: &str) -> (String, &str) {
    let trimmed = line.trim();
    match trimmed.find(' ') {
        Some(pos) => {
            let cmd = trimmed[..pos].to_ascii_uppercase();
            (cmd, trimmed[pos + 1..].trim())
        }
        None => (trimmed.to_ascii_uppercase(), ""),
    }
}

/// Extract an email address from SMTP arguments like `FROM:<addr>` or `TO:<addr>`.
pub fn extract_address(args: &str) -> Option<String> {
    let start = args.find('<')?;
    let end = args.find('>')?;
    if end > start + 1 {
        Some(args[start + 1..end].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    // -- read_line (bounded) --

    #[tokio::test]
    async fn read_line_normal() {
        let input = b"EHLO example.com\r\n";
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        let result = read_line(&mut reader, &mut buf, 1024).await.unwrap();
        assert_eq!(result, Some("EHLO example.com".to_string()));
    }

    #[tokio::test]
    async fn read_line_lf_only() {
        let input = b"QUIT\n";
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        let result = read_line(&mut reader, &mut buf, 1024).await.unwrap();
        assert_eq!(result, Some("QUIT".to_string()));
    }

    #[tokio::test]
    async fn read_line_eof_empty() {
        let input = b"";
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        let result = read_line(&mut reader, &mut buf, 1024).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn read_line_eof_with_partial_data() {
        let input = b"PARTIAL";
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        let result = read_line(&mut reader, &mut buf, 1024).await.unwrap();
        assert_eq!(result, Some("PARTIAL".to_string()));
    }

    #[tokio::test]
    async fn read_line_exceeds_limit() {
        // 10 bytes of 'A' without a newline, limit of 5
        let input = b"AAAAAAAAAA";
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        let result = read_line(&mut reader, &mut buf, 5).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_line_exactly_at_limit() {
        // 5 bytes + newline, limit of 5 — should succeed
        let input = b"ABCDE\n";
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        let result = read_line(&mut reader, &mut buf, 5).await.unwrap();
        assert_eq!(result, Some("ABCDE".to_string()));
    }

    #[tokio::test]
    async fn read_line_one_over_limit() {
        // 6 bytes without newline, limit of 5 — should fail
        let input = b"ABCDEF\n";
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        let result = read_line(&mut reader, &mut buf, 5).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_line_multiple_lines() {
        let input = b"LINE1\r\nLINE2\r\n";
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();

        let r1 = read_line(&mut reader, &mut buf, 1024).await.unwrap();
        assert_eq!(r1, Some("LINE1".to_string()));

        let r2 = read_line(&mut reader, &mut buf, 1024).await.unwrap();
        assert_eq!(r2, Some("LINE2".to_string()));

        let r3 = read_line(&mut reader, &mut buf, 1024).await.unwrap();
        assert_eq!(r3, None);
    }

    // -- read_data (no dot-unstuffing, raw wire format) --

    #[tokio::test]
    async fn read_data_simple_message() {
        let input = b"Subject: test\r\n\r\nHello world\r\n.\r\n";
        let mut reader = BufReader::new(&input[..]);
        let data = read_data(&mut reader, 10_000).await.unwrap();
        assert_eq!(data, b"Subject: test\r\n\r\nHello world\r\n");
    }

    #[tokio::test]
    async fn read_data_preserves_dot_stuffing() {
        // ".." lines should be passed through raw (no unstuffing)
        let input = b"..leading dot\r\n.\r\n";
        let mut reader = BufReader::new(&input[..]);
        let data = read_data(&mut reader, 10_000).await.unwrap();
        // Raw wire format: the ".." is preserved
        assert_eq!(data, b"..leading dot\r\n");
    }

    #[tokio::test]
    async fn read_data_dot_only_terminates() {
        let input = b"line1\r\n.\r\n";
        let mut reader = BufReader::new(&input[..]);
        let data = read_data(&mut reader, 10_000).await.unwrap();
        assert_eq!(data, b"line1\r\n");
    }

    #[tokio::test]
    async fn read_data_dot_lf_terminates() {
        // Lone "." with just LF (no CR)
        let input = b"line1\n.\n";
        let mut reader = BufReader::new(&input[..]);
        let data = read_data(&mut reader, 10_000).await.unwrap();
        assert_eq!(data, b"line1\n");
    }

    #[tokio::test]
    async fn read_data_exceeds_max_size() {
        // 100 bytes of data, max_size of 10
        let mut input = Vec::new();
        for _ in 0..20 {
            input.extend_from_slice(b"AAAAA\r\n");
        }
        input.extend_from_slice(b".\r\n");
        let mut reader = BufReader::new(&input[..]);
        let result = read_data(&mut reader, 10).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_data_eof_before_terminator() {
        let input = b"line1\r\nline2\r\n";
        let mut reader = BufReader::new(&input[..]);
        let result = read_data(&mut reader, 10_000).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn read_data_empty_message() {
        // Just a terminator, no body
        let input = b".\r\n";
        let mut reader = BufReader::new(&input[..]);
        let data = read_data(&mut reader, 10_000).await.unwrap();
        assert!(data.is_empty());
    }

    #[tokio::test]
    async fn read_data_dot_in_middle_of_line_not_terminator() {
        // A line with "." in it but not alone
        let input = b".not-a-terminator\r\n.\r\n";
        let mut reader = BufReader::new(&input[..]);
        let data = read_data(&mut reader, 10_000).await.unwrap();
        // ".not-a-terminator" is not a lone ".", so it's included in data
        assert_eq!(data, b".not-a-terminator\r\n");
    }
}
