use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, error, info};

/// Read a single SMTP response line and extract the status code.
async fn read_response(
    reader: &mut BufReader<tokio::io::ReadHalf<TcpStream>>,
    buf: &mut String,
) -> Result<(u16, String), RelayError> {
    buf.clear();
    reader.read_line(buf).await?;
    let code: u16 = buf.get(..3).and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((code, buf.clone()))
}

/// Relay a complete SMTP message to the backend server.
///
/// Performs a full SMTP transaction: connect, EHLO, MAIL FROM, RCPT TO (for each
/// recipient), DATA, message body, QUIT.
#[tracing::instrument(skip(message_data), fields(size = message_data.len()))]
pub async fn relay_message(
    backend_addr: &str,
    sender: &str,
    recipients: &[String],
    message_data: &[u8],
) -> Result<(), RelayError> {
    let stream = TcpStream::connect(backend_addr)
        .await
        .map_err(|e| RelayError::Connect(e.to_string()))?;

    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line_buf = String::new();

    // Read banner
    let (code, resp) = read_response(&mut reader, &mut line_buf).await?;
    if code != 220 {
        return Err(RelayError::Protocol(format!(
            "unexpected banner: {}",
            resp.trim()
        )));
    }
    debug!(response = %resp.trim(), "backend banner");

    // EHLO
    writer.write_all(b"EHLO burngate\r\n").await?;
    // Read all EHLO response lines (multi-line: "250-..." continues, "250 ..." ends)
    loop {
        line_buf.clear();
        reader.read_line(&mut line_buf).await?;
        if line_buf.len() < 4 {
            return Err(RelayError::Protocol(format!(
                "short EHLO response: {}",
                line_buf.trim()
            )));
        }
        if &line_buf[3..4] == " " {
            break;
        }
    }

    // MAIL FROM
    let mail_from = format!("MAIL FROM:<{}>\r\n", sender);
    writer.write_all(mail_from.as_bytes()).await?;
    let (code, resp) = read_response(&mut reader, &mut line_buf).await?;
    if code != 250 {
        return Err(RelayError::Protocol(format!(
            "MAIL FROM rejected: {}",
            resp.trim()
        )));
    }

    // RCPT TO for each recipient
    for rcpt in recipients {
        let rcpt_to = format!("RCPT TO:<{}>\r\n", rcpt);
        writer.write_all(rcpt_to.as_bytes()).await?;
        let (code, resp) = read_response(&mut reader, &mut line_buf).await?;
        if code != 250 {
            error!(recipient = %rcpt, response = %resp.trim(), "backend rejected recipient");
        }
    }

    // DATA
    writer.write_all(b"DATA\r\n").await?;
    let (code, resp) = read_response(&mut reader, &mut line_buf).await?;
    if code != 354 {
        return Err(RelayError::Protocol(format!(
            "DATA not accepted: {}",
            resp.trim()
        )));
    }

    // Inject W3C traceparent header so Ratatoskr can continue this trace.
    // No-op when OTel is not configured (carrier stays empty, nothing is written).
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
                .write_all(format!("traceparent: {}\r\n", tp).as_bytes())
                .await?;
            if let Some(ts) = carrier.get("tracestate").filter(|s| !s.is_empty()) {
                writer
                    .write_all(format!("tracestate: {}\r\n", ts).as_bytes())
                    .await?;
            }
        }
    }

    // Send message body
    writer.write_all(message_data).await?;

    // Ensure message ends with \r\n.\r\n
    if !message_data.ends_with(b"\r\n") {
        writer.write_all(b"\r\n").await?;
    }
    writer.write_all(b".\r\n").await?;

    let (code, resp) = read_response(&mut reader, &mut line_buf).await?;
    if code != 250 {
        return Err(RelayError::Protocol(format!(
            "message not accepted: {}",
            resp.trim()
        )));
    }

    // QUIT
    writer.write_all(b"QUIT\r\n").await?;

    info!(
        sender = sender,
        recipients = ?recipients,
        size = message_data.len(),
        "message relayed to backend"
    );

    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
}
