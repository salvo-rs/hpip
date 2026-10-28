use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::error::Error;

/// Import the documented PKCS12 identity without writing an unencrypted key to disk.
pub fn load_identity(
    path: &std::path::Path,
    password: &str,
) -> Result<salvo::conn::rustls::RustlsConfig, Error> {
    use salvo::conn::rustls::{Keycert, RustlsConfig};

    // OpenSSL's Windows file BIO does not accept Rust's verbatim canonical paths.
    #[cfg(windows)]
    let openssl_path = {
        let path = path.to_string_lossy();
        if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{unc}"))
        } else {
            PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(&path))
        }
    };
    #[cfg(not(windows))]
    let openssl_path = path;
    let output = Command::new("openssl")
        .args(["pkcs12", "-in"])
        .arg(openssl_path)
        .args([
            "-nodes",
            "-passin",
            if password.is_empty() {
                "pass:"
            } else {
                "env:HPIP_TLS_PASSWORD"
            },
        ])
        .env("HPIP_TLS_PASSWORD", password)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error(format!("Loading TLS identity requires OpenSSL: {e}")))?;
    if !output.status.success() {
        return Err(Error(
            "Unable to load TLS identity; check its format and HTTP_SSL_PASS".into(),
        ));
    }
    Ok(RustlsConfig::new(
        Keycert::new()
            .cert(output.stdout.clone())
            .key(output.stdout),
    ))
}

/// Generate a noninteractive, short-lived localhost identity.
pub fn generate_tls_data(
    temp_dir: &(String, PathBuf),
) -> Result<((String, PathBuf), String), Error> {
    let tls_dir = temp_dir.1.join("tls");
    fs::create_dir_all(&tls_dir).map_err(|e| Error(format!("Creating TLS directory: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tls_dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| Error(format!("Protecting TLS directory: {e}")))?;
    }
    run_openssl(
        Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                "tls.key",
                "-out",
                "tls.crt",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1",
            ])
            .current_dir(&tls_dir),
        "Generating TLS certificate",
    )?;
    run_openssl(
        Command::new("openssl")
            .args([
                "pkcs12", "-export", "-out", "tls.p12", "-inkey", "tls.key", "-in", "tls.crt",
                "-passout", "pass:",
            ])
            .current_dir(&tls_dir),
        "Exporting TLS identity",
    )?;
    Ok((
        (
            format!("{}/tls/tls.p12", temp_dir.0),
            tls_dir.join("tls.p12"),
        ),
        String::new(),
    ))
}

fn run_openssl(command: &mut Command, operation: &str) -> Result<(), Error> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error(format!("{operation} requires OpenSSL: {e}")))?;
    if !output.status.success() {
        return Err(Error(format!("{operation} failed with {}", output.status)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use salvo::conn::Acceptor;
    use salvo::prelude::*;

    use super::*;

    #[tokio::test]
    async fn generated_identity_binds_tls() {
        if Command::new("openssl").arg("version").output().is_err() {
            eprintln!("OpenSSL is unavailable; TLS generation requires OpenSSL at runtime");
            return;
        }
        let dir = crate::test_support::TestDir::new();
        let ((_, path), password) =
            generate_tls_data(&(dir.path().display().to_string(), dir.path().to_owned())).unwrap();
        let acceptor = TcpListener::new("127.0.0.1:0")
            .rustls(load_identity(&path.canonicalize().unwrap(), &password).unwrap())
            .try_bind()
            .await
            .unwrap();
        assert_eq!(acceptor.holdings()[0].http_scheme.as_str(), "https");
    }

    #[tokio::test]
    async fn rejects_invalid_tls_identity() {
        let dir = crate::test_support::TestDir::new();
        let identity = dir.path().join("invalid.p12");
        fs::write(&identity, "invalid certificate").unwrap();
        assert!(load_identity(&identity, "").is_err());
        let invalid = salvo::conn::rustls::RustlsConfig::new(
            salvo::conn::rustls::Keycert::new()
                .cert(b"invalid".to_vec())
                .key(b"invalid".to_vec()),
        );
        assert!(
            TcpListener::new("127.0.0.1:0")
                .rustls(invalid)
                .try_bind()
                .await
                .is_err()
        );
    }
}
