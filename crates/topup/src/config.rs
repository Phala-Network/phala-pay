//! The service configuration file (`--config`, docs/configuration.md): the public settings of one
//! deployment, attested with the compose that inlines it.
//!
//! [`Config::parse`] is the only validation of the file. It reads no secret, so `topup config
//! check` and `config show` run where no key exists (CI, a first provisioning, Deploy's unsealed
//! preflight), and a keyed provider's URL keeps its `{key}`. The keys are resolved only by
//! [`Config::route_set`] (the service) and [`Config::check_secrets`] (a preflight that holds them),
//! both through [`ProviderUrl::resolve`].
//!
//! The public origin and the admin public key may be left out of the file only for `topup run` to
//! take them from its environment ([`Config::runtime_public_origin`],
//! [`Config::runtime_admin_key`]): the Phala Cloud template (deploy/compose.template.yaml), whose
//! deploy form holds them. Each has exactly one source; neither or both refuses to start.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use topup_core::route::RouteFile;

use crate::api::{PublicOrigin, VerificationKey};
use crate::chain_rpc::ChainRpcSpec;
use crate::routes::RouteSet;
use crate::rpc_provider::ProviderUrl;

/// The file as written. Maintenance keys default to empty; the two runtime-resolved public
/// settings may be omitted. Other fields are required and unknown fields are errors.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigSpec {
    environment: String,
    #[serde(default)]
    public_origin: Option<String>,
    admin_key: AdminKeySpec,
    #[serde(default)]
    maintenance_keys: Vec<MaintenanceKeySpec>,
    rpc: Vec<ChainRpcSpec>,
    routes: Vec<RouteFile>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AdminKeySpec {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    public_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MaintenanceKeySpec {
    id: String,
    public_key: String,
}

/// A validated service configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// The deployment's name, reported to Sentry (`<environment>-restore` while read-only). Explicit non-production names
    /// permit staging-only price opt-in; every other name requires Allowed licensing.
    pub environment: String,
    /// The API's one public origin: admin signatures and treasury challenges name it. `None` when
    /// `topup run` takes it from its environment.
    pub public_origin: Option<PublicOrigin>,
    /// The operator's admin key id.
    pub admin_key_id: String,
    /// The operator's admin verification key. `None` when `topup run` takes it from its
    /// environment.
    pub admin_key: Option<VerificationKey>,
    /// Optional, distinct signing keys authorized only to start/clear instance maintenance.
    pub maintenance_keys: Vec<VerificationKey>,
    /// Read and verify endpoints for route and price chains.
    pub rpc: Vec<ChainRpcSpec>,
    /// Every enabled route version.
    pub routes: Vec<RouteFile>,
    admin_key_spec: AdminKeySpec,
    maintenance_key_specs: Vec<MaintenanceKeySpec>,
}

impl std::fmt::Debug for AdminKeySpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdminKeySpec")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The resolved configuration as `topup config show` prints it: every route default written out.
#[derive(Serialize)]
struct ResolvedConfig<'a> {
    environment: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_origin: Option<String>,
    admin_key: &'a AdminKeySpec,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    maintenance_keys: &'a Vec<MaintenanceKeySpec>,
    rpc: &'a [ChainRpcSpec],
    routes: &'a [RouteFile],
}

