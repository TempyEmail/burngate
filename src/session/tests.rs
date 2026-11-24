use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use super::*;
use crate::config::{CheckMode, Config};
use crate::lookup::{MailboxCheck, Verdict};

const IDLE: Duration = Duration::from_secs(60);

// -- helpers --

/// In-memory mailbox lookup.
struct FakeLookup(HashMap<&'static str, Verdict>);

impl MailboxCheck for FakeLookup {
    async fn check(&self, address: &str) -> Verdict {
        self.0.get(address).copied().unwrap_or(Verdict::Reject)
    }
}

fn lookup() -> FakeLookup {
    FakeLookup(HashMap::from([
        ("alice@tempy.email", Verdict::Accept),
        ("bob@tempy.email", Verdict::Accept),
        ("down@tempy.email", Verdict::TempFail),
    ]))
}

fn test_config(backend_addr: &str) -> Config {
    Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        backend_addr: backend_addr.to_string(),
        redis_url: String::new(),
        accepted_domains: HashSet::from(["tempy.email".to_string()]),
        max_message_size: 10_000,
        tls_cert_path: None,
        tls_key_path: None,
        server_name: "mx.test".to_string(),
        connection_timeout_secs: 300,
        redis_key_pattern: "mb:{address}".to_string(),
        redis_set_name: "addresses".to_string(),
        redis_check_mode: CheckMode::Both,
        metrics_interval_secs: 0,
        max_connections: 10,
        max_recipients: 100,
        max_line_length: 1024,
        max_connections_per_ip: 0,
        max_concurrent_per_ip: 0,
        idle_timeout_secs: 60,
        relay_timeout_secs: 10,
        redis_timeout_ms: 1000,
        tls_reload_interval_secs: 0,
        shutdown_timeout_secs: 1,
    }
}

/// A scripted backend SMTP server. Replies to RCPT TO in order from
/// `rcpt_replies` (then "250 OK"), and answers end-of-DATA with `final_reply`.
/// Resolves to the raw bytes received after DATA (including the terminator).
async fn fake_backend(
    rcpt_replies: Vec<&'static str>,
    final_reply: &'static str,
) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = tokio::io::split(stream);
        let mut r = BufReader::new(r);
        let mut rcpt_replies = rcpt_replies.into_iter();
        let mut captured = Vec::new();
        w.write_all(b"220-fake\r\n220 ready\r\n").await.unwrap();
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            let reply = match &line.to_ascii_uppercase()[..4] {
                "EHLO" => "250-fake\r\n250 PIPELINING".to_string(),
                "RCPT" => rcpt_replies.next().unwrap_or("250 OK").to_string(),
                "DATA" => {
                    w.write_all(b"354 go\r\n").await.unwrap();
                    loop {
                        let n = r.read_until(b'\n', &mut captured).await.unwrap();
                        if n == 0 || captured.ends_with(b"\r\n.\r\n") {
                            break;
                        }
                    }
                    final_reply.to_string()
                }
                "QUIT" => {
                    let _ = w.write_all(b"221 bye\r\n").await;
                    break;
                }
                _ => "250 OK".to_string(),
            };
            w.write_all(format!("{reply}\r\n").as_bytes())
                .await
                .unwrap();
        }
        captured
    });
    (addr, handle)
}

/// Run a session with `script` as the client's input and return everything
/// the server wrote.
async fn run(config: Config, script: &[u8]) -> String {
    run_with(config, lookup(), None, script).await
}

async fn run_with(
    config: Config,
    lookup: FakeLookup,
    tls: Option<TlsConfig>,
    script: &[u8],
) -> String {
    let (mut client, server) = tokio::io::duplex(1 << 20);
    let peer: SocketAddr = "203.0.113.7:40000".parse().unwrap();
    let session = tokio::spawn(handle_session(
        server,
        peer,
        Arc::new(config),
        lookup,
        tls,
        Arc::new(Metrics::new()),
    ));
    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();
    let mut out = String::new();
    client.read_to_string(&mut out).await.unwrap();
    session.await.unwrap();
    out
}

