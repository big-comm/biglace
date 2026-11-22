//! Who is at the other end of a TLS connection on the local network, and
//! pairing two devices. Each side signs the other's fresh nonce and the
//! server certificate's fingerprint with its device key; a device in the
//! middle holds two sessions with different certificates, so no signature
//! carries over. Pairing adds a six-digit code the two people compare. The
//! client commits to its random part before it sees the server's, so a
//! device in the middle can't try keys until the codes match.

use std::io::{self, Read, Write};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::device_key::hex;
use crate::remote::{write_frame, Kind};

/// Largest step read: they are a few hundred bytes of JSON.
const MAX_STEP: usize = 4096;
/// Longest name a device gives itself.
const MAX_NAME: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// Become paired (anyone on the network may ask; the person decides).
    Pair,
    /// Talk, between devices already paired.
    Talk,
}

#[derive(Serialize, Deserialize)]
struct Hello {
    key: String,
    nonce: String,
    name: String,
    purpose: Purpose,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    key: String,
    nonce: String,
    name: String,
    sig: String,
}

#[derive(Serialize, Deserialize)]
struct Proof {
    sig: String,
}

#[derive(Serialize, Deserialize)]
struct Commit {
    hash: String,
}

#[derive(Serialize, Deserialize)]
struct Part {
    random: String,
}

#[derive(Serialize, Deserialize)]
struct Answer {
    ok: bool,
}

/// The device at the other end, as it proved itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Its device key (base64url), what pairing pins.
    pub key: String,
    pub name: String,
}

fn send<T: Serialize>(w: &mut impl Write, step: &T) -> io::Result<()> {
    write_frame(w, Kind::Lan, &serde_json::to_vec(step)?)
}

