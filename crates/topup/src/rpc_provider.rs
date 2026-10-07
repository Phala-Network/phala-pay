//! RPC endpoint URL templates (`rpc`, docs/configuration.md).
//!
//! Each public, attested template names an endpoint and optionally `{key}`. The typed endpoint
//! names its sealed `TOPUP_RPC_*_KEY` explicitly, so credentials never enter public config.
//!
//! The placeholder may only be a whole path segment or a whole query value, and substituting the
//! key must leave the scheme, user information, host, and port as the template has them: a sealed
//! secret can choose a credential, never the server that receives the requests.

use serde::Serialize;

/// Where an attested provider URL takes its owner-sealed key.
pub const KEY_PLACEHOLDER: &str = "{key}";
/// Shortest accepted key: every provider key is longer, and the redaction of node messages
/// scrubs URL path segments only from this length on.
const MIN_KEY_CHARS: usize = 8;
/// Stands in for the placeholder while a template is parsed as a URL: unreserved characters only,
/// so the URL parser neither rejects nor percent-encodes it.
const MARKER: &str = "topup-rpc-key-placeholder";

/// A validated provider URL: `http` or `https`, no user information or fragment, and at most one
/// `{key}`, which is a whole path segment or a whole query value.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProviderUrl(String);

impl ProviderUrl {
    /// Validates a configured URL template. Errors are log-safe: they never repeat the URL.
    pub fn parse(template: &str) -> Result<Self, &'static str> {
        let occurrences = template.matches(KEY_PLACEHOLDER).count();
        if occurrences > 1 {
            return Err("has more than one {key} placeholder");
        }
        if template.contains(MARKER) {
            return Err("contains a reserved word");
        }
        let url = url::Url::parse(&template.replace(KEY_PLACEHOLDER, MARKER))
            .map_err(|_| "is not an absolute URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("must use http or https");
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err("must include a host");
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("must not include user information; put the key in {key}");
        }
        if url.fragment().is_some() {
            return Err("must not include a fragment");
        }
        if occurrences == 1 && placeholder_positions(&url) != 1 {
            return Err("may have {key} only as a whole path segment or a whole query value");
        }
        Ok(Self(template.to_owned()))
    }

    /// The template as configured, with `{key}` in place of a key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the URL takes a sealed key.
    #[must_use]
    pub fn is_keyed(&self) -> bool {
        self.0.contains(KEY_PLACEHOLDER)
    }

    /// The URL with `key` in place of `{key}`. A keyless URL takes no key. The key must be URL-safe
    /// (RFC 3986 unreserved characters), and the result must reach the template's own server.
    pub fn resolve(&self, key: Option<&str>) -> Result<String, &'static str> {
        match (self.is_keyed(), key) {
            (false, None) => Ok(self.0.clone()),
            (false, Some(_)) => Err("is set, but the URL has no {key} placeholder"),
            (true, None) => Err("is required by the {key} placeholder of the URL"),
            (true, Some(key))
                if key.len() >= MIN_KEY_CHARS
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)) =>
            {
                let resolved = self.0.replace(KEY_PLACEHOLDER, key);
                let template = url::Url::parse(&self.0.replace(KEY_PLACEHOLDER, MARKER));
                match (template, url::Url::parse(&resolved)) {
                    (Ok(template), Ok(url)) if same_server(&template, &url) => Ok(resolved),
                    _ => Err("would change the URL's server"),
                }
            }
            (true, Some(_)) => Err("must be at least 8 characters of A-Z, a-z, 0-9, and -._~"),
        }
    }
}

/// The places the marker occupies in a parsed template: whole path segments and whole query
/// values count; an occurrence anywhere else (host, port, part of a segment, a query name) makes
/// the count differ from the template's single occurrence.
fn placeholder_positions(url: &url::Url) -> usize {
    let serialized = url.as_str().matches(MARKER).count();
    let segments = url.path_segments().map_or(0, |segments| {
        segments.filter(|segment| *segment == MARKER).count()
    });
    let values = url.query().map_or(0, |query| {
        query
            .split('&')
            .filter(|pair| {
                pair.split_once('=')
                    .is_some_and(|(_, value)| value == MARKER)
            })
            .count()
    });
    if serialized == segments + values {
        serialized
    } else {
        0
    }
}

fn same_server(template: &url::Url, resolved: &url::Url) -> bool {
    template.scheme() == resolved.scheme()
        && template.host_str() == resolved.host_str()
        && template.port_or_known_default() == resolved.port_or_known_default()
        && template.username() == resolved.username()
        && template.password() == resolved.password()
}