/// Reply codes in order, one per response (continuation lines skipped).
fn codes(out: &str) -> Vec<&str> {
    out.lines()
        .filter(|l| l.as_bytes().get(3) != Some(&b'-'))
        .map(|l| &l[..3])
        .collect()
}

// -- read_line (bounded) --

async fn line(input: &[u8], max: usize) -> Line {
    let mut reader = BufReader::new(input);
    read_line(&mut reader, &mut Vec::new(), max).await.unwrap()
}

#[tokio::test]
async fn read_line_normal() {
    assert_eq!(
        line(b"EHLO example.com\r\n", 1024).await,
        Line::Text("EHLO example.com".into())
    );
}

#[tokio::test]
async fn read_line_lf_only() {
    assert_eq!(line(b"QUIT\n", 1024).await, Line::Text("QUIT".into()));
}

#[tokio::test]
async fn read_line_eof_empty() {
    assert_eq!(line(b"", 1024).await, Line::Eof);
}

#[tokio::test]
async fn read_line_eof_with_partial_data() {
    assert_eq!(line(b"PARTIAL", 1024).await, Line::Text("PARTIAL".into()));
}

#[tokio::test]
async fn read_line_exceeds_limit_without_newline() {
    assert_eq!(line(b"AAAAAAAAAA", 5).await, Line::TooLong);
}

#[tokio::test]
async fn read_line_exactly_at_limit() {
    assert_eq!(line(b"ABCDE\r\n", 5).await, Line::Text("ABCDE".into()));
}

#[tokio::test]
async fn read_line_one_over_limit() {
    assert_eq!(line(b"ABCDEF\n", 5).await, Line::TooLong);
}

#[tokio::test]
async fn read_line_too_long_discards_through_newline() {
    let input = b"AAAAAAAAAAAAAAAAAAAA\r\nNOOP\r\n";
    let mut reader = BufReader::with_capacity(4, &input[..]);
    let mut buf = Vec::new();
    assert_eq!(
        read_line(&mut reader, &mut buf, 5).await.unwrap(),
        Line::TooLong
    );
    assert_eq!(
        read_line(&mut reader, &mut buf, 5).await.unwrap(),
        Line::Text("NOOP".into())
    );
}

#[tokio::test]
async fn read_line_multiple_lines() {
    let input = b"LINE1\r\nLINE2\r\n";
    let mut reader = BufReader::new(&input[..]);
    let mut buf = Vec::new();
    assert_eq!(
        read_line(&mut reader, &mut buf, 1024).await.unwrap(),
        Line::Text("LINE1".into())
    );
    assert_eq!(
        read_line(&mut reader, &mut buf, 1024).await.unwrap(),
        Line::Text("LINE2".into())
    );
    assert_eq!(
        read_line(&mut reader, &mut buf, 1024).await.unwrap(),
        Line::Eof
    );
}

// -- read_data --

async fn data(input: &[u8], max: usize) -> DataOutcome {
    let mut reader = BufReader::new(input);
    read_data(&mut reader, max, IDLE).await.unwrap()
}

#[tokio::test]
async fn read_data_simple_message() {
    assert_eq!(
        data(b"Subject: test\r\n\r\nHello world\r\n.\r\n", 10_000).await,
        DataOutcome::Message(b"Subject: test\r\n\r\nHello world\r\n".to_vec())
    );
}

#[tokio::test]
async fn read_data_removes_dot_stuffing() {
    assert_eq!(
        data(b"..leading dot\r\n..\r\n.\r\n", 10_000).await,
        DataOutcome::Message(b".leading dot\r\n.\r\n".to_vec())
    );
}

#[tokio::test]
async fn read_data_lf_line_endings_normalized() {
    assert_eq!(
        data(b"line1\nline2\n.\n", 10_000).await,
        DataOutcome::Message(b"line1\r\nline2\r\n".to_vec())
    );
}

#[tokio::test]
async fn read_data_empty_message() {
    assert_eq!(
        data(b".\r\n", 10_000).await,
        DataOutcome::Message(Vec::new())
    );
}

