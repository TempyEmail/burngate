use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arrayvec::ArrayString;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::lookup::{MailboxCheck, Verdict};
use crate::relay;
use crate::tls::TlsConfig;

/// Longest path (address inside `<...>`) we accept, per RFC 5321 §4.5.3.1.3.
const MAX_PATH_LEN: usize = 256;
/// Longest HELO/EHLO name we keep.
const MAX_HELO_LEN: usize = 255;

/// Global counters for monitoring.
#[derive(Default)]
pub struct Metrics {
    pub accepted: AtomicU64,
    pub rejected: AtomicU64,
    /// Recipients answered with a temporary failure (e.g. Redis unavailable).
    pub deferred: AtomicU64,
    pub connections: AtomicU64,
    pub relay_errors: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Shared SMTP session state (preserved across TLS upgrade).
struct SessionState {
    /// Name given in HELO/EHLO, sanitized. `None` until the client greets.
    helo: Option<String>,
    /// True if the client used EHLO (vs. HELO).
    esmtp: bool,
    /// Envelope sender. `Some("")` is the null sender `<>`.
    sender: Option<String>,
    /// Accepted recipients for the current transaction, in order.
    recipients: Vec<String>,
    /// Running count of RCPT TO commands in this session (not reset per transaction).
    recipient_count: usize,
}

impl SessionState {
    fn new() -> Self {
        Self {
            helo: None,
            esmtp: false,
            sender: None,
            recipients: Vec::new(),
            recipient_count: 0,
        }
    }

    fn reset_transaction(&mut self) {
        self.sender = None;
        self.recipients.clear();
    }
}

/// Immutable context shared across the SMTP command loop.
struct SmtpContext<'a, L> {
    peer_addr: std::net::SocketAddr,
    config: &'a Config,
    lookup: &'a L,
    tls_config: &'a Option<TlsConfig>,
    metrics: &'a Metrics,
    tls_active: bool,
    idle: Duration,
}

impl<L> SmtpContext<'_, L> {
    fn starttls_available(&self) -> bool {
        !self.tls_active && self.tls_config.as_ref().is_some_and(|t| t.is_ready())
    }
}

