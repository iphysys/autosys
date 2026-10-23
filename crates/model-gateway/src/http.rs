//! The HTTPS client every adapter uses: TLS verification always on, against
//! the operating system's roots plus an optional internal CA bundle.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use contracts::{Error, ErrorCode};
use reqwest::Url;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use rustls_platform_verifier::Verifier;

/// Upper bound for opening a connection, within the request timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A PEM bundle of CAs to trust in addition to the operating system's roots:
/// the environment variable that named it, and the path it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaBundle {
    pub var: String,
    pub path: String,
}

/// A client for one provider: see [`client`]. Reads `ca_bundle`, failing with
/// an error that names its variable.
pub(crate) fn provider_client(
    timeout: Duration,
    ca_bundle: Option<&CaBundle>,
) -> Result<reqwest::Client, Error> {
    let extra_roots = match ca_bundle {
        Some(bundle) => self::ca_bundle(&bundle.var, &bundle.path)?,
        None => Vec::new(),
    };
    client(timeout, extra_roots)
}

/// A client whose requests time out after `timeout` and never follow
/// redirects. The server is verified by the platform verifier, which also
/// trusts `extra_roots`. Uses the ring crypto provider directly, so nothing
/// depends on a process-wide default.
pub(crate) fn client(
    timeout: Duration,
    extra_roots: Vec<CertificateDer<'static>>,
) -> Result<reqwest::Client, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = if extra_roots.is_empty() {
        Verifier::new(provider.clone())
    } else {
        Verifier::new_with_extra_roots(extra_roots, provider.clone())
    }
    .map_err(|e| internal("cannot set up TLS certificate verification", e))?;
    // rustls names the API for supplying a verifier `dangerous`; the verifier
    // supplied is the platform's, so verification stays on.
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| internal("cannot set up TLS", e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| internal("cannot build the HTTP client", e))
}

/// Parses a provider base URL. It must be `https`, except on a loopback host,
/// where plain `http` serves local test servers.
pub(crate) fn base_url(raw: &str, origin: &str) -> Result<Url, Error> {
    let url = Url::parse(raw)
        .map_err(|e| Error::new(ErrorCode::InvalidInput, format!("{origin}: not a URL: {e}")))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err(Error::new(
            ErrorCode::InvalidInput,
            format!("{origin}: must be an https URL"),
        )),
    }
}

/// `base` followed by `path`, keeping any path `base` already has.
pub(crate) fn endpoint(base: &Url, path: &str) -> Result<Url, Error> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|()| {
            Error::new(
                ErrorCode::InvalidInput,
                format!("{base} cannot be a base URL"),
            )
        })?
        .pop_if_empty()
        .extend(path.split('/').filter(|s| !s.is_empty()));
    Ok(url)
}

/// The certificates of the PEM bundle at `path`, named by environment
/// variable `var` in errors. A missing file, unreadable PEM, or a bundle
/// without certificates fails.
pub(crate) fn ca_bundle(var: &str, path: &str) -> Result<Vec<CertificateDer<'static>>, Error> {
    let bytes = std::fs::read(Path::new(path)).map_err(|e| {
        Error::new(
            ErrorCode::InvalidInput,
            format!("{var}: cannot read the CA bundle {path}: {e}"),
        )
    })?;
    let certs = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            Error::new(
                ErrorCode::InvalidInput,
                format!("{var}: {path} is not a PEM certificate bundle: {e}"),
            )
        })?;
    if certs.is_empty() {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            format!("{var}: {path} holds no certificate"),
        ));
    }
    Ok(certs)
}

fn internal(message: &str, source: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::new(ErrorCode::Internal, format!("{message}: {source}")).with_source(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_must_be_https_except_on_loopback() {
        assert!(base_url("https://api.example.com", "test").is_ok());
        assert!(base_url("http://127.0.0.1:8080", "test").is_ok());
        assert!(base_url("http://localhost:8080/v1", "test").is_ok());
        assert!(base_url("http://[::1]:8080", "test").is_ok());
        let plain = base_url("http://api.example.com", "PLINY_BASE_URL").expect_err("plain http");
        assert!(plain.message().contains("PLINY_BASE_URL"), "{plain}");
        assert!(base_url("not a url", "test").is_err());
    }

    #[test]
    fn endpoints_keep_the_base_path() {
        let base = base_url("https://host.example/api/llm", "t").expect("valid");
        assert_eq!(
            endpoint(&base, "chat/completions").expect("joins").as_str(),
            "https://host.example/api/llm/chat/completions"
        );
        let trailing = base_url("https://host.example/api/llm/", "t").expect("valid");
        assert_eq!(
            endpoint(&trailing, "chat/completions")
                .expect("joins")
                .as_str(),
            "https://host.example/api/llm/chat/completions"
        );
        let root = base_url("https://api.example.com", "t").expect("valid");
        assert_eq!(
            endpoint(&root, "v1/messages").expect("joins").as_str(),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn a_missing_or_empty_ca_bundle_fails_naming_the_variable() {
        let missing = ca_bundle("INTERNAL_CA_BUNDLE", "/no/such/bundle.pem").expect_err("missing");
        assert!(
            missing.message().starts_with("INTERNAL_CA_BUNDLE:"),
            "{missing}"
        );

        let dir = tempfile::tempdir().expect("temp dir");
        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "").expect("write");
        let error = ca_bundle("INTERNAL_CA_BUNDLE", &empty.display().to_string())
            .expect_err("no certificate");
        assert!(error.message().contains("no certificate"), "{error}");
    }

    #[test]
    fn a_client_builds_with_and_without_extra_roots() {
        assert!(client(Duration::from_secs(5), Vec::new()).is_ok());
        let pem = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/test-ca.pem"
        ))
        .expect("test CA");
        let roots = CertificateDer::pem_slice_iter(&pem)
            .collect::<Result<Vec<_>, _>>()
            .expect("PEM");
        assert!(client(Duration::from_secs(5), roots).is_ok());
    }
}
