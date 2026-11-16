//! Minimal POP3 client (RFC 1939 + STLS from RFC 2595, UIDL, CAPA).
//!
//! Fork addition — upstream Skim is IMAP-only. POP3 is a download protocol:
//! there are no folders and no flags, so this client does exactly four things
//! after login — list the maildrop (`UIDL`), size it (`LIST`), download a
//! message (`RETR`) and leave (`QUIT`). Nothing is ever deleted from the
//! server.
//!
//! Port 995 gets implicit TLS from the first byte; every other port starts in
//! plaintext and must upgrade with STLS before the password is sent — a server
//! that can't is refused rather than handed credentials in the clear.

use crate::error::{Result, SkimError};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// One command/response round trip that carries no message data.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Silence allowed between two lines of a message being downloaded. A gap
/// leash rather than a total: a 30 MB attachment on a slow link is legitimate.
const LINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// A status line longer than this is not POP3 (RFC 2449 caps it at 512).
const MAX_STATUS_LINE: usize = 4096;
/// Refuse to buffer a single message beyond this; nothing sane is larger.
const MAX_MESSAGE_BYTES: usize = 150 * 1024 * 1024;
/// The maildrop listing: about 400,000 entries at RFC 1939's longest ids.
const MAX_LISTING_BYTES: usize = 32 * 1024 * 1024;
/// The capability list.
const MAX_CAPA_BYTES: usize = 64 * 1024;
/// A whole download, however steadily the server trickles it: the per-line
/// leash alone would let a server keep one RETR going indefinitely, and the
/// account's outgoing mail waits behind the pass.
const RETR_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// A whole listing or capability response.
const LISTING_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// One entry of the maildrop listing: the session-local message number and the
/// server's stable unique id for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub number: u32,
    pub uidl: String,
}

pub struct Pop3<S> {
    io: BufReader<S>,
}

/// Port 995 is POP3S (TLS from the first byte); anything else upgrades via STLS.
fn uses_implicit_tls(port: u16) -> bool {
    port == 995
}

fn proto(message: impl Into<String>) -> SkimError {
    SkimError::other("pop3", message)
}

fn network(message: impl Into<String>) -> SkimError {
    SkimError::other("network", message)
}

impl<S: Stream> Pop3<S> {
    /// Wrap an already-connected stream. The greeting has not been read yet.
    pub fn new(stream: S) -> Self {
        Self {
            io: BufReader::new(stream),
        }
    }

    fn into_inner(self) -> S {
        // Dropping the read buffer is safe where this is used (STLS): after the
        // +OK the server stays silent until the client opens the handshake.
        self.io.into_inner()
    }