/// Handle a single SMTP session.
#[tracing::instrument(skip_all, fields(peer = %peer_addr))]
pub async fn handle_session<S, L>(
    stream: S,
    peer_addr: std::net::SocketAddr,
    config: Arc<Config>,
    lookup: L,
    tls_config: Option<TlsConfig>,
    metrics: Arc<Metrics>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    L: MailboxCheck,
{
    metrics.connections.fetch_add(1, Ordering::Relaxed);
    info!(peer = %peer_addr, "new connection");

    let timeout = Duration::from_secs(config.connection_timeout_secs);

    let result = tokio::time::timeout(
        timeout,
        run_session(stream, peer_addr, &config, &lookup, &tls_config, &metrics),
    )
    .await;

    match result {
        Ok(Ok(())) => debug!(peer = %peer_addr, "session completed"),
        Ok(Err(e)) => debug!(peer = %peer_addr, error = %e, "session error"),
        Err(_) => debug!(peer = %peer_addr, "session timed out"),
    }
}

type SessionResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

async fn run_session<S, L>(
    stream: S,
    peer_addr: std::net::SocketAddr,
    config: &Config,
    lookup: &L,
    tls_config: &Option<TlsConfig>,
    metrics: &Metrics,
) -> SessionResult
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    L: MailboxCheck,
{
    let mut reader = BufReader::new(stream);
    let mut state = SessionState::new();

    send_line(
        reader.get_mut(),
        &format!("220 {} ESMTP burngate", config.server_name),
    )
    .await?;

    let mut ctx = SmtpContext {
        peer_addr,
        config,
        lookup,
        tls_config,
        metrics,
        tls_active: false,
        idle: Duration::from_secs(config.idle_timeout_secs.max(1)),
    };

    match smtp_loop(&mut reader, &mut state, &ctx).await {
        LoopResult::Done(r) => r,
        LoopResult::StartTls => {
            let tls_cfg = tls_config
                .as_ref()
                .ok_or("STARTTLS accepted without TLS config")?;

            // Anything the client pipelined after STARTTLS was sent in
            // plaintext and must not be executed inside the TLS session
            // (CVE-2011-0411 class). `into_inner()` drops the buffer.
            if !reader.buffer().is_empty() {
                warn!(peer = %peer_addr, "discarding plaintext pipelined after STARTTLS");
            }
            let stream = reader.into_inner();
            let tls_stream = tls_cfg.accept(stream).await?;
            info!(peer = %peer_addr, "STARTTLS handshake completed");

            // RFC 3207: forget everything learned before TLS; client must re-EHLO.
            state.helo = None;
            state.esmtp = false;
            state.reset_transaction();

            let mut tls_reader = BufReader::new(tls_stream);
            ctx.tls_active = true;
            match smtp_loop(&mut tls_reader, &mut state, &ctx).await {
                LoopResult::Done(r) => r,
                LoopResult::StartTls => Err("STARTTLS requested on already-TLS connection".into()),
            }
        }
    }
}

enum LoopResult {
    Done(SessionResult),
    StartTls,
}

/// Write an SMTP response line.
async fn send_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    line: &str,
) -> Result<(), std::io::Error> {
    let mut buf = Vec::with_capacity(line.len() + 2);
    buf.extend_from_slice(line.as_bytes());
    buf.extend_from_slice(b"\r\n");
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

/// Send an SMTP response line, returning from the loop on write error.
macro_rules! send_or_return {
    ($reader:expr, $line:expr) => {
        if let Err(e) = send_line($reader.get_mut(), $line).await {
            return LoopResult::Done(Err(e.into()));
        }
    };
}

/// One line read from the client.
#[derive(Debug, PartialEq)]
enum Line {
    Text(String),
    /// The line exceeded the limit; it has been read and discarded.
    TooLong,
    Eof,
}

/// Read a single line from the SMTP client with a hard byte limit.
///
/// Reads in buffered chunks via `fill_buf()`/`consume()`. A line longer
/// than `max_len` is discarded through its newline without being stored,
/// so memory use stays bounded however long the line is.
async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
    max_len: usize,
) -> Result<Line, std::io::Error> {
    buf.clear();
    let mut too_long = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if too_long {
                return Ok(Line::TooLong);
            }
            if buf.is_empty() {
                return Ok(Line::Eof);
            }
            return Ok(Line::Text(
                String::from_utf8_lossy(buf).trim_end().to_string(),
            ));
        }

        let (chunk, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos, true),
            None => (available.len(), false),
        };
        if !too_long {
            buf.extend_from_slice(&available[..chunk]);
            // Don't count the CR of a CRLF (which may still be unread) toward the limit
            let over = if found_newline {
                buf.len() - usize::from(buf.last() == Some(&b'\r')) > max_len
            } else {
                buf.len() > max_len + 1
            };
            if over {
                too_long = true;
                buf.clear();
            }
        }
        reader.consume(chunk + usize::from(found_newline));

        if found_newline {
            if too_long {
                return Ok(Line::TooLong);
            }
            return Ok(Line::Text(
                String::from_utf8_lossy(buf).trim_end().to_string(),
            ));
        }
    }
}

/// Result of reading a DATA section.
#[derive(Debug, PartialEq)]
enum DataOutcome {
    /// Complete message: dot-unstuffed, every line ending in CRLF.
    Message(Vec<u8>),
    /// The message exceeded the size limit and was discarded.
    TooLarge,
    /// The message contained a bare CR (CR not followed by LF) and was discarded.
    BareCr,
}