#[tokio::test]
async fn read_data_dot_in_middle_of_line_not_terminator() {
    assert_eq!(
        data(b"a.b\r\n.x\r\n.\r\n", 10_000).await,
        DataOutcome::Message(b"a.b\r\nx\r\n".to_vec())
    );
}

#[tokio::test]
async fn read_data_too_large_reads_through_terminator() {
    let mut input = Vec::new();
    for _ in 0..20 {
        input.extend_from_slice(b"AAAAA\r\n");
    }
    input.extend_from_slice(b".\r\nQUIT\r\n");
    let mut reader = BufReader::new(&input[..]);
    assert_eq!(
        read_data(&mut reader, 10, IDLE).await.unwrap(),
        DataOutcome::TooLarge
    );
    // The rest of the message was consumed; the next thing is the next command
    let mut rest = String::new();
    reader.read_to_string(&mut rest).await.unwrap();
    assert_eq!(rest, "QUIT\r\n");
}

#[tokio::test]
async fn read_data_without_newlines_is_bounded() {
    // 1 MB with no newline must not be buffered; max is 1000 bytes
    let mut input = vec![b'A'; 1 << 20];
    input.extend_from_slice(b"\r\n.\r\n");
    assert_eq!(data(&input, 1000).await, DataOutcome::TooLarge);
}

#[tokio::test]
async fn read_data_rejects_bare_cr() {
    assert_eq!(
        data(b"evil\r.\r\nmore\r\n.\r\n", 10_000).await,
        DataOutcome::BareCr
    );
}

