use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// Longest backend response line we are willing to buffer.
const MAX_RESPONSE_LINE: usize = 4096;
/// Most lines we accept in one multi-line backend response.
const MAX_RESPONSE_LINES: usize = 100;

type Reader = BufReader<tokio::io::ReadHalf<TcpStream>>;

/// Read a complete (possibly multi-line) SMTP response.
///
/// Multi-line responses use `250-...` for continuation lines and `250 ...`
/// for the last one. Returns the status code and the text of the last line.
async fn read_response(reader: &mut Reader) -> Result<(u16, String), RelayError> {
    let mut line = Vec::with_capacity(128);
    for _ in 0..MAX_RESPONSE_LINES {
        line.clear();
        let n = (&mut *reader)
            .take(MAX_RESPONSE_LINE as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if n == 0 {
            return Err(RelayError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "backend closed the connection",
            )));
        }
        let text = String::from_utf8_lossy(&line).trim_end().to_string();
        let code: u16 = text.get(..3).and_then(|s| s.parse().ok()).unwrap_or(0);
        if code == 0 {
            return Err(RelayError::Protocol(format!("malformed response: {text}")));
        }
        // "250-" continues, "250 " or bare "250" ends the response
        if text.as_bytes().get(3) != Some(&b'-') {
            return Ok((code, text));
        }
    }
    Err(RelayError::Protocol("backend response too long".into()))
}

/// Turn a status code + text into a `Rejected` error.
fn rejected(stage: &'static str, code: u16, text: String) -> RelayError {
    RelayError::Rejected { stage, code, text }
}

/// Dot-stuff a message for transmission and terminate it with `.`.
///
/// The input is the message as received after dot-unstuffing, with CRLF line
/// endings. Every line that starts with `.` gets an extra `.` prepended, so
/// the backend can never see a premature end-of-data marker.
pub fn encode_data(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + message.len() / 64 + 8);
    let mut at_line_start = true;
    for &b in message {
        if at_line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        at_line_start = b == b'\n';
    }
    if !message.is_empty() && !message.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// Outcome of a successful relay.
#[derive(Debug, Default)]
pub struct RelayReport {
    /// Recipients the backend permanently rejected (address, response).
    /// The message was delivered to all other recipients.
    pub rejected_recipients: Vec<(String, String)>,
}