/// Read the DATA portion of an SMTP message up to the lone "." line.
///
/// - Always reads through the terminator, even when the message is rejected,
///   so the rest of the message is never interpreted as commands.
/// - Memory use is bounded by `max_size`, whether or not the input has newlines.
/// - Removes dot-stuffing (RFC 5321 §4.5.2) and normalizes line endings to
///   CRLF; the relay re-applies dot-stuffing. The backend therefore never sees
///   an ambiguous end-of-data sequence (SMTP smuggling).
/// - Bare LF is accepted as a line ending (common from sloppy senders); bare
///   CR is rejected, because it can't be normalized without ambiguity.
async fn read_data<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max_size: usize,
    idle: Duration,
) -> Result<DataOutcome, std::io::Error> {
    let mut out = Vec::with_capacity(8192);
    let mut line_start = 0usize; // index in `out` where the current line begins
    let mut line_len = 0usize; // bytes seen in the current line (excl. line ending)
    let mut first_dot = false; // current line starts with '.'
    let mut pending_cr = false; // previous byte was CR
    let mut failure: Option<DataOutcome> = None;

    loop {
        let available = match tokio::time::timeout(idle, reader.fill_buf()).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "idle timeout during DATA",
                ))
            }
        };
        if available.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed during DATA",
            ));
        }

        let mut consumed = 0;
        let mut done = false;
        for &b in available {
            consumed += 1;
            if pending_cr {
                pending_cr = false;
                if b != b'\n' {
                    // Bare CR: remember the failure, keep scanning for the end
                    line_len += 1;
                    if failure.is_none() {
                        failure = Some(DataOutcome::BareCr);
                        out = Vec::new();
                    }
                }
            }
            match b {
                b'\n' => {
                    if line_len == 1 && first_dot {
                        // The lone "." was already appended; drop it
                        out.truncate(line_start);
                        done = true;
                        break;
                    }
                    if failure.is_none() {
                        if first_dot {
                            out.remove(line_start); // dot-unstuff
                        }
                        out.extend_from_slice(b"\r\n");
                        line_start = out.len();
                    }
                    line_len = 0;
                    first_dot = false;
                }
                b'\r' => pending_cr = true,
                _ => {
                    if line_len == 0 && b == b'.' {
                        first_dot = true;
                    }
                    line_len += 1;
                    if failure.is_none() {
                        out.push(b);
                        if out.len() > max_size {
                            failure = Some(DataOutcome::TooLarge);
                            out = Vec::new();
                        }
                    }
                }
            }
        }
        reader.consume(consumed);
        if done {
            return Ok(failure.unwrap_or(DataOutcome::Message(out)));
        }
    }
}