    /// Read one CRLF-terminated line of at most `max` bytes, without its line
    /// ending. The cap applies while reading: a server that never sends a line
    /// break must not be able to fill memory before the length is checked.
    async fn read_line(&mut self, leash: std::time::Duration, max: usize) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        // Room for the CRLF on top of `max`, plus one byte to notice overflow.
        let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(3);
        let mut capped = (&mut self.io).take(limit);
        let n = tokio::time::timeout(leash, capped.read_until(b'\n', &mut line))
            .await
            .map_err(|_| network("the POP3 server stopped responding"))?
            .map_err(|e| network(format!("POP3 connection failed: {e}")))?;
        if n == 0 {
            return Err(network("the POP3 server closed the connection"));
        }
        if !line.ends_with(b"\n") && line.len() as u64 >= limit {
            return Err(proto("the server sent an oversized line"));
        }
        if line.ends_with(b"\n") {
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
        }
        Ok(line)
    }

    /// Read a status line and return the text after `+OK`; `-ERR` is an error.
    async fn read_status(&mut self) -> Result<String> {
        let line = self.read_line(COMMAND_TIMEOUT, MAX_STATUS_LINE).await?;
        if line.len() > MAX_STATUS_LINE {
            return Err(proto("the server sent an oversized status line"));
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        if let Some(rest) = text.strip_prefix("+OK") {
            Ok(rest.trim().to_string())
        } else if let Some(rest) = text.strip_prefix("-ERR") {
            Err(proto(rest.trim().to_string()))
        } else {
            Err(proto(format!("unexpected reply: {text}")))
        }
    }

    async fn send(&mut self, command: &str) -> Result<()> {
        let io = self.io.get_mut();
        let write = async {
            io.write_all(command.as_bytes()).await?;
            io.write_all(b"\r\n").await?;
            io.flush().await
        };
        tokio::time::timeout(COMMAND_TIMEOUT, write)
            .await
            .map_err(|_| network("the POP3 server stopped accepting data"))?
            .map_err(|e| network(format!("POP3 connection failed: {e}")))
    }

    async fn command(&mut self, command: &str) -> Result<String> {
        self.send(command).await?;
        self.read_status().await
    }

    /// Read a multi-line response body of at most `max` bytes up to the lone
    /// `.` terminator, undoing dot-stuffing. Lines are rejoined with CRLF, as
    /// they were on the wire. The whole read must finish within `deadline`.
    async fn read_multiline(
        &mut self,
        max: usize,
        deadline: std::time::Duration,
    ) -> Result<Vec<u8>> {
        let read = async {
            let mut out = Vec::new();
            loop {
                // One more byte than the room left, for a dot-stuffed line.
                let room = max.saturating_sub(out.len()).saturating_add(1);
                let line = self.read_line(LINE_TIMEOUT, room).await?;
                if line == b"." {
                    return Ok(out);
                }
                let data = if line.starts_with(b".") {
                    &line[1..]
                } else {
                    &line[..]
                };
                if out.len() + data.len() + 2 > max {
                    return Err(proto("message is too large to download"));
                }
                out.extend_from_slice(data);
                out.extend_from_slice(b"\r\n");
            }
        };
        tokio::time::timeout(deadline, read)
            .await
            .map_err(|_| network("the POP3 server is sending too slowly"))?
    }

    pub async fn read_greeting(&mut self) -> Result<()> {
        self.read_status().await.map(|_| ())
    }

    /// Capability names, upper-cased (`STLS`, `UIDL`, `USER`, …). A server
    /// without CAPA answers `-ERR`; that reads as "no capabilities".
    pub async fn capabilities(&mut self) -> Result<Vec<String>> {
        self.send("CAPA").await?;
        match self.read_status().await {
            Ok(_) => {}
            Err(e) if e.code() == "pop3" => return Ok(Vec::new()),
            Err(e) => return Err(e),
        }
        let body = self
            .read_multiline(MAX_CAPA_BYTES, LISTING_DEADLINE)
            .await?;
        Ok(String::from_utf8_lossy(&body)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(|c| c.to_ascii_uppercase())
            .collect())
    }

    /// USER/PASS. A refusal is an `auth` error so the UI can say "wrong
    /// password" rather than show protocol noise.
    pub async fn login(&mut self, user: &str, password: &str) -> Result<()> {
        if user.contains(['\r', '\n']) || password.contains(['\r', '\n']) {
            return Err(SkimError::other(
                "auth",
                "sign-in failed: invalid characters",
            ));
        }
        let as_auth = |e: SkimError| match e.code() {
            "pop3" => SkimError::other("auth", format!("sign-in failed: {e}")),
            _ => e,
        };
        self.command(&format!("USER {user}"))
            .await
            .map_err(as_auth)?;
        self.command(&format!("PASS {password}"))
            .await
            .map_err(as_auth)?;
        Ok(())
    }

    /// The whole maildrop as `(message number, unique id)`.
    pub async fn uidl(&mut self) -> Result<Vec<Listing>> {
        self.command("UIDL").await.map_err(|e| match e.code() {
            "pop3" => SkimError::other(
                "pop3_no_uidl",
                "this POP3 server does not support UIDL, which is needed to tell \
                 new mail from mail already downloaded",
            ),
            _ => e,
        })?;
        let body = self
            .read_multiline(MAX_LISTING_BYTES, LISTING_DEADLINE)
            .await?;
        Ok(parse_uidl(&body))
    }

    /// Download one message in full.
    pub async fn retr(&mut self, number: u32) -> Result<Vec<u8>> {
        self.command(&format!("RETR {number}")).await?;
        self.read_multiline(MAX_MESSAGE_BYTES, RETR_DEADLINE).await
    }

    /// End the session. Skim never issues DELE, so QUIT removes nothing.
    pub async fn quit(&mut self) {
        let _ = self.command("QUIT").await;
    }
}

fn parse_uidl(body: &[u8]) -> Vec<Listing> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let number = parts.next()?.parse().ok()?;
            let uidl = parts.next()?.to_string();
            Some(Listing { number, uidl })
        })
        .collect()
}