impl Config {
    /// Reads and validates a configuration file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let yaml = std::fs::read_to_string(path)
            .map_err(|error| format!("failed to read `{}`: {error}", path.display()))?;
        Self::parse(&yaml)
            .map_err(|error| format!("invalid configuration `{}`: {error}", path.display()))
    }

    /// Parses and validates a configuration, with no secret: see the module documentation.
    pub fn parse(yaml: &str) -> Result<Self, String> {
        let spec: ConfigSpec =
            serde_saphyr::from_str(yaml).map_err(|error| format!("invalid YAML: {error}"))?;
        if !is_name(&spec.environment, 64) {
            return Err(
                "environment must be 1-64 lowercase letters, digits, `-`, or `_`, starting with a \
                 letter or digit"
                    .to_owned(),
            );
        }
        let public_origin = spec
            .public_origin
            .as_deref()
            .map(PublicOrigin::parse)
            .transpose()
            .map_err(|error| format!("public_origin: {error}"))?;
        let admin_key = spec
            .admin_key
            .public_key
            .as_deref()
            .map(|key| VerificationKey::from_base64(spec.admin_key.id.clone(), key))
            .transpose()
            .map_err(|error| format!("admin_key.public_key: {error}"))?;
        if spec.admin_key.id.is_empty() || spec.admin_key.id.chars().any(char::is_whitespace) {
            return Err("admin_key.id must be a non-empty key id without spaces".to_owned());
        }
        if spec.maintenance_keys.len() > 8 {
            return Err("maintenance_keys must contain at most 8 keys".to_owned());
        }
        let mut maintenance_keys: Vec<VerificationKey> = Vec::new();
        for entry in &spec.maintenance_keys {
            if entry.id.is_empty()
                || entry.id.len() > 128
                || !entry
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
            {
                return Err(
                    "maintenance_keys.id must be 1-128 ASCII letters, digits, . _ / or -"
                        .to_owned(),
                );
            }
            let key = VerificationKey::from_base64(entry.id.clone(), &entry.public_key)
                .map_err(|error| format!("maintenance_keys.public_key: {error}"))?;
            if key.kid == spec.admin_key.id
                || admin_key
                    .as_ref()
                    .is_some_and(|admin| admin.same_public_key(&key))
                || maintenance_keys
                    .iter()
                    .any(|other| other.kid == key.kid || other.same_public_key(&key))
            {
                return Err("maintenance_keys must have distinct ids and public keys, separate from admin_key".to_owned());
            }
            maintenance_keys.push(key);
        }
        crate::chain_rpc::validate(&spec.rpc, &spec.routes)?;
        if spec.routes.is_empty() {
            return Err("routes must list at least one route".to_owned());
        }
        RouteSet::check(&spec.routes)?;
        for route in &spec.routes {
            route
                .pricing
                .validate_licensing(matches!(
                    spec.environment.as_str(),
                    "staging" | "testnet" | "local" | "sandbox"
                ))
                .map_err(|e| e.to_string())?;
        }

        Ok(Self {
            environment: spec.environment,
            public_origin,
            admin_key_id: spec.admin_key.id.clone(),
            admin_key,
            maintenance_keys,
            rpc: spec.rpc,
            routes: spec.routes,
            admin_key_spec: spec.admin_key,
            maintenance_key_specs: spec.maintenance_keys,
        })
    }

    /// The public origin `topup run` serves: the file's, or, with `host_variable` (the template's
    /// `--public-origin-host-env`), `https://` and the host that variable holds, read through
    /// `env`. Exactly one of the two; the host must be a lowercase DNS name and nothing else.
    pub fn runtime_public_origin(
        &self,
        host_variable: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<PublicOrigin, String> {
        match (&self.public_origin, host_variable) {
            (Some(origin), None) => Ok(origin.clone()),
            (None, Some(name)) => {
                let host = env(name).unwrap_or_default();
                if !is_host_name(&host) {
                    return Err(format!(
                        "{name} must be the public origin's host, a lowercase DNS name"
                    ));
                }
                PublicOrigin::parse(&format!("https://{host}"))
                    .map_err(|error| format!("{name}: {error}"))
            }
            (Some(_), Some(name)) => Err(format!(
                "public_origin is set both in the configuration and by {name}; set it in one place"
            )),
            (None, None) => Err("public_origin is not set in the configuration".to_owned()),
        }
    }

    /// The admin verification key `topup run` uses: the file's, or, with `key_variable` (the
    /// template's `--admin-public-key-env`), the standard base64 ed25519 public key that variable
    /// holds, read through `env`. Exactly one of the two.
    pub fn runtime_admin_key(
        &self,
        key_variable: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<VerificationKey, String> {
        let key = match (&self.admin_key, key_variable) {
            (Some(key), None) => Ok(key.clone()),
            (None, Some(name)) => VerificationKey::from_base64(
                self.admin_key_id.clone(),
                &env(name).unwrap_or_default(),
            )
            .map_err(|error| format!("{name}: admin {error}")),
            (Some(_), Some(name)) => Err(format!(
                "admin_key.public_key is set both in the configuration and by {name}; set it in \
                 one place"
            )),
            (None, None) => Err("admin_key.public_key is not set in the configuration".to_owned()),
        }?;
        if self
            .maintenance_keys
            .iter()
            .any(|other| other.same_public_key(&key))
        {
            return Err("maintenance_keys must use public keys separate from admin_key".to_owned());
        }
        Ok(key)
    }

    /// The routes with each provider's client, its key read from `TOPUP_RPC_<ID>_KEY`.
    pub fn route_set(&self) -> Result<RouteSet, String> {
        self.check_secrets(|name| std::env::var(name).ok())?;
        let mut clients = BTreeMap::new();
        for rpc in &self.rpc {
            clients.insert(rpc.chain_id, rpc.clients(|name| std::env::var(name).ok())?);
        }
        RouteSet::with_rpc(self.routes.clone(), clients)
            .map(|routes| routes.with_environment(self.environment.clone()))
    }

    /// Require every explicit sealed key, with URL-safe substitution and log-safe errors.
    pub fn check_secrets(&self, key: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        for rpc in &self.rpc {
            for endpoint in [&rpc.read, &rpc.verify] {
                let value = key(&endpoint.sealed_key).filter(|v| !v.is_empty());
                ProviderUrl::parse(&endpoint.url)
                    .map_err(str::to_owned)?
                    .resolve(value.as_deref())
                    .map_err(|e| {
                        format!("{} {e} (endpoint {})", endpoint.sealed_key, endpoint.id)
                    })?;
            }
        }
        Ok(())
    }

    /// Checks sealed keys by their explicit environment names.
    pub fn check_environment_secrets(&self) -> Result<(), String> {
        self.check_secrets(|name| std::env::var(name).ok())
    }

    /// The resolved configuration as pretty JSON, which is also a valid configuration file.
    pub fn resolved_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(&ResolvedConfig {
            environment: &self.environment,
            public_origin: self.public_origin.as_ref().map(ToString::to_string),
            admin_key: &self.admin_key_spec,
            maintenance_keys: &self.maintenance_key_specs,
            rpc: &self.rpc,
            routes: &self.routes,
        })
        .map(|json| json + "\n")
        .map_err(|error| format!("failed to write the configuration: {error}"))
    }
}