/// Main SMTP command loop, generic over the stream type.
///
/// Uses `BufReader<S>` where S implements both AsyncRead and AsyncWrite.
/// Writes go through `reader.get_mut()` since SMTP is half-duplex.
async fn smtp_loop<S, L>(
    reader: &mut BufReader<S>,
    state: &mut SessionState,
    ctx: &SmtpContext<'_, L>,
) -> LoopResult
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: MailboxCheck,
{
    let mut line_buf = Vec::with_capacity(1024);
    let server = ctx.config.server_name.as_str();

    loop {
        let read = tokio::time::timeout(
            ctx.idle,
            read_line(reader, &mut line_buf, ctx.config.max_line_length),
        )
        .await;
        let line = match read {
            Ok(Ok(Line::Text(line))) => line,
            Ok(Ok(Line::TooLong)) => {
                send_or_return!(reader, "500 5.5.2 Line too long");
                continue;
            }
            Ok(Ok(Line::Eof)) => return LoopResult::Done(Ok(())),
            Ok(Err(e)) => {
                debug!(peer = %ctx.peer_addr, error = %e, "read error");
                return LoopResult::Done(Ok(()));
            }
            Err(_) => {
                debug!(peer = %ctx.peer_addr, "idle timeout");
                let msg = format!("421 4.4.2 {server} Idle timeout, closing connection");
                let _ = send_line(reader.get_mut(), &msg).await;
                return LoopResult::Done(Ok(()));
            }
        };

        let (command, args) = parse_command(&line);

        match command.as_str() {
            "EHLO" | "HELO" => {
                let name = sanitize_helo(args);
                if name.is_empty() {
                    let msg = format!("501 5.5.4 Syntax: {} hostname", command.as_str());
                    send_or_return!(reader, &msg);
                    continue;
                }
                // A greeting resets any transaction in progress (RFC 5321 §4.1.4)
                state.reset_transaction();
                state.esmtp = command.as_str() == "EHLO";
                state.helo = Some(name.clone());

                if !state.esmtp {
                    send_or_return!(reader, &format!("250 {server} Hello {name}"));
                    continue;
                }
                let mut caps = vec![
                    format!("250-{server} Hello {name}"),
                    format!("250-SIZE {}", ctx.config.max_message_size),
                    "250-8BITMIME".to_string(),
                    "250-PIPELINING".to_string(),
                    "250-ENHANCEDSTATUSCODES".to_string(),
                ];
                if ctx.starttls_available() {
                    caps.push("250-STARTTLS".to_string());
                }
                if let Some(last) = caps.last_mut() {
                    *last = last.replacen("250-", "250 ", 1);
                }
                let reply = caps.join("\r\n");
                send_or_return!(reader, &reply);
            }

            "STARTTLS" => {
                if ctx.tls_active {
                    send_or_return!(reader, "503 5.5.1 TLS already active");
                } else if !args.is_empty() {
                    send_or_return!(reader, "501 5.5.4 Syntax: STARTTLS");
                } else if ctx.starttls_available() {
                    send_or_return!(reader, "220 2.0.0 Ready to start TLS");
                    return LoopResult::StartTls;
                } else {
                    send_or_return!(reader, "502 5.5.1 STARTTLS not available");
                }
            }

            "MAIL" => {
                if state.helo.is_none() {
                    send_or_return!(reader, "503 5.5.1 Send HELO/EHLO first");
                    continue;
                }
                if state.sender.is_some() {
                    send_or_return!(reader, "503 5.5.1 Nested MAIL command");
                    continue;
                }
                let (sender, params) = match parse_path(args, "FROM:") {
                    Some(p) => p,
                    None => {
                        send_or_return!(reader, "501 5.1.7 Bad sender address syntax");
                        continue;
                    }
                };
                if let Some(size) = declared_size(params) {
                    if size > ctx.config.max_message_size {
                        send_or_return!(reader, "552 5.3.4 Message size exceeds fixed limit");
                        continue;
                    }
                }
                state.sender = Some(sender);
                state.recipients.clear();
                send_or_return!(reader, "250 2.1.0 OK");
            }

            "RCPT" => {
                if state.sender.is_none() {
                    send_or_return!(reader, "503 5.5.1 Need MAIL command");
                    continue;
                }
                let address = match parse_path(args, "TO:") {
                    Some((addr, _)) if is_mailbox(&addr) => addr,
                    _ => {
                        send_or_return!(reader, "501 5.1.3 Bad recipient address syntax");
                        continue;
                    }
                };

                // Enforce per-session RCPT TO limit
                state.recipient_count += 1;
                if state.recipient_count > ctx.config.max_recipients {
                    warn!(
                        peer = %ctx.peer_addr,
                        count = state.recipient_count,
                        max = ctx.config.max_recipients,
                        "RCPT TO limit exceeded"
                    );
                    send_or_return!(reader, "452 4.5.3 Too many recipients");
                    continue;
                }

                let address_lower = address.to_lowercase();
                let domain = address_lower
                    .rsplit('@')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches('.');

                if !is_domain_accepted(domain, &ctx.config.accepted_domains) {
                    info!(
                        peer = %ctx.peer_addr,
                        address = %address_lower,
                        domain = domain,
                        "[MAIL-REJECTED] unknown domain"
                    );
                    ctx.metrics.rejected.fetch_add(1, Ordering::Relaxed);
                    send_or_return!(reader, "550 5.1.2 Unknown domain");
                    continue;
                }

                // Check Redis for mailbox existence — the key spam-filtering step
                match ctx.lookup.check(&address_lower).await {
                    Verdict::Accept => {}
                    Verdict::Reject => {
                        info!(
                            peer = %ctx.peer_addr,
                            address = %address_lower,
                            "[MAIL-REJECTED] mailbox not found"
                        );
                        ctx.metrics.rejected.fetch_add(1, Ordering::Relaxed);
                        send_or_return!(reader, "550 5.1.1 User unknown");
                        continue;
                    }
                    Verdict::TempFail => {
                        warn!(
                            peer = %ctx.peer_addr,
                            address = %address_lower,
                            "[MAIL-DEFERRED] mailbox lookup failed"
                        );
                        ctx.metrics.deferred.fetch_add(1, Ordering::Relaxed);
                        send_or_return!(
                            reader,
                            "451 4.3.0 Temporary lookup failure, try again later"
                        );
                        continue;
                    }
                }

                info!(
                    peer = %ctx.peer_addr,
                    address = %address_lower,
                    "[RCPT-ACCEPTED] mailbox verified"
                );
                if !state.recipients.contains(&address_lower) {
                    state.recipients.push(address_lower);
                }
                send_or_return!(reader, "250 2.1.5 OK");
            }

            "DATA" => {
                if state.sender.is_none() {
                    send_or_return!(reader, "503 5.5.1 Need MAIL command");
                    continue;
                }
                if state.recipients.is_empty() {
                    send_or_return!(reader, "554 5.5.1 No valid recipients");
                    continue;
                }

                send_or_return!(reader, "354 Start mail input; end with <CRLF>.<CRLF>");

                let outcome = read_data(reader, ctx.config.max_message_size, ctx.idle).await;
                let data = match outcome {
                    Ok(DataOutcome::Message(data)) => data,
                    Ok(DataOutcome::TooLarge) => {
                        debug!(peer = %ctx.peer_addr, "message too large");
                        state.reset_transaction();
                        send_or_return!(reader, "552 5.3.4 Message size exceeds fixed limit");
                        continue;
                    }
                    Ok(DataOutcome::BareCr) => {
                        info!(peer = %ctx.peer_addr, "[MAIL-REJECTED] bare CR in message");
                        state.reset_transaction();
                        send_or_return!(reader, "554 5.6.0 Message contains bare CR, rejected");
                        continue;
                    }
                    Err(e) => {
                        debug!(peer = %ctx.peer_addr, error = %e, "data read error");
                        if e.kind() == std::io::ErrorKind::TimedOut {
                            let msg =
                                format!("421 4.4.2 {server} Idle timeout, closing connection");
                            let _ = send_line(reader.get_mut(), &msg).await;
                        }
                        return LoopResult::Done(Ok(()));
                    }
                };

                let sender = state.sender.clone().unwrap_or_default();
                let recipients = std::mem::take(&mut state.recipients);
                state.reset_transaction();

                let mut message = received_header(
                    state.helo.as_deref().unwrap_or("unknown"),
                    ctx.peer_addr.ip(),
                    server,
                    protocol_name(state.esmtp, ctx.tls_active),
                    &recipients,
                )
                .into_bytes();
                message.extend_from_slice(&data);
                drop(data);

                let relay = tokio::time::timeout(
                    Duration::from_secs(ctx.config.relay_timeout_secs.max(1)),
                    relay::relay_message(
                        &ctx.config.backend_addr,
                        server,
                        &sender,
                        &recipients,
                        &message,
                    ),
                )
                .await
                .unwrap_or(Err(relay::RelayError::Timeout));

                match relay {
                    Ok(report) => {
                        let delivered = recipients.len() - report.rejected_recipients.len();
                        ctx.metrics
                            .accepted
                            .fetch_add(delivered as u64, Ordering::Relaxed);
                        if !report.rejected_recipients.is_empty() {
                            ctx.metrics.relay_errors.fetch_add(1, Ordering::Relaxed);
                            warn!(
                                peer = %ctx.peer_addr,
                                rejected = ?report.rejected_recipients,
                                "[RELAY-ERROR] backend rejected some recipients; delivered to the rest"
                            );
                        }
                        info!(
                            peer = %ctx.peer_addr,
                            sender = %sender,
                            recipients = ?recipients,
                            size = message.len(),
                            "[MAIL-RELAYED] forwarded to backend"
                        );
                        send_or_return!(reader, "250 2.0.0 OK message accepted");
                    }
                    Err(e) => {
                        ctx.metrics.relay_errors.fetch_add(1, Ordering::Relaxed);
                        warn!(
                            peer = %ctx.peer_addr,
                            error = %e,
                            permanent = e.is_permanent(),
                            "[RELAY-ERROR] failed to forward to backend"
                        );
                        let reply = match &e {
                            relay::RelayError::Rejected { code: 552, .. } => {
                                "552 5.3.4 Message size exceeds limit"
                            }
                            _ if e.is_permanent() => "554 5.0.0 Message rejected by mail server",
                            _ => "451 4.3.0 Temporary relay failure, try again later",
                        };
                        send_or_return!(reader, reply);
                    }
                }
            }

            "RSET" => {
                state.reset_transaction();
                send_or_return!(reader, "250 2.0.0 OK");
            }

            "NOOP" => {
                send_or_return!(reader, "250 2.0.0 OK");
            }

            "QUIT" => {
                let _ = send_line(reader.get_mut(), "221 2.0.0 Bye").await;
                return LoopResult::Done(Ok(()));
            }

            "VRFY" => {
                send_or_return!(reader, "252 2.5.2 Cannot verify user");
            }

            "" => {}

            _ => {
                send_or_return!(reader, "502 5.5.2 Command not recognized");
            }
        }
    }
}

