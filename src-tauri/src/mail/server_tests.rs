//! End-to-end checks against a real mail server (fork addition).
//!
//! Ignored by default: they need a server. Point them at one with
//!
//! ```text
//! SKIM_TEST_MAIL_HOST   host name the server's certificate is issued for
//! SKIM_TEST_MAIL_USER   a mailbox on it
//! SKIM_TEST_MAIL_PASS   its password
//! SKIM_TEST_MAIL_SEEDED how many messages the mailbox starts with
//! SKIM_TEST_ROGUE_HOST  optional: a host serving TLS on port 995 with a
//!                       certificate this machine does NOT trust
//! SKIM_TEST_SKIP_SMTP   optional: set to skip sending. Needed under Wine 9,
//!                       whose CertCreateCertificateChainEngine rejects the
//!                       Windows 8+ config the SMTP library's verifier builds;
//!                       real Windows accepts it.
//! ```
//!
//! and run `cargo test --lib -- --ignored real_mail_server --nocapture`.
//!
//! The server must offer POP3 on 110 (STLS) and 995, IMAP on 143 (STARTTLS)
//! and 993, and SMTP submission on 587 (STARTTLS) and 465, all with a
//! certificate this machine trusts, and must deliver what is submitted to it
//! back into the same mailbox. Everything runs in one test, in order, because
//! later steps depend on what earlier ones left in the mailbox.

use crate::db::models::Account;
use crate::db::{accounts as db_accounts, Db};
use crate::mail::{imap_client, local_store, pop3, pop3_client, smtp};

struct Server {
    host: String,
    user: String,
    pass: String,
    seeded: usize,
    rogue_host: Option<String>,
    skip_smtp: bool,
}

fn server() -> Option<Server> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    Some(Server {
        host: var("SKIM_TEST_MAIL_HOST")?,
        user: var("SKIM_TEST_MAIL_USER")?,
        pass: var("SKIM_TEST_MAIL_PASS")?,
        seeded: var("SKIM_TEST_MAIL_SEEDED")?.parse().ok()?,
        rogue_host: var("SKIM_TEST_ROGUE_HOST"),
        skip_smtp: var("SKIM_TEST_SKIP_SMTP").is_some(),
    })
}

fn account(s: &Server, smtp_port: u16, smtp_security: &str) -> Account {
    Account {
        id: "pop".into(),
        email: format!("{}@{}", s.user, s.host),
        imap_user: Some(s.user.clone()),
        display_name: Some("PCG Test".into()),
        provider: local_store::POP3.into(),
        imap_host: s.host.clone(),
        imap_port: 995,
        smtp_host: s.host.clone(),
        smtp_port,
        smtp_security: smtp_security.into(),
        auth_kind: "password".into(),
        signature: None,
    }
}

fn step(name: &str) {
    println!("--- {name}");
}

async fn pass(db: &Db, dir: &std::path::Path, acc: &Account, pw: &str, batch: usize) -> pop3::Pass {
    pop3::download_pass(db, dir, acc, pw, batch, |_, _, _| {})
        .await
        .unwrap_or_else(|p| {
            panic!(
                "POP3 pass failed after {} messages: {}",
                p.stored.len(),
                p.error
            )
        })
}

fn query<T: rusqlite::types::FromSql + Send + 'static>(db: &Db, sql: &str) -> T {
    let sql = sql.to_string();
    db.with(move |conn| conn.query_row(&sql, [], |r| r.get(0)))
        .unwrap()
}