fn recv<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if head[0] != Kind::Lan as u8 || len > MAX_STEP {
        return Err(io::Error::other("unexpected step"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(io::Error::other)
}

fn public(me: &SigningKey) -> String {
    B64.encode(me.verifying_key().to_bytes())
}

fn sign(me: &SigningKey, message: &str) -> String {
    B64.encode(me.sign(message.as_bytes()).to_bytes())
}

/// 32 random bytes, base64url (43 characters).
fn random() -> io::Result<String> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(B64.encode(raw))
}

fn token_ok(t: &str) -> bool {
    t.len() == 43 && t.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn display_name_ok(n: &str) -> bool {
    !n.trim().is_empty() && n.chars().count() <= MAX_NAME && !n.chars().any(char::is_control)
}

fn server_message(fingerprint: &[u8; 32], client_nonce: &str, server_nonce: &str, client_key: &str) -> String {
    format!("biglace-lan-server-v1\n{}\n{client_nonce}\n{server_nonce}\n{client_key}", hex(fingerprint))
}

fn client_message(fingerprint: &[u8; 32], client_nonce: &str, server_nonce: &str, server_key: &str) -> String {
    format!("biglace-lan-client-v1\n{}\n{client_nonce}\n{server_nonce}\n{server_key}", hex(fingerprint))
}

fn refused(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why.to_string())
}

/// Client side: say who we are and why, and check the server's proof (and,
/// for a paired device, that it holds `expect`).
pub fn client(
    s: &mut (impl Read + Write),
    fingerprint: &[u8; 32],
    me: &SigningKey,
    name: &str,
    purpose: Purpose,
    expect: Option<&str>,
) -> io::Result<Peer> {
    let nonce = random()?;
    send(s, &Hello { key: public(me), nonce: nonce.clone(), name: name.to_string(), purpose })?;
    let reply: Reply = recv(s)?;
    if !token_ok(&reply.key) || !token_ok(&reply.nonce) || !display_name_ok(&reply.name) {
        return Err(refused("malformed reply"));
    }
    if expect.is_some_and(|k| k != reply.key) {
        return Err(refused("not the paired device"));
    }
    if !crate::device_key::verify(&reply.key, &server_message(fingerprint, &nonce, &reply.nonce, &public(me)), &reply.sig) {
        return Err(refused("the server's proof doesn't hold"));
    }
    send(s, &Proof { sig: sign(me, &client_message(fingerprint, &nonce, &reply.nonce, &reply.key)) })?;
    Ok(Peer { key: reply.key, name: reply.name })
}

/// Server side: `admit` decides from the client's key and purpose whether
/// to go on (a stranger may only ask to pair).
pub fn server(
    s: &mut (impl Read + Write),
    fingerprint: &[u8; 32],
    me: &SigningKey,
    name: &str,
    admit: impl Fn(&str, Purpose) -> bool,
) -> io::Result<(Peer, Purpose)> {
    let hello: Hello = recv(s)?;
    if !token_ok(&hello.key) || !token_ok(&hello.nonce) || !display_name_ok(&hello.name) {
        return Err(refused("malformed hello"));
    }
    if !admit(&hello.key, hello.purpose) {
        return Err(refused("not paired"));
    }
    let nonce = random()?;
    let sig = sign(me, &server_message(fingerprint, &hello.nonce, &nonce, &hello.key));
    send(s, &Reply { key: public(me), nonce: nonce.clone(), name: name.to_string(), sig })?;
    let proof: Proof = recv(s)?;
    if !crate::device_key::verify(&hello.key, &client_message(fingerprint, &hello.nonce, &nonce, &public(me)), &proof.sig) {
        return Err(refused("the client's proof doesn't hold"));
    }
    Ok((Peer { key: hello.key, name: hello.name }, hello.purpose))
}

/// The six digits both people compare.
fn code(client_key: &str, server_key: &str, fingerprint: &[u8; 32], client_random: &str, server_random: &str) -> String {
    let digest = Sha256::digest(
        format!("biglace-lan-code-v1\n{client_key}\n{server_key}\n{}\n{client_random}\n{server_random}", hex(fingerprint)).as_bytes(),
    );
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{n:06}")
}

/// Client side of pairing, after [`client`] with [`Purpose::Pair`]: the code to show.
pub fn pair_client(s: &mut (impl Read + Write), fingerprint: &[u8; 32], me: &SigningKey, peer: &Peer) -> io::Result<String> {
    let mine = random()?;
    send(s, &Commit { hash: B64.encode(Sha256::digest(mine.as_bytes())) })?;
    let theirs: Part = recv(s)?;
    if !token_ok(&theirs.random) {
        return Err(refused("malformed part"));
    }
    send(s, &Part { random: mine.clone() })?;
    Ok(code(&public(me), &peer.key, fingerprint, &mine, &theirs.random))
}

/// Server side of pairing, after [`server`] answered [`Purpose::Pair`]: the code to show.
pub fn pair_server(s: &mut (impl Read + Write), fingerprint: &[u8; 32], me: &SigningKey, peer: &Peer) -> io::Result<String> {
    let commit: Commit = recv(s)?;
    let mine = random()?;
    send(s, &Part { random: mine.clone() })?;
    let theirs: Part = recv(s)?;
    if !token_ok(&theirs.random) || B64.encode(Sha256::digest(theirs.random.as_bytes())) != commit.hash {
        return Err(refused("the client's part doesn't match what it committed to"));
    }
    Ok(code(&peer.key, &public(me), fingerprint, &theirs.random, &mine))
}

/// Our person's answer goes out, the other's comes back: paired only when both said yes.
pub fn agree(s: &mut (impl Read + Write), ours: bool) -> io::Result<bool> {
    send(s, &Answer { ok: ours })?;
    let theirs: Answer = recv(s)?;
    Ok(ours && theirs.ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    /// A client and a server over loopback TLS (the server on a thread): what each returned.
    fn session<T: Send + 'static, U>(
        server: impl FnOnce(&mut openssl::ssl::SslStream<TcpStream>, [u8; 32]) -> T + Send + 'static,
        client: impl FnOnce(&mut openssl::ssl::SslStream<TcpStream>, [u8; 32]) -> U,
    ) -> (T, U) {
        let tls = crate::lan::tls::server().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let fingerprint = tls.fingerprint;
        let side = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut s = tls.acceptor.accept(tcp).unwrap();
            server(&mut s, fingerprint)
        });
        let tcp = TcpStream::connect(addr).unwrap();
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut config = crate::lan::tls::connector().unwrap().configure().unwrap();
        config.set_verify_hostname(false);
        let mut s = config.connect("biglace", tcp).unwrap();
        let seen = crate::lan::tls::peer_fingerprint(&s).unwrap();
        assert_eq!(seen, fingerprint, "the client sees the server's certificate");
        let theirs = client(&mut s, seen);
        // Hang up first: a server still waiting for a step then sees the end.
        drop(s);
        (side.join().unwrap(), theirs)
    }

    #[test]
    fn pairing_shows_the_same_code() {
        let ((server_code, server_agreed), (client_code, client_agreed)) = session(
            |s, fp| {
                let (peer, purpose) = server(s, &fp, &key(2), "notebook", |_, _| true).unwrap();
                assert_eq!(purpose, Purpose::Pair);
                assert_eq!(peer.key, public(&key(1)));
                let code = pair_server(s, &fp, &key(2), &peer).unwrap();
                (code, agree(s, true).unwrap())
            },
            |s, fp| {
                let peer = client(s, &fp, &key(1), "phone", Purpose::Pair, None).unwrap();
                assert_eq!((peer.key.as_str(), peer.name.as_str()), (public(&key(2)).as_str(), "notebook"));
                let code = pair_client(s, &fp, &key(1), &peer).unwrap();
                (code, agree(s, true).unwrap())
            },
        );
        assert_eq!(server_code.len(), 6);
        assert_eq!(server_code, client_code);
        assert!(server_agreed && client_agreed);
    }

    #[test]
    fn either_refusal_cancels_pairing() {
        let (server_result, ()) = session(
            |s, fp| {
                let (peer, _) = server(s, &fp, &key(2), "notebook", |_, _| true).unwrap();
                pair_server(s, &fp, &key(2), &peer).unwrap();
                agree(s, true).unwrap()
            },
            |s, fp| {
                let peer = client(s, &fp, &key(1), "phone", Purpose::Pair, None).unwrap();
                pair_client(s, &fp, &key(1), &peer).unwrap();
                assert!(!agree(s, false).unwrap());
            },
        );
        assert!(!server_result, "the server's yes doesn't pair alone");
    }

    #[test]
    fn talking_needs_the_paired_key() {
        // The client expects another device's key: it stops before proving itself.
        let (server_result, ()) = session(
            |s, fp| server(s, &fp, &key(2), "notebook", |_, _| true).map(|_| ()),
            |s, fp| {
                let err = client(s, &fp, &key(1), "phone", Purpose::Talk, Some(&public(&key(3)))).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            },
        );
        assert!(server_result.is_err(), "no proof comes after the client gave up");
        // A stranger asking to talk is turned away.
        let (server_result, ()) = session(
            |s, fp| server(s, &fp, &key(2), "notebook", |_, purpose| purpose == Purpose::Pair).map(|_| ()),
            |s, fp| assert!(client(s, &fp, &key(1), "phone", Purpose::Talk, None).is_err()),
        );
        assert_eq!(server_result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn proofs_bind_the_certificate() {
        // A device in the middle shows its own certificate: the client binds
        // to that fingerprint, which isn't the server's, so neither proof holds.
        let (server_result, ()) = session(
            |s, fp| server(s, &fp, &key(2), "notebook", |_, _| true).map(|_| ()),
            |s, _| {
                let elsewhere = [7u8; 32];
                let err = client(s, &elsewhere, &key(1), "phone", Purpose::Pair, None).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            },
        );
        assert!(server_result.is_err());
    }

    #[test]
    fn a_changed_part_is_refused() {
        let (server_result, ()) = session(
            |s, fp| {
                let (peer, _) = server(s, &fp, &key(2), "notebook", |_, _| true).unwrap();
                pair_server(s, &fp, &key(2), &peer)
            },
            |s, fp| {
                client(s, &fp, &key(1), "phone", Purpose::Pair, None).unwrap();
                send(s, &Commit { hash: B64.encode(Sha256::digest(random().unwrap().as_bytes())) }).unwrap();
                let _: Part = recv(s).unwrap();
                // Another random than the one committed to: chosen after seeing the server's.
                send(s, &Part { random: random().unwrap() }).unwrap();
            },
        );
        assert_eq!(server_result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn steps_are_bounded() {
        let mut big = Vec::new();
        write_frame(&mut big, Kind::Lan, &vec![b' '; MAX_STEP + 1]).unwrap();
        assert!(recv::<Answer>(&mut big.as_slice()).is_err());
        let mut other = Vec::new();
        write_frame(&mut other, Kind::Chat, b"{\"ok\":true}").unwrap();
        assert!(recv::<Answer>(&mut other.as_slice()).is_err(), "only Lan steps");
        assert!(!display_name_ok("a\u{1b}[2J"), "no control characters in names");
        assert!(!display_name_ok(&"x".repeat(MAX_NAME + 1)));
    }
}