/// "SMTP", "ESMTP" or "ESMTPS", for the `with` clause of the Received header.
fn protocol_name(esmtp: bool, tls: bool) -> &'static str {
    match (esmtp, tls) {
        (true, true) => "ESMTPS",
        (true, false) => "ESMTP",
        (false, true) => "SMTPS",
        (false, false) => "SMTP",
    }
}

/// Build the `Received:` trace header this hop adds (RFC 5321 §4.4).
///
/// It records the real client IP, which the backend can't otherwise see
/// since every connection it gets comes from burngate.
fn received_header(
    helo: &str,
    ip: std::net::IpAddr,
    server_name: &str,
    protocol: &str,
    recipients: &[String],
) -> String {
    let ip = match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => format!("IPv6:{v6}"),
        },
    };
    // Only name the recipient when there is one, so a message to several
    // recipients doesn't reveal the others.
    let for_clause = match recipients {
        [only] => format!("\r\n\tfor <{only}>"),
        _ => String::new(),
    };
    format!(
        "Received: from {helo} ([{ip}])\r\n\tby {server_name} (burngate) with {protocol}{for_clause};\r\n\t{}\r\n",
        rfc5322_date(std::time::SystemTime::now())
    )
}

/// Format a time as an RFC 5322 date in UTC, e.g. `Tue, 24 Nov 2026 21:05:09 +0000`.
pub fn rfc5322_date(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} +0000",
        WEEKDAYS[days.rem_euclid(7) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        h,
        m,
        s
    )
}

