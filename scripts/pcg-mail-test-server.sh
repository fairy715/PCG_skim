#!/usr/bin/env bash
# Throwaway mail server for src-tauri/src/mail/server_tests.rs (fork addition).
#
# Linux only, as root, in a disposable machine or container: it installs
# packages, adds a test CA to the system trust store and binds ports 110, 143,
# 465, 587, 993 and 995 on 127.0.0.1. Undo with `--stop`.
#
#   scripts/pcg-mail-test-server.sh [path/to/winmail.dat]
#   scripts/pcg-mail-test-server.sh --stop
#
# What it sets up, all signed by a throwaway "company" CA that publishes a
# revocation list (Wine's verifier needs one, see server_tests.rs):
#   - Dovecot: POP3 on 110 (STLS) and 995, IMAP on 143 (STARTTLS) and 993,
#     passwords only over TLS. Mailbox `ming`, password `Secret-123`.
#   - aiosmtpd: submission on 587 (STARTTLS) and 465, AUTH only over TLS,
#     delivering into the same mailbox.
#   - rogue.test (127.0.0.2:995): TLS with a certificate nobody trusts.
# The mailbox starts with three messages (UTF-8 Chinese, legacy Big5, an
# attachment with a Chinese file name), four if a winmail.dat is given.
set -euo pipefail

DIR=/srv/pcgmail
CA_NAME=pcg-test-company-ca

stop() {
  [[ -f "$DIR/dovecot.conf" ]] && doveadm -c "$DIR/dovecot.conf" stop 2>/dev/null || true
  # By recorded process id: matching command lines could hit unrelated
  # processes, including whatever shell happens to be running this script.
  if [[ -f "$DIR/pids" ]]; then
    while read -r pid; do kill "$pid" 2>/dev/null || true; done < "$DIR/pids"
  fi
  rm -f "/usr/local/share/ca-certificates/$CA_NAME.crt"
  update-ca-certificates --fresh >/dev/null 2>&1 || true
  sed -i '/ rogue\.test$/d' /etc/hosts
  rm -rf "$DIR" /run/pcgdv
}

if [[ "${1:-}" == "--stop" ]]; then
  stop
  echo "stopped and removed"
  exit 0
fi
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }
WINMAIL="${1:-}"

stop
DEBIAN_FRONTEND=noninteractive apt-get install -y -q dovecot-pop3d dovecot-imapd openssl >/dev/null
python3 -c "import aiosmtpd" 2>/dev/null || pip install -q --break-system-packages aiosmtpd
id vmail >/dev/null 2>&1 || useradd -r -u 5000 -M -d /nonexistent -s /usr/sbin/nologin vmail 2>/dev/null

mkdir -p "$DIR"/{ca/newcerts,crl,mail/ming/{new,cur,tmp}} /run/pcgdv
cd "$DIR"

# --- certificates -----------------------------------------------------------
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=PCG Test Company CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -keyout ca.key -out ca.crt 2>/dev/null
touch ca/index.txt
echo 1000 > ca/crlnumber
cat > ca.cnf <<EOF
[ ca ]
default_ca = test_ca
[ test_ca ]
dir = $DIR/ca
database = \$dir/index.txt
new_certs_dir = \$dir/newcerts
certificate = $DIR/ca.crt
private_key = $DIR/ca.key
crlnumber = \$dir/crlnumber
default_md = sha256
default_crl_days = 30
EOF
openssl ca -config ca.cnf -gencrl -out crl/ca.pem 2>/dev/null
openssl crl -in crl/ca.pem -outform DER -out crl/ca.crl
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
cat > server.ext <<EOF
subjectAltName=DNS:localhost
basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
crlDistributionPoints=URI:http://localhost:8099/ca.crl
EOF
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 30 \
  -extfile server.ext -out server.crt 2>/dev/null
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=rogue.test" \
  -addext "subjectAltName=DNS:rogue.test" -keyout rogue.key -out rogue.crt 2>/dev/null
cp ca.crt "/usr/local/share/ca-certificates/$CA_NAME.crt"
update-ca-certificates >/dev/null
grep -q " rogue\.test$" /etc/hosts || echo "127.0.0.2 rogue.test" >> /etc/hosts

# --- Dovecot ----------------------------------------------------------------
echo 'ming:{PLAIN}Secret-123' > users
cat > dovecot.conf <<EOF
protocols = pop3 imap
base_dir = /run/pcgdv
listen = 127.0.0.1
log_path = $DIR/dovecot.log
disable_plaintext_auth = yes
ssl = yes
ssl_cert = <$DIR/server.crt
ssl_key = <$DIR/server.key
ssl_min_protocol = TLSv1.2
mail_location = maildir:$DIR/mail/%n
first_valid_uid = 5000
pop3_uidl_format = %08Xu%08Xv
passdb {
  driver = passwd-file
  args = scheme=PLAIN username_format=%n $DIR/users
}
userdb {
  driver = static
  args = uid=vmail gid=vmail home=$DIR/mail/%n
}
service pop3-login {
  inet_listener pop3 {
    port = 110
  }
  inet_listener pop3s {
    port = 995
    ssl = yes
  }
}
service imap-login {
  inet_listener imap {
    port = 143
  }
  inet_listener imaps {
    port = 993
    ssl = yes
  }
}
EOF

# --- SMTP -------------------------------------------------------------------
cat > smtpd.py <<EOF
import asyncio, os, ssl, time, pwd
from aiosmtpd.controller import Controller
from aiosmtpd.smtp import AuthResult, LoginPassword

MAILDIR = "$DIR/mail/ming"
vmail = pwd.getpwnam("vmail")

