//! TLS on the local network only encrypts: its certificate, made at each
//! start, names nobody. Who is at the other end is proven by device keys
//! bound to the certificate's fingerprint (see `super::handshake`).

use std::io::{self, Read, Write};

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use openssl::ssl::{SslAcceptor, SslConnector, SslMethod, SslStream, SslVerifyMode, SslVersion};
use openssl::x509::{X509NameBuilder, X509};
use sha2::{Digest, Sha256};

pub struct Server {
    pub acceptor: SslAcceptor,
    /// SHA-256 of our certificate (DER), which both sides' proofs name.
    pub fingerprint: [u8; 32],
}

/// A TLS 1.3 server with a fresh P-256 certificate.
pub fn server() -> anyhow::Result<Server> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    let key = PKey::from_ec_key(EcKey::generate(&group)?)?;
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, "biglace")?;
    let name = name.build();
    let mut serial = BigNum::new()?;
    serial.rand(127, MsbOption::MAYBE_ZERO, false)?;
    let serial = serial.to_asn1_integer()?;
    let (start, end) = (Asn1Time::days_from_now(0)?, Asn1Time::days_from_now(3650)?);
    let mut cert = X509::builder()?;
    cert.set_version(2)?;
    cert.set_serial_number(&serial)?;
    cert.set_subject_name(&name)?;
    cert.set_issuer_name(&name)?;
    cert.set_pubkey(&key)?;
    cert.set_not_before(&start)?;
    cert.set_not_after(&end)?;
    cert.sign(&key, MessageDigest::sha256())?;
    let cert = cert.build();
    let mut acceptor = SslAcceptor::mozilla_modern_v5(SslMethod::tls_server())?;
    acceptor.set_private_key(&key)?;
    acceptor.set_certificate(&cert)?;
    acceptor.check_private_key()?;
    Ok(Server { acceptor: acceptor.build(), fingerprint: Sha256::digest(cert.to_der()?).into() })
}

/// A TLS 1.3 client that takes any certificate: the handshake then checks
/// the server's device key against the certificate it saw.
pub fn connector() -> anyhow::Result<SslConnector> {
    let mut connector = SslConnector::builder(SslMethod::tls_client())?;
    connector.set_min_proto_version(Some(SslVersion::TLS1_3))?;
    connector.set_verify(SslVerifyMode::NONE); // ast-grep-ignore: rust-tls-unverified (identity is the device key's signature over this certificate, checked in lan::handshake)
    Ok(connector.build())
}

/// SHA-256 of the certificate the server showed this client.
pub fn peer_fingerprint<S: Read + Write>(stream: &SslStream<S>) -> io::Result<[u8; 32]> {
    let cert = stream.ssl().peer_certificate().ok_or_else(|| io::Error::other("the server showed no certificate"))?;
    Ok(Sha256::digest(cert.to_der().map_err(io::Error::other)?).into())
}