/// A lowercase DNS name of at least two labels: letters, digits, and inner `-`, at most 253
/// characters; no port, path, or anything else.
fn is_host_name(value: &str) -> bool {
    value.len() <= 253
        && value.split('.').count() >= 2
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

/// Lowercase letters, digits, `-`, and `_`, starting with a letter or digit.
fn is_name(value: &str, max: usize) -> bool {
    value.len() <= max
        && value
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTE: &str = include_str!("../tests/fixtures/phala-cloud-pha.yaml");
    const KEY: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";

    const PROVIDERS: &str = include_str!("../tests/fixtures/chain-rpc.yaml");
    fn config(providers: &str, origin: &str) -> String {
        let route = ROUTE
            .lines()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "environment: staging\npublic_origin: {origin}\nadmin_key:\n  id: admin/staging-v1\n  public_key: {KEY}\n{providers}\nroutes:\n  -\n{route}\n"
        )
    }

    #[test]
    fn maintenance_keys_validate_and_round_trip_with_rotation() {
        use base64::Engine as _;
        let encode = |seed| {
            base64::engine::general_purpose::STANDARD.encode(
                ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
                    .verifying_key()
                    .as_bytes(),
            )
        };
        let base = config(PROVIDERS, "https://pay.example");
        let yaml = format!(
            "{base}maintenance_keys:\n  - id: maintenance/v1\n    public_key: {}\n  - id: maintenance/v2\n    public_key: {}\n",
            encode(7),
            encode(8)
        );
        let parsed = Config::parse(&yaml).expect("two independent rotation keys");
        let shown = parsed.resolved_json().expect("show");
        assert_eq!(
            Config::parse(&shown)
                .expect("round trip")
                .maintenance_keys
                .len(),
            2
        );
        for invalid in [
            yaml.replace("maintenance/v2", "maintenance/v1"),
            yaml.replace("maintenance/v2", "admin/staging-v1"),
            yaml.replace(&encode(8), &encode(7)),
            yaml.replace(&encode(7), KEY),
            yaml.replace("maintenance/v1", "maintenance bad"),
            yaml.replace(&encode(7), "invalid"),
            yaml.replace(&format!("public_key: {}", encode(7)), "unexpected: value"),
        ] {
            assert!(Config::parse(&invalid).is_err());
        }
        let entries = (1..=9)
            .map(|i| format!("  - id: maintenance/v{i}\n    public_key: {}\n", encode(i)))
            .collect::<String>();
        assert!(Config::parse(&format!("{base}maintenance_keys:\n{entries}")).is_err());
        // An admin key supplied by the deployment template must also remain independent.
        let template = Config::parse(&yaml.replace(&format!("  public_key: {KEY}\n"), ""))
            .expect("runtime admin");
        assert!(
            template
                .runtime_admin_key(Some("ADMIN"), |_| Some(encode(7)))
                .is_err()
        );
        assert!(
            template
                .runtime_admin_key(Some("ADMIN"), |_| Some(encode(9)))
                .is_ok()
        );
        assert!(
            Config::parse(&base)
                .expect("existing config")
                .maintenance_keys
                .is_empty()
        );
    }

    #[test]
    fn a_valid_configuration_parses_without_any_secret() {
        let parsed = Config::parse(&config(PROVIDERS, "https://pay.example")).expect("valid");
        assert_eq!(
            parsed
                .public_origin
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("https://pay.example")
        );
        assert_eq!(
            parsed.admin_key.as_ref().map(|key| key.kid.as_str()),
            Some("admin/staging-v1")
        );
        let shown = parsed.resolved_json().expect("show");
        assert!(shown.contains("https://rpc.ankr.com/eth/{key}"));
        let reparsed = Config::parse(&shown).expect("show prints a valid configuration");
        assert_eq!(
            serde_json::to_value(reparsed.rpc).unwrap(),
            serde_json::to_value(parsed.rpc).unwrap()
        );
    }

    #[test]
    fn secrets_are_checked_only_on_request_and_by_the_same_rule() {
        let parsed = Config::parse(&config(PROVIDERS, "https://pay.example")).unwrap();
        let missing = parsed.check_secrets(|_| None).unwrap_err();
        assert!(missing.contains("TOPUP_RPC_ANKR_KEY"), "{missing}");
        let missing_verify = parsed
            .check_secrets(|key| (key == "TOPUP_RPC_ANKR_KEY").then(|| "fixture-key".to_owned()))
            .unwrap_err();
        assert!(
            missing_verify.contains("TOPUP_RPC_INFURA_KEY"),
            "{missing_verify}"
        );
        parsed
            .check_secrets(|_| Some("fixture-key".to_owned()))
            .unwrap();
    }

    #[test]
    fn invalid_configurations_are_refused_with_the_reason() {
        for (yaml, reason) in [
            (config(PROVIDERS, "https://pay.example/v1"), "public_origin"),
            (
                config(
                    &PROVIDERS.replace("https://rpc.ankr.com/eth/{key}", "https://{key}/v2"),
                    "https://pay.example",
                ),
                "whole path segment",
            ),
            (
                config(
                    &PROVIDERS.replace("chain_id: 1", "chain_id: 2"),
                    "https://pay.example",
                ),
                "every route and price chain",
            ),
            (
                config(
                    &PROVIDERS.replace("mainnet.infura.io", "rpc.ankr.com"),
                    "https://pay.example",
                ),
                "different hosts",
            ),
            (
                config(PROVIDERS, "https://pay.example")
                    .replace("environment: staging", "extra: 1\nenvironment: staging"),
                "unknown field",
            ),
        ] {
            let error = Config::parse(&yaml).unwrap_err();
            assert!(error.contains(reason), "{reason}: {error}");
        }
    }

    /// Every committed configuration validates without secrets, as CI and Deploy's unsealed
    /// preflight run it.
    #[test]
    fn every_committed_configuration_validates() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        let mut directories = vec![root.join("deploy/environments")];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(&directory).expect("environments directory") {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    directories.push(path);
                } else if path.file_name().is_some_and(|name| name == "topup.yaml") {
                    files.push(path);
                }
            }
        }
        assert!(files.len() >= 2, "{files:?}");
        for file in files {
            Config::load(&file).unwrap_or_else(|error| panic!("{error}"));
        }
    }

    #[test]
    fn committed_stablecoin_defaults_are_production_eligible() {
        for yaml in [
            include_str!("../../../deploy/environments/phala-network/staging/topup/topup.yaml"),
            include_str!("../../../deploy/environments/phala-cloud-template/topup/topup.yaml"),
        ] {
            let config = Config::parse(yaml).unwrap();
            let mut resolved: serde_json::Value =
                serde_json::from_str(&config.resolved_json().unwrap()).unwrap();
            resolved["environment"] = serde_json::json!("production");
            let routes = resolved["routes"].as_array_mut().unwrap();
            routes.retain(|r| r["price"]["mode"] == "stablecoin");
            assert_eq!(routes.len(), 4);
            let production = Config::parse(&serde_json::to_string(&resolved).unwrap()).unwrap();
            for route in production.routes {
                assert!(!route.pricing.allow_unclear_sources);
                assert!(
                    route.pricing.sources.iter().all(|source| matches!(
                        source,
                        topup_core::price::Source::Chainlink { .. }
                    ))
                );
            }
        }
    }

    /// The Phala Cloud template leaves the origin and the admin key to `topup run`'s environment
    /// (deploy/compose.template.yaml): each must then come from exactly one place and be well
    /// formed, or topup refuses to start.
    #[test]
    fn runtime_settings_have_one_source_and_are_parsed_strictly() {
        const DOMAIN: &str = "0123abcd.dstack-pha-prod5.phala.network";
        let template = Config::parse(include_str!(
            "../../../deploy/environments/phala-cloud-template/topup/topup.yaml"
        ))
        .expect("the template's configuration");
        assert!(template.public_origin.is_none() && template.admin_key.is_none());
        let env = |host: &'static str, key: &'static str| {
            move |name: &str| match name {
                "DSTACK_APP_DOMAIN" => Some(host.to_owned()),
                "TOPUP_ADMIN_PUBLIC_KEY" => Some(key.to_owned()),
                _ => None,
            }
        };
        let origin = template
            .runtime_public_origin(Some("DSTACK_APP_DOMAIN"), env(DOMAIN, KEY))
            .expect("origin");
        assert_eq!(origin.to_string(), format!("https://{DOMAIN}"));
        let key = template
            .runtime_admin_key(Some("TOPUP_ADMIN_PUBLIC_KEY"), env(DOMAIN, KEY))
            .expect("key");
        assert_eq!(key.kid, "admin/v1");
        for host in [
            "",
            "localhost",
            "pay.example:443",
            "pay.example/path",
            "Pay.example",
            "pay.example\npublic_origin: https://evil.example",
            "-pay.example",
        ] {
            let error = template
                .runtime_public_origin(Some("DSTACK_APP_DOMAIN"), env(host, KEY))
                .expect_err(host);
            assert!(error.starts_with("DSTACK_APP_DOMAIN "), "{host}: {error}");
        }
        for key in [
            "",
            "not-a-key",
            " 11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=",
        ] {
            let error = template
                .runtime_admin_key(Some("TOPUP_ADMIN_PUBLIC_KEY"), env(DOMAIN, key))
                .expect_err(key);
            assert!(
                error.starts_with("TOPUP_ADMIN_PUBLIC_KEY: "),
                "{key}: {error}"
            );
        }
        assert!(
            template
                .runtime_public_origin(None, env(DOMAIN, KEY))
                .is_err()
        );
        assert!(template.runtime_admin_key(None, env(DOMAIN, KEY)).is_err());
        // A configuration that writes them keeps them: an environment variable cannot replace them.
        let written = Config::parse(&config(PROVIDERS, "https://pay.example")).expect("valid");
        assert!(
            written
                .runtime_public_origin(Some("DSTACK_APP_DOMAIN"), env(DOMAIN, KEY))
                .expect_err("two sources")
                .contains("in one place")
        );
        assert!(
            written
                .runtime_admin_key(Some("TOPUP_ADMIN_PUBLIC_KEY"), env(DOMAIN, KEY))
                .expect_err("two sources")
                .contains("in one place")
        );
        assert_eq!(
            written
                .runtime_public_origin(None, |_| None)
                .expect("the file's")
                .to_string(),
            "https://pay.example"
        );
    }
}
