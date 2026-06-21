//! TLS support (feature `tls`), built on rustls with the `ring` crypto provider.
//!
//! Enables encrypted client connections (`--tls-cert` / `--tls-key`). Replication is not offered
//! over TLS in this version — run replicas against the plaintext port. Build with
//! `cargo build --features tls`.

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::fs::File;
use std::io::{self, BufReader};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

/// A configured TLS acceptor that wraps accepted TCP sockets in a server-side TLS stream.
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
}

impl TlsAcceptor {
    /// Load the certificate chain and private key (both PEM) and build the server config.
    pub fn new(cert_path: &Path, key_path: &Path) -> io::Result<TlsAcceptor> {
        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(File::open(cert_path)?))
                .collect::<Result<_, _>>()?;
        if certs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no certificates in cert file",
            ));
        }
        let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut BufReader::new(
            File::open(key_path)?,
        ))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no private key found"))?;

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        Ok(TlsAcceptor {
            config: Arc::new(config),
        })
    }

    /// Wrap a freshly-accepted socket. The TLS handshake completes lazily on first I/O.
    pub fn accept(&self, sock: TcpStream) -> io::Result<StreamOwned<ServerConnection, TcpStream>> {
        let conn = ServerConnection::new(self.config.clone()).map_err(io::Error::other)?;
        Ok(StreamOwned::new(conn, sock))
    }
}
