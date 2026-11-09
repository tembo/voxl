//! HTTPS. Browsers only decode video on secure pages, so a viewer on another machine needs
//! voxl over HTTPS. With `--tls-cert` and `--tls-key` voxl serves the given PEM certificate
//! chain and key; with `--tls` alone it makes a self-signed certificate once, keeps it (so a
//! browser's exception for it lasts across restarts) and serves that.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// The TLS settings the flags ask for, or `None` to serve plain HTTP.
/// `names` go into a newly made self-signed certificate.
pub fn from_args(
    a: &HashMap<String, String>,
    names: Vec<String>,
) -> Option<Arc<rustls::ServerConfig>> {
    let (cert, key) = match (a.get("tls-cert"), a.get("tls-key")) {
        (Some(c), Some(k)) => (PathBuf::from(c), PathBuf::from(k)),
        (Some(_), None) | (None, Some(_)) => {
            panic!("--tls-cert and --tls-key go together: give both, or --tls for a self-signed certificate")
        }
        (None, None) if crate::truthy(a.get("tls"), false) => self_signed(names),
        (None, None) => return None,
    };
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert)
        .and_then(|it| it.collect())
        .unwrap_or_else(|e| panic!("reading the certificate {}: {e}", cert.display()));
    let key = PrivateKeyDer::from_pem_file(&key)
        .unwrap_or_else(|e| panic!("reading the key {}: {e}", key.display()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap_or_else(|e| panic!("the certificate and key don't work together: {e}"));
    // HTTP/1.1 only: the stream is a WebSocket, which browsers open over HTTP/1.1.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    log::info!("HTTPS with the certificate {}", cert.display());
    Some(Arc::new(config))
}

/// Where voxl keeps the certificate it makes: %LOCALAPPDATA%\voxl on Windows,
/// $XDG_CONFIG_HOME/voxl or ~/.config/voxl elsewhere, or VOXL_TLS_DIR.
fn tls_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("VOXL_TLS_DIR") {
        return d.into();
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.unwrap_or_else(|| PathBuf::from(".")).join("voxl")
}

/// The kept self-signed certificate and key, made on first use.
fn self_signed(names: Vec<String>) -> (PathBuf, PathBuf) {
    let dir = tls_dir();
    let (cert, key) = (dir.join("voxl-cert.pem"), dir.join("voxl-key.pem"));
    if cert.exists() && key.exists() {
        return (cert, key);
    }
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("creating {}: {e}", dir.display()));
    let made = rcgen::generate_simple_self_signed(names.clone()).expect("making a certificate");
    std::fs::write(&cert, made.cert.pem())
        .unwrap_or_else(|e| panic!("writing {}: {e}", cert.display()));
    write_private(&key, made.key_pair.serialize_pem().as_bytes())
        .unwrap_or_else(|e| panic!("writing {}: {e}", key.display()));
    log::info!(
        "made a self-signed certificate for {} in {}; browsers warn about it once",
        names.join(", "),
        dir.display()
    );
    (cert, key)
}

/// Writes the private key so only its owner can read it (on Unix; on Windows the file
/// inherits the per-user %LOCALAPPDATA% permissions).
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(path)?, data)
}

/// Serves `app` over TLS on `listener` until the process ends.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    config: Arc<rustls::ServerConfig>,
) {
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    loop {
        let tcp = match listener.accept().await {
            Ok((tcp, _)) => tcp,
            Err(e) => {
                log::warn!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            // A client that never finishes the handshake doesn't hold the task forever.
            let stream =
                match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => return log::debug!("TLS handshake: {e}"),
                    Err(_) => return,
                };
            let service = hyper_util::service::TowerToHyperService::new(app);
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
        });
    }
}