#[tokio::test]
async fn read_data_eof_before_terminator() {
    let mut reader = BufReader::new(&b"line1\r\nline2\r\n"[..]);
    let err = read_data(&mut reader, 10_000, IDLE).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

// -- full sessions --

#[tokio::test]
async fn delivers_message_with_received_header_and_dot_stuffing() {
    let (addr, backend) = fake_backend(vec![], "250 queued").await;
    let out = run(
        test_config(&addr),
        b"EHLO client.example\r\n\
          MAIL FROM:<sender@example.org> SIZE=50\r\n\
          RCPT TO:<Alice@Tempy.Email>\r\n\
          DATA\r\n\
          Subject: hi\r\n\r\n..dotted\r\nbody\r\n.\r\n\
          QUIT\r\n",
    )
    .await;
    assert_eq!(
        codes(&out),
        ["220", "250", "250", "250", "354", "250", "221"]
    );

    let sent = String::from_utf8(backend.await.unwrap()).unwrap();
    assert!(sent.starts_with("Received: from client.example ([203.0.113.7])\r\n\tby mx.test (burngate) with ESMTP\r\n\tfor <alice@tempy.email>;"), "{sent}");
    assert!(
        sent.ends_with("Subject: hi\r\n\r\n..dotted\r\nbody\r\n.\r\n"),
        "{sent}"
    );
}

#[tokio::test]
async fn ehlo_advertises_configured_size() {
    let mut config = test_config("127.0.0.1:1");
    config.max_message_size = 12345;
    let out = run(config, b"EHLO c\r\nQUIT\r\n").await;
    assert!(out.contains("250-SIZE 12345\r\n"), "{out}");
    assert!(!out.contains("STARTTLS"));
}

#[tokio::test]
async fn helo_gets_single_line_reply() {
    let out = run(test_config("127.0.0.1:1"), b"HELO c\r\nQUIT\r\n").await;
    assert_eq!(
        out,
        "220 mx.test ESMTP burngate\r\n250 mx.test Hello c\r\n221 2.0.0 Bye\r\n"
    );
}

#[tokio::test]
async fn ehlo_requires_hostname() {
    let out = run(test_config("127.0.0.1:1"), b"EHLO\r\nQUIT\r\n").await;
    assert_eq!(codes(&out), ["220", "501", "221"]);
}

#[tokio::test]
async fn enforces_command_order() {
    let out = run(
        test_config("127.0.0.1:1"),
        b"MAIL FROM:<a@b.c>\r\n\
          EHLO c\r\n\
          RCPT TO:<alice@tempy.email>\r\n\
          DATA\r\n\
          MAIL FROM:<a@b.c>\r\n\
          MAIL FROM:<a@b.c>\r\n\
          QUIT\r\n",
    )
    .await;
    assert_eq!(
        codes(&out),
        ["220", "503", "250", "503", "503", "250", "503", "221"]
    );
}

#[tokio::test]
async fn validates_paths() {
    let out = run(
        test_config("127.0.0.1:1"),
        b"EHLO c\r\n\
          MAIL TO:<a@b.c>\r\n\
          MAIL FROM:<>\r\n\
          RCPT FROM:<alice@tempy.email>\r\n\
          RCPT TO:<alice>\r\n\
          RCPT TO:<alice@tempy.email>\r\n\
          QUIT\r\n",
    )
    .await;
    // MAIL TO is wrong; null sender is fine; RCPT FROM and missing @ are wrong
    assert_eq!(
        codes(&out),
        ["220", "250", "501", "250", "501", "501", "250", "221"]
    );
}

#[tokio::test]
async fn rejects_declared_size_over_limit() {
    let out = run(
        test_config("127.0.0.1:1"),
        b"EHLO c\r\nMAIL FROM:<a@b.c> SIZE=999999\r\nQUIT\r\n",
    )
    .await;
    assert_eq!(codes(&out), ["220", "250", "552", "221"]);
}

#[tokio::test]
async fn lookup_failure_is_temporary() {
    let out = run(
        test_config("127.0.0.1:1"),
        b"EHLO c\r\nMAIL FROM:<a@b.c>\r\nRCPT TO:<down@tempy.email>\r\nRCPT TO:<nobody@tempy.email>\r\nRCPT TO:<x@other.com>\r\nQUIT\r\n",
    )
    .await;
    assert_eq!(
        codes(&out),
        ["220", "250", "250", "451", "550", "550", "221"]
    );
}

#[tokio::test]
async fn deep_subdomains_accepted() {
    let lookup = FakeLookup(HashMap::from([("x@a.b.tempy.email", Verdict::Accept)]));
    let out = run_with(
        test_config("127.0.0.1:1"),
        lookup,
        None,
        b"EHLO c\r\nMAIL FROM:<a@b.c>\r\nRCPT TO:<x@a.b.tempy.email>\r\nQUIT\r\n",
    )
    .await;
    assert_eq!(codes(&out), ["220", "250", "250", "250", "221"]);
}

#[tokio::test]
async fn oversized_message_does_not_desync() {
    let mut config = test_config("127.0.0.1:1");
    config.max_message_size = 50;
    let mut script =
        b"EHLO c\r\nMAIL FROM:<a@b.c>\r\nRCPT TO:<alice@tempy.email>\r\nDATA\r\n".to_vec();
    for _ in 0..10 {
        script.extend_from_slice(b"RCPT TO:<bob@tempy.email>\r\n");
    }
    script.extend_from_slice(b".\r\nQUIT\r\n");
    let out = run(config, &script).await;
    // One 552 for the message, nothing for the body lines, then QUIT
    assert_eq!(
        codes(&out),
        ["220", "250", "250", "250", "354", "552", "221"]
    );
}

#[tokio::test]
async fn long_command_line_gets_500_and_session_continues() {
    let mut config = test_config("127.0.0.1:1");
    config.max_line_length = 20;
    let out = run(
        config,
        b"EHLO aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\nNOOP\r\nQUIT\r\n",
    )
    .await;
    assert_eq!(codes(&out), ["220", "500", "250", "221"]);
}

#[tokio::test]
async fn overlong_command_word_is_unrecognized() {
    let out = run(test_config("127.0.0.1:1"), b"STARTTLSX\r\nQUIT\r\n").await;
    assert_eq!(codes(&out), ["220", "502", "221"]);
}

#[tokio::test]
async fn backend_permanent_rejection_is_permanent() {
    let (addr, _backend) = fake_backend(vec![], "554 5.7.1 spam").await;
    let out = run(
        test_config(&addr),
        b"EHLO c\r\nMAIL FROM:<a@b.c>\r\nRCPT TO:<alice@tempy.email>\r\nDATA\r\nhi\r\n.\r\nQUIT\r\n",
    )
    .await;
    assert_eq!(
        codes(&out),
        ["220", "250", "250", "250", "354", "554", "221"]
    );
}

#[tokio::test]
async fn backend_temporary_recipient_rejection_defers_message() {
    let (addr, _backend) = fake_backend(vec!["250 OK", "450 4.2.0 busy"], "250 queued").await;
    let out = run(
        test_config(&addr),
        b"EHLO c\r\nMAIL FROM:<a@b.c>\r\nRCPT TO:<alice@tempy.email>\r\nRCPT TO:<bob@tempy.email>\r\nDATA\r\nhi\r\n.\r\nQUIT\r\n",
    )
    .await;
    assert_eq!(
        codes(&out),
        ["220", "250", "250", "250", "250", "354", "451", "221"]
    );
}

#[tokio::test]
async fn backend_partial_permanent_rejection_delivers_rest() {
    let (addr, backend) = fake_backend(vec!["550 no such user", "250 OK"], "250 queued").await;
    let out = run(
        test_config(&addr),
        b"EHLO c\r\nMAIL FROM:<a@b.c>\r\nRCPT TO:<alice@tempy.email>\r\nRCPT TO:<bob@tempy.email>\r\nDATA\r\nhi\r\n.\r\nQUIT\r\n",
    )
    .await;
    assert_eq!(
        codes(&out),
        ["220", "250", "250", "250", "250", "354", "250", "221"]
    );
    let sent = String::from_utf8(backend.await.unwrap()).unwrap();
    // Two recipients: the Received header must not name either
    assert!(!sent.contains("for <"), "{sent}");
}

#[tokio::test]
async fn backend_down_is_temporary() {
    // Port 1 on localhost: connection refused
    let out = run(
        test_config("127.0.0.1:1"),
        b"EHLO c\r\nMAIL FROM:<a@b.c>\r\nRCPT TO:<alice@tempy.email>\r\nDATA\r\nhi\r\n.\r\nQUIT\r\n",
    )
    .await;
    assert_eq!(
        codes(&out),
        ["220", "250", "250", "250", "354", "451", "221"]
    );
}

#[tokio::test]
async fn starttls_not_offered_without_loaded_cert() {
    let tls = TlsConfig::new("/nonexistent/cert.pem", "/nonexistent/key.pem");
    let out = run_with(
        test_config("127.0.0.1:1"),
        lookup(),
        Some(tls),
        b"EHLO c\r\nSTARTTLS\r\nQUIT\r\n",
    )
    .await;
    assert!(
        !out.contains("250-STARTTLS") && !out.contains("250 STARTTLS"),
        "{out}"
    );
    assert_eq!(codes(&out), ["220", "250", "502", "221"]);
}

#[tokio::test(start_paused = true)]
async fn idle_client_gets_421() {
    let (mut client, server) = tokio::io::duplex(1 << 16);
    let mut config = test_config("127.0.0.1:1");
    config.idle_timeout_secs = 5;
    let session = tokio::spawn(handle_session(
        server,
        "192.0.2.1:1".parse().unwrap(),
        Arc::new(config),
        lookup(),
        None,
        Arc::new(Metrics::new()),
    ));
    client.write_all(b"EHLO c\r\n").await.unwrap();
    // Don't send anything else; paused time auto-advances past the idle timeout
    let mut out = String::new();
    client.read_to_string(&mut out).await.unwrap();
    session.await.unwrap();
    assert!(
        out.ends_with("421 4.4.2 mx.test Idle timeout, closing connection\r\n"),
        "{out}"
    );
}

// -- small helpers --

#[test]
fn rfc5322_date_formats() {
    let t = std::time::UNIX_EPOCH + Duration::from_secs(1_795_640_709);
    assert_eq!(rfc5322_date(t), "Wed, 25 Nov 2026 21:05:09 +0000");
    assert_eq!(
        rfc5322_date(std::time::UNIX_EPOCH),
        "Thu, 01 Jan 1970 00:00:00 +0000"
    );
}

#[test]
fn received_header_uses_ipv6_literal() {
    let h = received_header("h", "2001:db8::1".parse().unwrap(), "mx", "ESMTPS", &[]);
    assert!(h.starts_with(
        "Received: from h ([IPv6:2001:db8::1])\r\n\tby mx (burngate) with ESMTPS;\r\n\t"
    ));
}

#[test]
fn helo_is_sanitized() {
    assert_eq!(sanitize_helo("evil(x);\\host extra"), "evilxhost");
}

// -- STARTTLS --

const CERT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test-cert.pem");
const KEY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test-key.pem");

fn tls_client() -> tokio_rustls::TlsConnector {
    use rustls_pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pki_types::CertificateDer::pem_file_iter(CERT).unwrap() {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

async fn read_reply<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> String {
    let mut all = String::new();
    loop {
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        all.push_str(&line);
        if line.len() < 4 || line.as_bytes()[3] != b'-' {
            return all;
        }
    }
}

#[tokio::test]
async fn starttls_upgrades_and_discards_pipelined_plaintext() {
    let tls = TlsConfig::new(CERT, KEY);
    assert!(tls.is_ready());
    let (client, server) = tokio::io::duplex(1 << 16);
    let session = tokio::spawn(handle_session(
        server,
        "192.0.2.1:1".parse().unwrap(),
        Arc::new(test_config("127.0.0.1:1")),
        lookup(),
        Some(tls),
        Arc::new(Metrics::new()),
    ));

    let mut client = BufReader::new(client);
    assert!(read_reply(&mut client).await.starts_with("220 "));
    client.get_mut().write_all(b"EHLO c\r\n").await.unwrap();
    assert!(read_reply(&mut client).await.contains("250 STARTTLS\r\n"));

    // Pipeline a plaintext command after STARTTLS: it must NOT be executed
    client
        .get_mut()
        .write_all(b"STARTTLS\r\nMAIL FROM:<injected@evil>\r\n")
        .await
        .unwrap();
    assert!(read_reply(&mut client).await.starts_with("220 2.0.0"));

    let stream = client.into_inner();
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let tls_stream = tls_client().connect(name, stream).await.unwrap();
    let mut tls_stream = BufReader::new(tls_stream);

    // EHLO state was reset: MAIL before EHLO is refused, so the injected
    // MAIL above was dropped rather than run inside the TLS session.
    tls_stream
        .get_mut()
        .write_all(b"MAIL FROM:<a@b.c>\r\n")
        .await
        .unwrap();
    assert!(read_reply(&mut tls_stream).await.starts_with("503 "));
    tls_stream.get_mut().write_all(b"EHLO c\r\n").await.unwrap();
    let ehlo = read_reply(&mut tls_stream).await;
    assert!(ehlo.starts_with("250-"), "{ehlo}");
    assert!(!ehlo.contains("STARTTLS"), "{ehlo}");
    tls_stream.get_mut().write_all(b"QUIT\r\n").await.unwrap();
    assert!(read_reply(&mut tls_stream).await.starts_with("221 "));
    session.await.unwrap();
}

#[test]
fn tls_reload_keeps_previous_cert_on_failure() {
    let dir = std::env::temp_dir().join(format!("burngate-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let (cert_s, key_s) = (cert.to_str().unwrap(), key.to_str().unwrap());

    // Nothing there yet: not ready, STARTTLS won't be offered
    let tls = TlsConfig::new(cert_s, key_s);
    assert!(!tls.is_ready());

    // Cert appears (e.g. first issuance): reload picks it up
    std::fs::copy(CERT, &cert).unwrap();
    std::fs::copy(KEY, &key).unwrap();
    assert!(tls.reload());
    assert!(tls.is_ready());

    // Broken file during renewal: reload fails, old cert stays in use
    std::fs::write(&cert, "garbage").unwrap();
    assert!(!tls.reload());
    assert!(tls.is_ready());

    std::fs::remove_dir_all(&dir).unwrap();
}
