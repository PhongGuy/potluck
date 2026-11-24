//! The validated `--oidc-*` flags and the client secret read from its file.

use std::io::Read;
use std::path::Path;

use url::Url;

use crate::config::{ConfigError, SchemeCheck, check_scheme, redact_userinfo};

/// The longest `--oidc-name`, in characters.
const MAX_NAME_CHARS: usize = 64;

/// The largest client secret file read, in bytes; real secrets are a few hundred at most.
const MAX_SECRET_BYTES: u64 = 4096;

/// The OIDC client secret; its `Debug` never shows the value.
#[derive(Clone)]
pub struct ClientSecret(String);

impl ClientSecret {
    /// The secret itself, for the token request only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ClientSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientSecret([redacted])")
    }
}

/// One OpenID provider as configured by the `--oidc-*` flags.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OidcSettings {
    issuer: String,
    client_id: String,
    client_secret: ClientSecret,
    name: String,
    jit: bool,
}

impl OidcSettings {
    /// Validates the five OIDC flags: issuer, client id and secret file come together or not at
    /// all, and name or jit alone count as a partial set. `None` when none of the five is set.
    /// Errors are [`ConfigError`] variants naming the flag at fault, never the secret.
    pub fn from_flags(
        issuer: Option<&str>,
        client_id: Option<&str>,
        secret_file: Option<&Path>,
        name: Option<&str>,
        jit: bool,
        insecure_http: bool,
    ) -> Result<Option<OidcSettings>, ConfigError> {
        let any_set = issuer.is_some() || client_id.is_some() || secret_file.is_some();
        if !any_set && name.is_none() && !jit {
            return Ok(None);
        }
        let client_id = client_id.filter(|id| !id.trim().is_empty());
        let (Some(issuer), Some(client_id), Some(secret_file)) = (issuer, client_id, secret_file)
        else {
            let missing: Vec<&str> = [
                (issuer.is_none(), "--oidc-issuer"),
                (client_id.is_none(), "--oidc-client-id"),
                (secret_file.is_none(), "--oidc-client-secret-file"),
            ]
            .into_iter()
            .filter_map(|(absent, flag)| absent.then_some(flag))
            .collect();
            return Err(ConfigError::OidcIncomplete {
                missing: missing.join(", "),
            });
        };
        let host = validate_issuer(issuer, insecure_http)?;
        let client_secret = read_secret(secret_file)?;
        let name = match name {
            Some(name) => {
                let trimmed = name.trim();
                if !(1..=MAX_NAME_CHARS).contains(&trimmed.chars().count()) {
                    return Err(ConfigError::InvalidOidcName);
                }
                trimmed.to_owned()
            }
            None => host,
        };
        Ok(Some(OidcSettings {
            issuer: issuer.to_owned(),
            client_id: client_id.to_owned(),
            client_secret,
            name,
            jit,
        }))
    }

    /// The issuer URL exactly as given, trailing slash included, since discovery compares it
    /// byte for byte.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The client id registered with the provider.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The client secret read from `--oidc-client-secret-file`.
    pub fn client_secret(&self) -> &ClientSecret {
        &self.client_secret
    }

    /// The provider's name on the apps' sign-in button.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the first sign-in of an unlinked subject creates an account.
    pub fn jit(&self) -> bool {
        self.jit
    }
}

/// Refuses an issuer that is malformed, carries credentials, a query or a fragment, is not
/// http(s), or is plain http off loopback without `--insecure-http`. Returns its host.
fn validate_issuer(raw: &str, insecure_http: bool) -> Result<String, ConfigError> {
    let invalid = |reason: &str| ConfigError::InvalidOidcIssuer {
        url: redact_userinfo(raw),
        reason: reason.to_owned(),
    };
    let url = Url::parse(raw).map_err(|e| invalid(&e.to_string()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("must not carry a username or password"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("must not have a query or fragment"));
    }
    match check_scheme(&url, insecure_http) {
        SchemeCheck::Accepted => {}
        SchemeCheck::Insecure => {
            return Err(ConfigError::InsecureOidcIssuer {
                url: redact_userinfo(raw),
            });
        }
        SchemeCheck::Unsupported => return Err(invalid("must use http or https")),
    }
    // http and https URLs always parse with a host.
    Ok(url.host_str().unwrap_or_default().to_owned())
}

/// Reads the secret, dropping one trailing `\n` and then one `\r` before it. The error's
/// reason is a fixed phrase or the io error kind, never anything read from the file.
fn read_secret(path: &Path) -> Result<ClientSecret, ConfigError> {
    let refuse = |reason: String| ConfigError::OidcSecretFile {
        path: path.to_owned(),
        reason,
    };
    // Read one byte past the cap so an oversized or endless file is refused without loading it.
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_SECRET_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| refuse(e.kind().to_string()))?;
    if bytes.len() as u64 > MAX_SECRET_BYTES {
        return Err(refuse(format!("is larger than {MAX_SECRET_BYTES} bytes")));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| refuse(std::io::ErrorKind::InvalidData.to_string()))?;
    let text = match text.strip_suffix('\n') {
        Some(line) => line.strip_suffix('\r').unwrap_or(line),
        None => &text,
    };
    if text.is_empty() {
        return Err(refuse("is empty".to_owned()));
    }
    Ok(ClientSecret(text.to_owned()))
}