pub type Session = Pop3<Box<dyn Stream>>;

async fn tcp_connect(host: &str, port: u16) -> Result<TcpStream> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| network(format!("connection to {host}:{port} timed out")))?
        .map_err(|e| network(format!("cannot reach {host}:{port}: {e}")))?;
    tcp.set_nodelay(true).ok();
    Ok(tcp)
}

async fn tls_handshake(
    host: &str,
    port: u16,
    tcp: TcpStream,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| SkimError::other("tls", e.to_string()))?;
    let connector = TlsConnector::from(Arc::new(crate::net::tls_config()?));
    tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| SkimError::other("tls", format!("TLS handshake with {host} timed out")))?
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("InvalidContentType") || msg.contains("corrupt message") {
                SkimError::other(
                    "plaintext_port",
                    format!(
                        "{host}:{port} answered without encryption — this doesn't look \
                         like a TLS port. POP3 normally uses port 995, or 110 with STLS."
                    ),
                )
            } else {
                SkimError::other("tls", format!("TLS handshake with {host} failed: {msg}"))
            }
        })
}

/// Connect, secure the channel, and consume the greeting.
pub async fn connect(host: &str, port: u16) -> Result<Session> {
    let tcp = tcp_connect(host, port).await?;
    if uses_implicit_tls(port) {
        let tls = tls_handshake(host, port, tcp).await?;
        let mut session: Session = Pop3::new(Box::new(tls));
        session.read_greeting().await?;
        return Ok(session);
    }
    // Plaintext phase: greeting, CAPA and STLS only. Nothing sensitive is sent
    // before the handshake completes.
    let mut plain = Pop3::new(tcp);
    plain.read_greeting().await?;
    let caps = plain.capabilities().await?;
    if !caps.iter().any(|c| c == "STLS") {
        return Err(SkimError::other(
            "starttls_unsupported",
            format!(
                "{host}:{port} does not offer encryption (STLS). Skim will not send \
                 a password over an unencrypted connection — try port 995."
            ),
        ));
    }
    plain.command("STLS").await?;
    let tls = tls_handshake(host, port, plain.into_inner()).await?;
    // No second greeting after STLS.
    Ok(Pop3::new(Box::new(tls)))
}