/// Keep only the first word of a HELO/EHLO argument, limited to printable
/// ASCII, so it is safe to echo back and to put in a Received header.
fn sanitize_helo(args: &str) -> String {
    args.split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_graphic() && !matches!(c, '(' | ')' | ';' | '\\'))
        .take(MAX_HELO_LEN)
        .collect()
}

/// Check if a domain (or any parent domain) is in the accepted set.
/// Supports subdomain matching at any depth: `abc.tempy.email` and
/// `a.b.tempy.email` both match if `tempy.email` is accepted.
pub fn is_domain_accepted(domain: &str, accepted: &HashSet<String>) -> bool {
    let mut candidate = domain;
    loop {
        if candidate.is_empty() {
            return false;
        }
        if accepted.contains(candidate) {
            return true;
        }
        match candidate.find('.') {
            Some(i) => candidate = &candidate[i + 1..],
            None => return false,
        }
    }
}

/// Parse the first word (command) and the rest (arguments) from an SMTP line.
/// The command is uppercased for case-insensitive matching per RFC 5321.
/// Uses a stack-allocated `ArrayString<8>` since SMTP commands are at most 8 bytes;
/// a longer first word is returned as `*`, which never matches a command.
pub fn parse_command(line: &str) -> (ArrayString<8>, &str) {
    let trimmed = line.trim();
    let (word, args) = match trimmed.find(' ') {
        Some(pos) => (&trimmed[..pos], trimmed[pos + 1..].trim()),
        None => (trimmed, ""),
    };
    let mut cmd = ArrayString::<8>::new();
    if word.len() > 8 {
        cmd.push('*');
    } else {
        for ch in word.chars() {
            let _ = cmd.try_push(ch.to_ascii_uppercase());
        }
    }
    (cmd, args)
}

/// Parse a `MAIL`/`RCPT` argument like `FROM:<addr> SIZE=123`.
///
/// `keyword` is `"FROM:"` or `"TO:"` (matched case-insensitively). Returns the
/// address inside the angle brackets (empty for the null path `<>`) and the
/// remaining ESMTP parameters. Rejects control characters, nested brackets
/// and over-long paths, so the address is safe to pass to the backend.
pub fn parse_path<'a>(args: &'a str, keyword: &str) -> Option<(String, &'a str)> {
    let head = args.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = args[keyword.len()..].trim_start();
    let rest = rest.strip_prefix('<')?;
    let end = rest.find('>')?;
    let addr = &rest[..end];
    if addr.len() > MAX_PATH_LEN || addr.chars().any(|c| c.is_control() || c == '<') {
        return None;
    }
    Some((addr.to_string(), rest[end + 1..].trim()))
}

/// Extract an email address from SMTP arguments like `FROM:<addr>` or `TO:<addr>`.
/// Returns `None` for the null path `<>`.
pub fn extract_address(args: &str) -> Option<String> {
    let colon = args.find(':')?;
    let (keyword, _) = args.split_at(colon + 1);
    parse_path(args, keyword)
        .map(|(addr, _)| addr)
        .filter(|a| !a.is_empty())
}

/// True for `local@domain` with both parts non-empty.
fn is_mailbox(addr: &str) -> bool {
    match addr.rsplit_once('@') {
        Some((local, domain)) => !local.is_empty() && !domain.is_empty() && !addr.contains(' '),
        None => false,
    }
}

/// The `SIZE=` value from MAIL FROM parameters, if present and valid.
fn declared_size(params: &str) -> Option<usize> {
    params.split_whitespace().find_map(|p| {
        let (k, v) = p.split_once('=')?;
        if k.eq_ignore_ascii_case("SIZE") {
            v.parse().ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests;