/// Submit a message and wait for the server to deliver it back to us.
async fn submit_and_collect(
    s: &Server,
    db: &Db,
    dir: &std::path::Path,
    port: u16,
    security: &str,
    subject: &str,
) -> pop3::Pass {
    let acc = account(s, port, security);
    let raw = format!(
        "From: PCG Test <{0}>\r\nTo: {0}\r\nSubject: {subject}\r\n\
         Message-ID: <{port}.{security}@pcg.test>\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n寄件測試 via {security}:{port}\r\n",
        acc.email
    );
    smtp::send(
        &acc,
        &imap_client::Credentials::Password(s.pass.clone()),
        raw.as_bytes(),
    )
    .await
    .unwrap_or_else(|e| panic!("SMTP {security}:{port} failed: {e}"));
    for _ in 0..20 {
        let p = pass(db, dir, &acc, &s.pass, 50).await;
        if !p.stored.is_empty() {
            return p;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("the message sent over {security}:{port} never arrived");
}

#[tokio::test]
#[ignore = "needs a real mail server, see the module docs"]
async fn real_mail_server() {
    let Some(s) = server() else {
        println!("SKIM_TEST_MAIL_* not set; nothing to do");
        return;
    };

    step("POP3 over implicit TLS (995) and STLS (110)");
    for port in [995, 110] {
        let mut session = pop3_client::login(&s.host, port, &s.user, &s.pass)
            .await
            .unwrap_or_else(|e| panic!("POP3 login on {port} failed: {e}"));
        let listing = session.uidl().await.unwrap();
        assert_eq!(listing.len(), s.seeded, "port {port}");
        session.quit().await;
        println!("    {port}: logged in, {} messages listed", listing.len());
    }

    step("a wrong password is reported as such");
    let e = pop3_client::login(&s.host, 995, &s.user, "wrong-password")
        .await
        .err()
        .expect("must be refused");
    assert_eq!(e.code(), "auth", "{e}");
    println!("    refused: {e}");

    step("a certificate not issued for the name connected to is refused");
    let e = pop3_client::login("127.0.0.1", 995, &s.user, &s.pass)
        .await
        .err()
        .expect("must be refused");
    assert_eq!(e.code(), "tls", "{e}");
    println!("    refused: {e}");

    if let Some(rogue) = &s.rogue_host {
        step("a certificate from an untrusted issuer is refused");
        let e = pop3_client::login(rogue, 995, &s.user, &s.pass)
            .await
            .err()
            .expect("must be refused");
        assert_eq!(e.code(), "tls", "{e}");
        println!("    refused: {e}");
    }

    step("POP3 download, two at a time, into a local inbox");
    let dir = std::env::temp_dir().join(format!("skim-server-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open(&dir.join("skim.db")).unwrap();
    let acc = account(&s, 587, "starttls");
    db.with(|conn| db_accounts::insert(conn, &acc)).unwrap();

    let mut got = 0;
    let mut passes = 0;
    loop {
        let p = pass(&db, &dir, &acc, &s.pass, 2).await;
        passes += 1;
        assert!(!p.initial_done, "the first download is history, not news");
        got += p.stored.len();
        println!(
            "    pass {passes}: {} stored, more: {}",
            p.stored.len(),
            p.more
        );
        if !p.more {
            break;
        }
        assert!(passes < 10, "the download never finished");
    }
    assert_eq!(got, s.seeded);
    let again = pass(&db, &dir, &acc, &s.pass, 2).await;
    assert!(again.stored.is_empty(), "nothing is downloaded twice");
    assert!(again.initial_done);
    println!("    next pass: nothing new, first download marked complete");

    step("what landed in the inbox");
    let rows: Vec<(String, String, i64, i64)> = db
        .with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT COALESCE(subject, ''), COALESCE(from_name, from_addr, ''), body_state,
                        (SELECT count(*) FROM attachments a WHERE a.message_id = m.id)
                 FROM messages m ORDER BY date",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    for (subject, from, state, files) in &rows {
        println!("    {subject:<20} from {from:<16} body_state={state} attachments={files}");
        assert_eq!(*state, 1, "{subject}: the body is stored with the message");
    }
    let details: Vec<(String, String)> = db
        .with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT COALESCE(m.snippet, ''),
                        COALESCE((SELECT group_concat(filename, ', ') FROM attachments a
                                  WHERE a.message_id = m.id), '')
                 FROM messages m ORDER BY date",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    for (snippet, files) in &details {
        let snippet: String = snippet.chars().take(24).collect();
        println!("      text: {snippet:<28} files: {files}");
    }
    let unread: i64 = query(&db, "SELECT unread_count FROM folders WHERE role = 'inbox'");
    println!("    inbox unread: {unread}");
    assert_eq!(unread as usize, s.seeded);

    step("SMTP submission over STARTTLS (587) and implicit TLS (465), delivered back over POP3");
    let sent = if s.skip_smtp {
        println!("    skipped (SKIM_TEST_SKIP_SMTP)");
        0
    } else {
        2
    };
    for (port, security) in [(587, "starttls"), (465, "tls")].into_iter().take(sent) {
        let subject = format!("寄件測試 {security}");
        let p = submit_and_collect(&s, &db, &dir, port, security, &subject).await;
        assert!(p.initial_done, "mail after the first download is news");
        let found: i64 = query(
            &db,
            &format!("SELECT count(*) FROM messages WHERE subject = '{subject}'"),
        );
        assert_eq!(found, 1);
        println!("    {port} ({security}): sent, delivered and downloaded");
    }

    step("IMAP over implicit TLS (993) and STARTTLS (143)");
    for port in [993, 143] {
        let mut session = imap_client::login(
            &s.host,
            port,
            &s.user,
            &imap_client::Credentials::Password(s.pass.clone()),
        )
        .await
        .unwrap_or_else(|e| panic!("IMAP login on {port} failed: {e}"));
        let inbox = session.select("INBOX").await.unwrap();
        assert_eq!(inbox.exists as usize, s.seeded + sent, "port {port}");
        let _ = session.logout().await;
        println!("    {port}: logged in, INBOX holds {}", inbox.exists);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