/// Connect and sign in.
pub async fn login(host: &str, port: u16, user: &str, password: &str) -> Result<Session> {
    let mut session = connect(host, port).await?;
    session.login(user, password).await?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// A scripted server: for each `(expected command, reply)` pair it reads
    /// one line, checks it, and writes the reply. `greeting` goes out first.
    async fn scripted(
        greeting: &'static str,
        script: Vec<(&'static str, &'static str)>,
    ) -> Pop3<tokio::io::DuplexStream> {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            server.write_all(greeting.as_bytes()).await.unwrap();
            for (expect, reply) in script {
                let mut got = Vec::new();
                let mut byte = [0u8; 1];
                while !got.ends_with(b"\r\n") {
                    if server.read(&mut byte).await.unwrap() == 0 {
                        return;
                    }
                    got.push(byte[0]);
                }
                assert_eq!(String::from_utf8_lossy(&got).trim_end(), expect);
                server.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        Pop3::new(client)
    }

    #[tokio::test]
    async fn logs_in_and_lists_the_maildrop() {
        let mut pop = scripted(
            "+OK ready\r\n",
            vec![
                ("USER me", "+OK\r\n"),
                ("PASS secret", "+OK logged in\r\n"),
                ("UIDL", "+OK\r\n1 abc\r\n2 def-2\r\n.\r\n"),
            ],
        )
        .await;
        pop.read_greeting().await.unwrap();
        pop.login("me", "secret").await.unwrap();
        let list = pop.uidl().await.unwrap();
        assert_eq!(
            list,
            vec![
                Listing {
                    number: 1,
                    uidl: "abc".into()
                },
                Listing {
                    number: 2,
                    uidl: "def-2".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_password_is_an_auth_error() {
        let mut pop = scripted(
            "+OK ready\r\n",
            vec![
                ("USER me", "+OK\r\n"),
                ("PASS nope", "-ERR bad password\r\n"),
            ],
        )
        .await;
        pop.read_greeting().await.unwrap();
        let e = pop.login("me", "nope").await.unwrap_err();
        assert_eq!(e.code(), "auth");
        assert!(e.to_string().contains("bad password"), "{e}");
    }

    #[tokio::test]
    async fn retr_undoes_dot_stuffing_and_keeps_line_endings() {
        let mut pop = scripted(
            "+OK ready\r\n",
            vec![(
                "RETR 3",
                "+OK 42 octets\r\nSubject: hi\r\n\r\n..leading dot\r\n.. \r\nlast\r\n.\r\n",
            )],
        )
        .await;
        pop.read_greeting().await.unwrap();
        let raw = pop.retr(3).await.unwrap();
        assert_eq!(
            String::from_utf8(raw).unwrap(),
            "Subject: hi\r\n\r\n.leading dot\r\n. \r\nlast\r\n"
        );
    }

    #[tokio::test]
    async fn a_server_without_capa_has_no_capabilities() {
        let mut pop = scripted("+OK ready\r\n", vec![("CAPA", "-ERR unknown command\r\n")]).await;
        pop.read_greeting().await.unwrap();
        assert!(pop.capabilities().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn capabilities_are_upper_cased_names() {
        let mut pop = scripted(
            "+OK ready\r\n",
            vec![("CAPA", "+OK\r\nstls\r\nUIDL\r\nSASL PLAIN LOGIN\r\n.\r\n")],
        )
        .await;
        pop.read_greeting().await.unwrap();
        assert_eq!(pop.capabilities().await.unwrap(), ["STLS", "UIDL", "SASL"]);
    }

    #[tokio::test]
    async fn a_server_without_uidl_gets_its_own_error_code() {
        let mut pop = scripted("+OK ready\r\n", vec![("UIDL", "-ERR not supported\r\n")]).await;
        pop.read_greeting().await.unwrap();
        assert_eq!(pop.uidl().await.unwrap_err().code(), "pop3_no_uidl");
    }

    #[tokio::test]
    async fn a_dropped_connection_is_a_network_error() {
        let (client, server) = tokio::io::duplex(1024);
        drop(server);
        let mut pop = Pop3::new(client);
        assert_eq!(pop.read_greeting().await.unwrap_err().code(), "network");
    }

    #[tokio::test]
    async fn line_breaks_in_credentials_are_refused_before_sending() {
        let mut pop = scripted("+OK ready\r\n", vec![]).await;
        pop.read_greeting().await.unwrap();
        let e = pop.login("me\r\nDELE 1", "x").await.unwrap_err();
        assert_eq!(e.code(), "auth");
    }

    #[tokio::test]
    async fn an_endless_line_is_refused_without_reading_it_all() {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            // A "greeting" that never ends: refused once it passes the cap,
            // not after the server stops sending.
            let chunk = vec![b'A'; 16 * 1024];
            while server.write_all(&chunk).await.is_ok() {}
        });
        let mut pop = Pop3::new(client);
        let err = pop.read_greeting().await.unwrap_err();
        assert_eq!(err.code(), "pop3");
    }

    #[tokio::test]
    async fn a_body_line_is_capped_by_the_room_left() {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            server.write_all(b"short\r\n").await.unwrap();
            server.write_all(&[b'B'; 100]).await.unwrap();
            server.write_all(b"\r\n.\r\n").await.unwrap();
        });
        let mut pop = Pop3::new(client);
        let err = pop
            .read_multiline(64, std::time::Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "pop3");
    }

    #[tokio::test]
    async fn a_body_that_fits_its_cap_is_read() {
        let (client, mut server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            server.write_all(b"..dot\r\nend\r\n.\r\n").await.unwrap();
        });
        let mut pop = Pop3::new(client);
        let body = pop
            .read_multiline(12, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(body, b".dot\r\nend\r\n");
    }

    #[tokio::test]
    async fn a_trickling_server_runs_into_the_deadline() {
        let (client, mut server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            loop {
                if server.write_all(b"x\r\n").await.is_err() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        });
        let mut pop = Pop3::new(client);
        let err = pop
            .read_multiline(MAX_MESSAGE_BYTES, std::time::Duration::from_millis(200))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "network");
    }

    #[test]
    fn only_995_is_implicit_tls() {
        assert!(uses_implicit_tls(995));
        assert!(!uses_implicit_tls(110));
        assert!(!uses_implicit_tls(993));
    }

    #[test]
    fn malformed_uidl_lines_are_skipped() {
        let list = parse_uidl(b"1 a\r\nnonsense\r\n2\r\n3 c extra\r\n");
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].uidl, "c");
    }
}