/// Returns the environment-variable name `TOPUP_RPC_<ID>_KEY` of one provider id's sealed key.
#[must_use]
pub fn key_environment(provider_id: &str) -> String {
    let normalized = provider_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("TOPUP_RPC_{normalized}_KEY")
}

/// The sealed key of a provider id from the process environment; unset and empty are both none.
#[must_use]
pub fn environment_key(provider_id: &str) -> Option<String> {
    std::env::var(key_environment(provider_id))
        .ok()
        .filter(|value| !value.is_empty())
}

/// Returns a log-safe label for the provider entry at `index` in `chain.rpc_providers`.
///
/// Provider ids are used as-is; an inline URL entry (tests only) is named by its position so its
/// host, path, and credentials never reach an error or log line.
pub(crate) fn provider_label(provider: &str, index: usize) -> String {
    if provider.contains("://") {
        format!("rpc_providers[{index}]")
    } else {
        provider.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_name_their_sealed_key() {
        assert_eq!(key_environment("provider-a"), "TOPUP_RPC_PROVIDER_A_KEY");
        assert_eq!(
            key_environment("quick-node.eu"),
            "TOPUP_RPC_QUICK_NODE_EU_KEY"
        );
        assert_eq!(provider_label("provider-a", 0), "provider-a");
        assert_eq!(
            provider_label("https://user:secret@rpc.example/v1?key=secret", 1),
            "rpc_providers[1]"
        );
    }

    #[test]
    fn keys_fill_the_placeholder_where_each_provider_documents_it() {
        let key = "AbC123_-.~xyz";
        for (template, expected) in [
            (
                "https://eth-mainnet.g.alchemy.com/v2/{key}",
                "https://eth-mainnet.g.alchemy.com/v2/AbC123_-.~xyz",
            ),
            (
                "https://mainnet.infura.io/v3/{key}",
                "https://mainnet.infura.io/v3/AbC123_-.~xyz",
            ),
            (
                "https://name.quiknode.pro/{key}/",
                "https://name.quiknode.pro/AbC123_-.~xyz/",
            ),
            (
                "https://lb.drpc.org/ogrpc?network=ethereum&dkey={key}",
                "https://lb.drpc.org/ogrpc?network=ethereum&dkey=AbC123_-.~xyz",
            ),
            (
                "http://anvil:8545/?key={key}",
                "http://anvil:8545/?key=AbC123_-.~xyz",
            ),
        ] {
            let url = ProviderUrl::parse(template).expect(template);
            assert!(url.is_keyed());
            assert_eq!(url.resolve(Some(key)).as_deref(), Ok(expected));
        }
        let keyless = ProviderUrl::parse("https://rpc.example/sepolia").expect("keyless");
        assert!(!keyless.is_keyed());
        assert_eq!(
            keyless.resolve(None).as_deref(),
            Ok("https://rpc.example/sepolia")
        );
    }

    #[test]
    fn the_placeholder_never_chooses_the_server() {
        for template in [
            "https://{key}/rpc",
            "https://{key}.alchemy.com/v2",
            "https://rpc.{key}/v2",
            "https://rpc.example:{key}/v2",
            "https://{key}@rpc.example/v2",
            "https://user:{key}@rpc.example/v2",
            "{key}://rpc.example/v2",
            "https://rpc.example/v2/prefix-{key}",
            "https://rpc.example/v2?{key}=1",
            "https://rpc.example/v2?key={key}{key}",
            "https://rpc.example/v2/{key}/{key}",
            "https://rpc.example/v2#{key}",
            "https://rpc.example/v2/{key}#fragment",
        ] {
            assert!(ProviderUrl::parse(template).is_err(), "{template}");
        }
        for template in [
            "ftp://rpc.example/v2",
            "rpc.example/v2",
            "https://user:pass@rpc.example/v2",
            "https://:443/v2",
        ] {
            assert!(ProviderUrl::parse(template).is_err(), "{template}");
        }
    }

    #[test]
    fn a_key_must_match_its_url_and_stay_inside_its_position() {
        let keyed = ProviderUrl::parse("https://rpc.example/v2/{key}").expect("keyed");
        assert!(keyed.resolve(None).is_err());
        let keyless = ProviderUrl::parse("https://rpc.example/sepolia").expect("keyless");
        assert!(keyless.resolve(Some("0123456789abcdef")).is_err());
        // A key that could move the request elsewhere, or too short to be scrubbed from logs.
        for key in [
            "short",
            "0123456789@evil.example",
            "01234567/../x",
            "0123456789?x=1",
            "0123456789#x",
            "01234567:8080",
        ] {
            assert!(keyed.resolve(Some(key)).is_err(), "{key}");
        }
    }
}
