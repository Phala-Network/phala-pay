//! Owned, certificate-verified local HTTPS ingress for CLI integration tests.
use anyhow::{Context, Result, ensure};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

pub struct RpcTlsProxy {
    child: Child,
    directory: PathBuf,
    pub read_url: String,
    pub verify_url: String,
    pub certificate: PathBuf,
}
impl RpcTlsProxy {
    pub fn start(upstream: &str) -> Result<Self> {
        let directory =
            std::env::temp_dir().join(format!("chain-reads-tls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory)?;
        let result = (|| {
            let certificate = directory.join("cert.pem");
            let key = directory.join("key.pem");
            let output = Command::new("openssl")
                .args([
                    "req",
                    "-x509",
                    "-newkey",
                    "rsa:2048",
                    "-nodes",
                    "-days",
                    "1",
                    "-subj",
                    "/CN=localhost",
                    "-addext",
                    "subjectAltName=DNS:localhost,IP:127.0.0.1",
                    "-keyout",
                ])
                .arg(&key)
                .arg("-out")
                .arg(&certificate)
                .output()?;
            ensure!(output.status.success(), "generate local RPC certificate");
            let script =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/local/tls_proxy.py");
            let mut child = Command::new("python3")
                .arg(script)
                .arg("--certificate")
                .arg(&certificate)
                .arg("--key")
                .arg(&key)
                .args(["--upstream", upstream, "--bind", "127.0.0.1", "--port", "0"])
                .stdout(Stdio::piped())
                .spawn()?;
            let mut line = String::new();
            let read_result = BufReader::new(child.stdout.take().context("TLS proxy stdout")?)
                .read_line(&mut line);
            let port =
                read_result.and_then(|_| line.trim().parse::<u16>().map_err(std::io::Error::other));
            let port = match port {
                Ok(port) => port,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.into());
                }
            };
            Ok(Self {
                child,
                directory: directory.clone(),
                read_url: format!("https://127.0.0.1:{port}"),
                verify_url: format!("https://localhost:{port}"),
                certificate,
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&directory);
        }
        result
    }
}
impl Drop for RpcTlsProxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