class Deliver:
    async def handle_DATA(self, server, session, envelope):
        name = f"{time.time():.6f}.{os.getpid()}.smtp"
        tmp = os.path.join(MAILDIR, "tmp", name)
        with open(tmp, "wb") as f:
            f.write(envelope.original_content)
        os.chown(tmp, vmail.pw_uid, vmail.pw_gid)
        os.rename(tmp, os.path.join(MAILDIR, "new", name))
        return "250 OK"

def auth(server, session, envelope, mechanism, data):
    ok = isinstance(data, LoginPassword) and data.login == b"ming" and data.password == b"Secret-123"
    return AuthResult(success=ok)

ctx = ssl.create_default_context(ssl.Purpose.CLIENT_AUTH)
ctx.load_cert_chain("$DIR/server.crt", "$DIR/server.key")
common = dict(authenticator=auth, auth_require_tls=True, auth_exclude_mechanism=["CRAM-MD5"])
Controller(Deliver(), hostname="127.0.0.1", port=587, tls_context=ctx, require_starttls=True,
           server_hostname="localhost", **common).start()
# Implicit TLS: the connection is encrypted from the first byte, but aiosmtpd
# only counts STARTTLS as TLS for auth_require_tls, so it is lifted here.
implicit = dict(common, auth_require_tls=False)
Controller(Deliver(), hostname="127.0.0.1", port=465, ssl_context=ctx,
           server_hostname="localhost", **implicit).start()
print("smtp up", flush=True)
asyncio.new_event_loop().run_forever()
EOF

# --- the mailbox ------------------------------------------------------------
python3 - "$DIR/mail/ming/new" "$WINMAIL" <<'EOF'
import sys, os, base64, email.message, email.policy
out, winmail = sys.argv[1], sys.argv[2]
def save(n, raw):
    open(os.path.join(out, f"seed{n}.pcg"), "wb").write(raw)
m = email.message.EmailMessage()
m["From"] = "陳經理 <chen@example.com.tw>"; m["To"] = "ming@localhost"
m["Subject"] = "季度報告 Q3"; m["Date"] = "Tue, 03 Mar 2026 10:00:00 +0800"
m["Message-ID"] = "<s1@example.com.tw>"
m.set_content("附件是第三季的報告，請查收。\n")
save(1, m.as_bytes(policy=email.policy.SMTP))
b5 = lambda s: base64.b64encode(s.encode("big5")).decode()
save(2, (f"From: =?big5?B?{b5('王小明')}?= <wang@example.com.tw>\r\nTo: ming@localhost\r\n"
         f"Subject: =?big5?B?{b5('會議通知')}?=\r\nDate: Wed, 04 Mar 2026 09:00:00 +0800\r\n"
         "Message-ID: <s2@example.com.tw>\r\nMIME-Version: 1.0\r\n"
         "Content-Type: text/plain; charset=big5\r\nContent-Transfer-Encoding: 8bit\r\n\r\n").encode()
        + "明天上午十點開會，請準時出席。\r\n".encode("big5"))
m = email.message.EmailMessage()
m["From"] = "Budi <budi@example.co.id>"; m["To"] = "ming@localhost"
m["Subject"] = "Laporan bulanan"; m["Date"] = "Thu, 05 Mar 2026 08:00:00 +0700"
m["Message-ID"] = "<s3@example.co.id>"
m.set_content("Terlampir laporannya.\n")
m.add_alternative("<p>Terlampir <b>laporannya</b>.</p>", subtype="html")
m.add_attachment(b"col1,col2\r\n1,2\r\n", maintype="text", subtype="csv", filename="月報表.csv")
save(3, m.as_bytes(policy=email.policy.SMTP))
if winmail:
    m = email.message.EmailMessage()
    m["From"] = "Outlook User <old@example.com>"; m["To"] = "ming@localhost"
    m["Subject"] = "From Outlook"; m["Date"] = "Fri, 06 Mar 2026 08:00:00 +0100"
    m["Message-ID"] = "<s4@example.com>"
    m.set_content("See attached.\n")
    m.add_attachment(open(winmail, "rb").read(), maintype="application",
                     subtype="ms-tnef", filename="winmail.dat")
    save(4, m.as_bytes(policy=email.policy.SMTP))
print(len(os.listdir(out)))
EOF
SEEDED=$(ls "$DIR/mail/ming/new" | wc -l)
chown -R vmail:vmail "$DIR/mail"
chmod 755 "$DIR"
chmod 644 users

# --- start ------------------------------------------------------------------
# Detached from this script's output, so a caller piping it is not kept waiting.
dovecot -c "$DIR/dovecot.conf" </dev/null >/dev/null 2>&1
nohup python3 -m http.server 8099 --bind 127.0.0.1 --directory "$DIR/crl" </dev/null >"$DIR/http.log" 2>&1 &
echo $! >> "$DIR/pids"
nohup python3 "$DIR/smtpd.py" </dev/null >"$DIR/smtpd.log" 2>&1 &
echo $! >> "$DIR/pids"
nohup openssl s_server -quiet -accept 127.0.0.2:995 -cert rogue.crt -key rogue.key </dev/null >/dev/null 2>&1 &
echo $! >> "$DIR/pids"
sleep 2

cat <<EOF
Test mail server up. For the tests:
  export SKIM_TEST_MAIL_HOST=localhost SKIM_TEST_MAIL_USER=ming SKIM_TEST_MAIL_PASS=Secret-123
  export SKIM_TEST_MAIL_SEEDED=$SEEDED SKIM_TEST_ROGUE_HOST=rogue.test
  export SKIM_TEST_SKIP_SMTP=1   # only under Wine
EOF
