//! A TLS connection to a paired device, handed to code written for the
//! tailnet's plain sockets (the remote screen) as a loopback socket: a
//! thread carries the bytes both ways. One TLS session can't be read on one
//! thread while another writes it, which that code does.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

use openssl::ssl::{ErrorCode, SslStream};
use rustix::event::{poll, PollFd, PollFlags};

/// Bytes held in each direction: little, so a slow network holds back the
/// writer at once (the screen lowers its bitrate from that).
const HELD: usize = 64 * 1024;

/// The plain end of `tls`: what's written to it goes to the device, encrypted,
/// and what the device sends comes out of it. Closing either side ends both.
pub fn bridge(tls: SslStream<TcpStream>) -> io::Result<TcpStream> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let plain = TcpStream::connect(listener.local_addr()?)?;
    let (inner, from) = listener.accept()?;
    // Another program on this computer may have connected first.
    if from != plain.local_addr()? {
        return Err(io::Error::other("someone else took the loopback socket"));
    }
    drop(listener);
    let _ = inner.set_nodelay(true);
    inner.set_nonblocking(true)?;
    tls.get_ref().set_nonblocking(true)?;
    std::thread::Builder::new().name("biglace-lan-bridge".into()).spawn(move || relay(tls, inner))?;
    Ok(plain)
}

/// What an attempt on a non-blocking socket says about waiting.
fn wants(code: ErrorCode) -> Option<PollFlags> {
    match code {
        ErrorCode::WANT_READ => Some(PollFlags::IN),
        ErrorCode::WANT_WRITE => Some(PollFlags::OUT),
        _ => None,
    }
}

/// One pass over both directions: whether bytes moved, and what each socket waits for.
struct Pass {
    moved: bool,
    tls_wait: PollFlags,
    local_wait: PollFlags,
}

/// The bytes held on their way, and which sides still send.
struct Held {
    /// Device → here.
    down: Vec<u8>,
    /// Here → device.
    up: Vec<u8>,
    tls_open: bool,
    local_open: bool,
}

/// Device → here. `None`: the loopback end is gone.
fn pass_down(tls: &mut SslStream<TcpStream>, local: &mut TcpStream, held: &mut Held, buf: &mut [u8], pass: &mut Pass) -> Option<()> {
    if held.tls_open && held.down.len() < HELD {
        match tls.ssl_read(&mut buf[..HELD - held.down.len()]) {
            Ok(n) if n > 0 => {
                held.down.extend_from_slice(&buf[..n]);
                pass.moved = true;
            }
            Ok(_) => held.tls_open = false,
            Err(e) => match wants(e.code()) {
                Some(flags) => pass.tls_wait |= flags,
                None => held.tls_open = false,
            },
        }
    }
    if !held.down.is_empty() {
        match local.write(&held.down) {
            Ok(n) if n > 0 => {
                held.down.drain(..n);
                pass.moved = true;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pass.local_wait |= PollFlags::OUT,
            _ => return None,
        }
    }
    Some(())
}

/// Here → device. `None`: the device's end is gone.
fn pass_up(tls: &mut SslStream<TcpStream>, local: &mut TcpStream, held: &mut Held, buf: &mut [u8], pass: &mut Pass) -> Option<()> {
    if held.local_open && held.up.len() < HELD {
        match local.read(&mut buf[..HELD - held.up.len()]) {
            Ok(n) if n > 0 => {
                held.up.extend_from_slice(&buf[..n]);
                pass.moved = true;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pass.local_wait |= PollFlags::IN,
            _ => held.local_open = false,
        }
    }
    if !held.up.is_empty() {
        match tls.ssl_write(&held.up) {
            Ok(n) if n > 0 => {
                held.up.drain(..n);
                pass.moved = true;
            }
            Ok(_) => return None,
            Err(e) => pass.tls_wait |= wants(e.code())?,
        }
    }
    Some(())
}

fn relay(mut tls: SslStream<TcpStream>, mut local: TcpStream) {
    let mut held = Held { down: Vec::with_capacity(HELD), up: Vec::with_capacity(HELD), tls_open: true, local_open: true };
    let mut buf = vec![0; HELD];
    loop {
        let mut pass = Pass { moved: false, tls_wait: PollFlags::empty(), local_wait: PollFlags::empty() };
        if pass_down(&mut tls, &mut local, &mut held, &mut buf, &mut pass).is_none()
            || pass_up(&mut tls, &mut local, &mut held, &mut buf, &mut pass).is_none()
        {
            return;
        }
        // One side closed and all it sent was passed on.
        if (!held.tls_open && held.down.is_empty()) || (!held.local_open && held.up.is_empty()) {
            break;
        }
        if pass.moved {
            continue;
        }
        let Pass { tls_wait, local_wait, .. } = pass;
        // Only the sockets waited on: a closed one would wake poll at once, again and again.
        let mut fds = Vec::with_capacity(2);
        if !tls_wait.is_empty() {
            fds.push(PollFd::new(tls.get_ref(), tls_wait));
        }
        if !local_wait.is_empty() {
            fds.push(PollFd::new(&local, local_wait));
        }
        if poll(&mut fds, None).is_err() {
            return;
        }
    }
    let _ = tls.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// A TLS pair over loopback: the client end bridged, the server end as is.
    fn pair() -> (TcpStream, SslStream<TcpStream>) {
        let server = super::super::tls::server().unwrap();
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        let accepting = std::thread::spawn(move || server.acceptor.accept(listener.accept().unwrap().0).unwrap());
        let mut config = super::super::tls::connector().unwrap().configure().unwrap();
        config.set_verify_hostname(false);
        let client = config.connect("biglace", TcpStream::connect(addr).unwrap()).unwrap();
        (bridge(client).unwrap(), accepting.join().unwrap())
    }

    #[test]
    fn carries_both_ways_and_ends_together() {
        let (mut plain, mut far) = pair();
        // More than the bridge holds, both ways at once.
        let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let sent = big.clone();
        let echo = std::thread::spawn(move || {
            let mut got = vec![0; sent.len()];
            far.read_exact(&mut got).unwrap();
            far.write_all(&got).unwrap();
            far.flush().unwrap();
            far
        });
        let mut writer = plain.try_clone().unwrap();
        let writing = std::thread::spawn(move || writer.write_all(&big).unwrap());
        let mut back = vec![0; 1_000_000];
        plain.read_exact(&mut back).unwrap();
        writing.join().unwrap();
        assert!(back.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        let mut far = echo.join().unwrap();
        // Closing the plain end closes the device's.
        drop(plain);
        far.get_ref().set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let after = far.read(&mut [0]);
        assert!(matches!(after, Ok(0)), "the device's end still open: {after:?}");
    }
}
