//! A minimal in-process SMTP server for tests (no network beyond loopback, no
//! TLS).
//!
//! It speaks just enough of RFC 5321 for `lettre` to complete a submission —
//! greeting, `EHLO`, `MAIL FROM`, `RCPT TO`, `DATA`, `QUIT` — and it records
//! every message it accepts, which is the only thing the tests actually assert
//! on. "Exactly one message reached the server" is a claim about *this*
//! recording; a fake `SmtpSender` could not make it, because the thing it
//! would be faking is the very step a duplicate would happen at.
//!
//! Its replies are programmable per stage, so the 4xx-versus-5xx
//! classification can be driven against a server genuinely answering `451` or
//! `550` rather than against a hand-built `lettre::Error`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// How a mock server answers each stage of a submission.
#[derive(Debug, Clone)]
pub struct MockSmtpConfig {
    /// Reply to `RCPT TO`.
    pub rcpt_reply: String,
    /// Reply after the message body's terminating `.`.
    pub data_reply: String,
    /// Close the connection abruptly instead of replying to `DATA`'s
    /// terminator — the shape "the network went away mid-send" takes.
    pub drop_after_data: bool,
}

impl Default for MockSmtpConfig {
    fn default() -> Self {
        Self {
            rcpt_reply: "250 2.1.5 Ok".to_owned(),
            data_reply: "250 2.0.0 Ok: queued".to_owned(),
            drop_after_data: false,
        }
    }
}

/// A running mock SMTP server.
pub struct MockSmtp {
    addr: SocketAddr,
    accepted: Arc<Mutex<Vec<Vec<u8>>>>,
    cancel: CancellationToken,
}

impl MockSmtp {
    /// Start one on an ephemeral loopback port.
    pub async fn start(config: MockSmtpConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let accepted: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();

        tokio::spawn({
            let accepted = Arc::clone(&accepted);
            let cancel = cancel.clone();
            async move {
                loop {
                    let stream = tokio::select! {
                        () = cancel.cancelled() => return,
                        accepted = listener.accept() => match accepted {
                            Ok((stream, _)) => stream,
                            Err(_) => return,
                        },
                    };
                    let accepted = Arc::clone(&accepted);
                    let config = config.clone();
                    // One task per connection: lettre's pool keeps a
                    // connection open across sends, so a serial accept loop
                    // would deadlock the second concurrent worker.
                    tokio::spawn(async move {
                        let _ = serve(stream, config, accepted).await;
                    });
                }
            }
        });

        Ok(Self {
            addr,
            accepted,
            cancel,
        })
    }

    /// The port to point a sender at.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Every message body this server accepted, in order.
    pub fn accepted(&self) -> Vec<Vec<u8>> {
        self.accepted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many messages it accepted.
    pub fn accepted_count(&self) -> usize {
        self.accepted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl Drop for MockSmtp {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn serve(
    stream: TcpStream,
    config: MockSmtpConfig,
    accepted: Arc<Mutex<Vec<Vec<u8>>>>,
) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    write.write_all(b"220 mock.rmail.test ESMTP\r\n").await?;

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let command = line.trim_end();
        let upper = command.to_ascii_uppercase();

        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            // No STARTTLS and no AUTH advertised: the tests run plaintext,
            // and an advertised AUTH would make lettre negotiate one.
            write
                .write_all(b"250-mock.rmail.test\r\n250 8BITMIME\r\n")
                .await?;
        } else if upper.starts_with("MAIL FROM") {
            write.write_all(b"250 2.1.0 Ok\r\n").await?;
        } else if upper.starts_with("RCPT TO") {
            write
                .write_all(format!("{}\r\n", config.rcpt_reply).as_bytes())
                .await?;
        } else if upper.starts_with("DATA") {
            write
                .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                .await?;
            let body = read_body(&mut reader).await?;
            if config.drop_after_data {
                // Recorded before the drop: the server *did* receive the
                // message, which is precisely the state a duplicate-delivery
                // bug would exploit.
                push(&accepted, body);
                return Ok(());
            }
            if config.data_reply.starts_with('2') {
                push(&accepted, body);
            }
            write
                .write_all(format!("{}\r\n", config.data_reply).as_bytes())
                .await?;
        } else if upper.starts_with("QUIT") {
            write.write_all(b"221 2.0.0 Bye\r\n").await?;
            return Ok(());
        } else if upper.starts_with("RSET") || upper.starts_with("NOOP") {
            write.write_all(b"250 2.0.0 Ok\r\n").await?;
        } else {
            write
                .write_all(b"502 5.5.2 Command not implemented\r\n")
                .await?;
        }
    }
}

/// Read a `DATA` body up to the terminating `.` line, undoing dot-stuffing.
async fn read_body(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = read_line_bytes(reader, &mut line).await?;
        if read == 0 {
            return Ok(body);
        }
        let mut trimmed: &[u8] = &line;
        if let Some(rest) = trimmed.strip_suffix(b"\n") {
            trimmed = rest;
        }
        if let Some(rest) = trimmed.strip_suffix(b"\r") {
            trimmed = rest;
        }
        if trimmed == b"." {
            return Ok(body);
        }
        // RFC 5321 §4.5.2: a leading '.' on a body line is doubled on the
        // wire. Undoing it here is what lets a test compare the accepted
        // bytes against the octets it handed the sender.
        let unstuffed = trimmed.strip_prefix(b".").unwrap_or(trimmed);
        body.extend_from_slice(unstuffed);
        body.extend_from_slice(b"\r\n");
    }
}

async fn read_line_bytes(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    out: &mut Vec<u8>,
) -> std::io::Result<usize> {
    let mut byte = [0u8; 1];
    let mut read = 0usize;
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Ok(read);
        }
        read += 1;
        out.push(byte[0]);
        if byte[0] == b'\n' {
            return Ok(read);
        }
    }
}

fn push(accepted: &Arc<Mutex<Vec<Vec<u8>>>>, body: Vec<u8>) {
    accepted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(body);
}