/// Relay a complete SMTP message to the backend server.
///
/// Performs a full SMTP transaction: connect, EHLO, MAIL FROM, RCPT TO (for each
/// recipient), DATA, message body, QUIT.
///
/// `message_data` must already be dot-unstuffed with CRLF line endings
/// (as produced by the session's DATA reader).
#[tracing::instrument(skip(message_data), fields(size = message_data.len()))]
pub async fn relay_message(
    backend_addr: &str,
    helo_name: &str,
    sender: &str,
    recipients: &[String],
    message_data: &[u8],
) -> Result<RelayReport, RelayError> {
    let stream = TcpStream::connect(backend_addr)
        .await
        .map_err(|e| RelayError::Connect(e.to_string()))?;

    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    // Banner
    let (code, resp) = read_response(&mut reader).await?;
    if code != 220 {
        return Err(rejected("banner", code, resp));
    }
    debug!(response = %resp, "backend banner");

    // EHLO
    writer
        .write_all(format!("EHLO {helo_name}\r\n").as_bytes())
        .await?;
    let (code, resp) = read_response(&mut reader).await?;
    if code != 250 {
        return Err(rejected("EHLO", code, resp));
    }

    // MAIL FROM
    writer
        .write_all(format!("MAIL FROM:<{sender}>\r\n").as_bytes())
        .await?;
    let (code, resp) = read_response(&mut reader).await?;
    if code != 250 {
        return Err(rejected("MAIL FROM", code, resp));
    }

    // RCPT TO for each recipient
    let mut report = RelayReport::default();
    let mut accepted = 0usize;
    for rcpt in recipients {
        writer
            .write_all(format!("RCPT TO:<{rcpt}>\r\n").as_bytes())
            .await?;
        let (code, resp) = read_response(&mut reader).await?;
        match code {
            250 | 251 => accepted += 1,
            // A temporary rejection of any recipient: give up on the whole
            // message so the sender retries it. Nothing has been delivered yet,
            // so a retry can't cause duplicates.
            400..=499 => {
                let _ = writer.write_all(b"QUIT\r\n").await;
                return Err(rejected("RCPT TO", code, format!("{rcpt}: {resp}")));
            }
            _ => {
                warn!(recipient = %rcpt, response = %resp, "backend rejected recipient");
                report.rejected_recipients.push((rcpt.clone(), resp));
            }
        }
    }
    if accepted == 0 {
        let _ = writer.write_all(b"QUIT\r\n").await;
        let detail = report
            .rejected_recipients
            .first()
            .map(|(_, r)| r.clone())
            .unwrap_or_default();
        let code = detail.get(..3).and_then(|s| s.parse().ok()).unwrap_or(550);
        return Err(rejected("RCPT TO", code, detail));
    }

    // DATA
    writer.write_all(b"DATA\r\n").await?;
    let (code, resp) = read_response(&mut reader).await?;
    if code != 354 {
        return Err(rejected("DATA", code, resp));
    }

    // Inject W3C traceparent header so downstream processors can continue this
    // trace. No-op when OTel is not configured (carrier stays empty).
    {
        use opentelemetry::propagation::TextMapPropagator;
        let propagator = opentelemetry_sdk::propagation::TraceContextPropagator::new();
        let mut carrier = std::collections::HashMap::<String, String>::new();
        propagator.inject_context(
            &tracing_opentelemetry::OpenTelemetrySpanExt::context(&tracing::Span::current()),
            &mut carrier,
        );
        if let Some(tp) = carrier.get("traceparent") {
            writer
                .write_all(format!("traceparent: {tp}\r\n").as_bytes())
                .await?;
            if let Some(ts) = carrier.get("tracestate").filter(|s| !s.is_empty()) {
                writer
                    .write_all(format!("tracestate: {ts}\r\n").as_bytes())
                    .await?;
            }
        }
    }

    // Message body, dot-stuffed, followed by the terminator
    writer.write_all(&encode_data(message_data)).await?;
    writer.flush().await?;

    let (code, resp) = read_response(&mut reader).await?;
    if code != 250 {
        return Err(rejected("end of DATA", code, resp));
    }

    // QUIT (best effort; the message is already accepted)
    let _ = writer.write_all(b"QUIT\r\n").await;

    info!(
        sender = sender,
        recipients = ?recipients,
        size = message_data.len(),
        "message relayed to backend"
    );

    Ok(report)
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("backend rejected {stage}: {text}")]
    Rejected {
        stage: &'static str,
        code: u16,
        text: String,
    },
    #[error("relay timed out")]
    Timeout,
}

impl RelayError {
    /// True when the backend gave a permanent (5xx) answer, meaning a retry
    /// by the sender would fail the same way.
    pub fn is_permanent(&self) -> bool {
        matches!(self, RelayError::Rejected { code, .. } if (500..600).contains(code))
    }
}

#[cfg(test)]
mod tests {
    use super::encode_data;

    #[test]
    fn encode_stuffs_leading_dots() {
        assert_eq!(
            encode_data(b".hidden\r\nok\r\n.\r\n"),
            b"..hidden\r\nok\r\n..\r\n.\r\n"
        );
    }

    #[test]
    fn encode_empty_message() {
        assert_eq!(encode_data(b""), b".\r\n");
    }

    #[test]
    fn encode_adds_missing_crlf() {
        assert_eq!(encode_data(b"no newline"), b"no newline\r\n.\r\n");
    }
}
