//! End-to-end checks for the command-line scaffold.

use std::process::{Command, Output};

fn topup(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .args(args)
        .output()
        .expect("topup process should start")
}

#[test]
fn help_and_version_succeed() {
    for args in [["--help"].as_slice(), ["--version"].as_slice()] {
        let output = topup(args);
        assert!(output.status.success(), "{args:?} should exit successfully");
    }
}

/// A service configuration around the route fixture, written to a temporary file removed on drop.
struct ConfigFile(std::path::PathBuf);

impl ConfigFile {
    fn new(origin: &str) -> Self {
        let route = include_str!("fixtures/phala-cloud-pha.yaml")
            .lines()
            .map(|line| format!("    {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let path = std::env::temp_dir().join(format!("topup-cli-{}.yaml", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            format!(
                "environment: staging\npublic_origin: {origin}\nadmin_key:\n  id: admin/v1\n  \
                 public_key: 11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=\n{rpc}\nroutes:\n  -\n{route}\n",
            rpc=include_str!("fixtures/rpc-groups.yaml")
            ),
        )
        .expect("write the configuration");
        Self(path)
    }

    fn path(&self) -> &str {
        self.0.to_str().expect("UTF-8 temporary path")
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn heartbeat_requires_a_database_url() {
    let output = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .arg("heartbeat")
        .env_remove("DATABASE_URL")
        .output()
        .expect("topup process should start");
    assert!(!output.status.success());
    assert!(output_text(&output).contains("required for heartbeat"));
}

#[test]
fn run_requires_a_database_url() {
    let config = ConfigFile::new("https://topup.example");
    let output = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .args(["run", "--config", config.path()])
        .env_remove("DATABASE_URL")
        .output()
        .expect("topup process should start");
    assert!(!output.status.success());
    assert!(output_text(&output).contains("DATABASE_URL is required for run"));
}

#[test]
fn run_refuses_an_invalid_configuration_or_origin() {
    let config = ConfigFile::new("https://topup.example/v1");
    let output = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .args(["run", "--config", config.path()])
        .env("DATABASE_URL", "postgres://unused@127.0.0.1:1/unused")
        .output()
        .expect("topup process should start");
    assert!(!output.status.success());
    let text = output_text(&output);
    assert!(
        text.contains("public origin must not include a path, query, or fragment"),
        "{text}"
    );

    let config = ConfigFile::new("https://topup.example");
    let output = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .args([
            "run",
            "--config",
            config.path(),
            "--public-origin",
            "ftp://topup.example",
        ])
        .env("DATABASE_URL", "postgres://unused@127.0.0.1:1/unused")
        .output()
        .expect("topup process should start");
    assert!(!output.status.success());
    assert!(output_text(&output).contains("invalid --public-origin"));
}

/// The restore report is part of the read-only service only.
#[test]
fn restore_report_requires_read_only() {
    let output = topup(&[
        "run",
        "--config",
        "unused.yaml",
        "--restore-report",
        "/tmp/report.json",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--read-only"));
}

/// A failed check is published to --report too, for the read-only `/healthz`.
#[test]
fn restore_check_reports_failures() {
    let config = ConfigFile::new("https://topup.example");
    let report =
        std::env::temp_dir().join(format!("topup-restore-check-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&report);
    let output = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .args(["restore-check", "--config", config.path(), "--report"])
        .arg(&report)
        .args(["--failure-at", "2026-10-06T00:00:00Z"])
        .env_remove("DATABASE_URL")
        .output()
        .expect("topup process should start");
    assert!(!output.status.success());
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).expect("report written"))
            .expect("report is JSON");
    std::fs::remove_file(&report).expect("report removed");
    assert_eq!(written["status"], "failed");
    assert_eq!(
        written["failures"][0],
        "failed to connect to the restored database"
    );
}

#[test]
fn restore_check_needs_a_failure_instant_for_an_lsn() {
    let output = topup(&[
        "restore-check",
        "--expected-lsn",
        "0/0",
        "--config",
        "unused.yaml",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--failure-at"));
}

/// `config check` and `config show` need no secret; `--secrets` checks each key against its URL
/// and prints no value.
#[test]
fn config_commands_validate_without_secrets_and_check_them_on_request() {
    let config = ConfigFile::new("https://topup.example");
    let checked = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .args(["config", "check", config.path()])
        .env_remove("TOPUP_RPC_ALCHEMY_KEY")
        .output()
        .expect("topup process should start");
    assert!(checked.status.success(), "{}", output_text(&checked));

    let shown = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .args(["config", "show", config.path()])
        .env("TOPUP_RPC_ALCHEMY_KEY", "sealed-key-0123456789")
        .output()
        .expect("topup process should start");
    assert!(shown.status.success());
    let resolved: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("JSON");
    assert_eq!(
        resolved["rpc_groups"]["alchemy"]["members"][0]["url"],
        "https://eth-mainnet.g.alchemy.com/v2/{key}"
    );
    assert_eq!(
        resolved["routes"][0]["merchant"]["quote_ttl_seconds"]["default"],
        900
    );
    assert!(!String::from_utf8_lossy(&shown.stdout).contains("sealed-key"));

    let missing = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .args(["config", "check", "--secrets", config.path()])
        .env_remove("TOPUP_RPC_ALCHEMY_KEY")
        .output()
        .expect("topup process should start");
    assert!(!missing.status.success());
    assert!(output_text(&missing).contains("TOPUP_RPC_ALCHEMY_KEY is required"));

    let sealed = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .args(["config", "check", "--secrets", config.path()])
        .env("TOPUP_RPC_ALCHEMY_KEY", "sealed-key-0123456789")
        .env_remove("TOPUP_RPC_QUICKNODE_KEY")
        .output()
        .expect("topup process should start");
    assert!(sealed.status.success(), "{}", output_text(&sealed));
    assert!(!output_text(&sealed).contains("sealed-key"));

    let staging = format!(
        "{}/../../deploy/environments/phala-network/staging/topup/topup.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    assert!(topup(&["config", "check", &staging]).status.success());
}

const ACCOUNT: &str = "acct_0123456789abcdef0123456789abcdef";

#[test]
fn attest_requires_a_hex_nonce_and_an_account() {
    let missing = topup(&["attest", "--account", ACCOUNT]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--nonce"));

    let no_account = topup(&["attest", "--nonce", "00"]);
    assert!(!no_account.status.success());
    assert!(String::from_utf8_lossy(&no_account.stderr).contains("--account"));

    let invalid = topup(&["attest", "--account", ACCOUNT, "--nonce", "not-hex"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("valid hexadecimal"));

    let empty = topup(&["attest", "--account", ACCOUNT, "--nonce", ""]);
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("non-empty hexadecimal"));

    let oversized = "ab".repeat(33);
    let oversized = topup(&["attest", "--account", ACCOUNT, "--nonce", &oversized]);
    assert!(!oversized.status.success());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("at most 32 bytes"));

    for (account, version) in [("cus_1", "1"), (ACCOUNT, "0")] {
        let output = topup(&[
            "attest",
            "--account",
            account,
            "--version",
            version,
            "--nonce",
            "00",
        ]);
        assert!(!output.status.success(), "{account} v{version}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("acct_ id"));
    }
}

#[cfg(not(feature = "dev-signer"))]
#[test]
fn dev_attestation_is_not_available_without_the_feature() {
    let output = topup(&["attest", "--account", ACCOUNT, "--nonce", "00", "--dev"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--dev'"));
}

#[cfg(feature = "dev-signer")]
#[test]
fn dev_attestation_prints_the_required_json_shape() {
    let nonce = "ab".repeat(32);
    let output = topup(&["attest", "--account", ACCOUNT, "--nonce", &nonce, "--dev"]);
    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("attestation should be JSON");
    let object = value.as_object().expect("attestation should be an object");

    assert_eq!(object.len(), 8);
    assert_eq!(value["object"], "attestation");
    assert_eq!(value["account"], ACCOUNT);
    assert_eq!(value["livemode"], false);
    assert_eq!(value["webhook_keys"][0]["version"], 1);
    assert_eq!(
        value["webhook_keys"][0]["public_key"]
            .as_str()
            .map(str::len),
        Some("whpk_".len() + 44)
    );
    assert_eq!(value["report_data"].as_str().map(str::len), Some(64));
    assert_eq!(value["tdx_quote"], "");
    assert_eq!(value["app_id"], "");
    assert_eq!(value["compose_hash"], "");
}

#[cfg(feature = "dev-signer")]
#[test]
fn dev_attestation_binds_the_nonce_account_mode_and_keys_like_the_api() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use topup_adapters::attestation::{AttestedWebhookKey, report_data};

    let output = topup(&[
        "attest",
        "--account",
        ACCOUNT,
        "--live",
        "--version",
        "2",
        "--version",
        "1",
        "--nonce",
        "00010203",
        "--dev",
    ]);
    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("attestation should be JSON");
    let keys: Vec<AttestedWebhookKey> = value["webhook_keys"]
        .as_array()
        .expect("webhook keys")
        .iter()
        .map(|key| AttestedWebhookKey {
            version: u32::try_from(key["version"].as_u64().expect("version")).expect("u32"),
            public_key: topup_core::Ed25519PublicKey(
                STANDARD
                    .decode(
                        key["public_key"]
                            .as_str()
                            .and_then(|key| key.strip_prefix("whpk_"))
                            .expect("whpk_ public key"),
                    )
                    .expect("base64 public key")
                    .try_into()
                    .expect("32-byte public key"),
            ),
        })
        .collect();
    assert_eq!(
        keys.iter().map(|key| key.version).collect::<Vec<_>>(),
        [2, 1]
    );
    assert_ne!(keys[0].public_key, keys[1].public_key);
    assert_eq!(value["livemode"], true);
    assert_eq!(
        value["report_data"],
        hex::encode(report_data(&[0, 1, 2, 3], ACCOUNT, true, &keys).expect("report data"))
    );
}

#[test]
fn migrate_requires_a_database_url() {
    let output = Command::new(env!("CARGO_BIN_EXE_topup"))
        .env("TOPUP_RPC_ALCHEMY_KEY", "test-sealed-0123456789")
        .arg("migrate")
        .env_remove("DATABASE_URL")
        .output()
        .expect("topup process should start");
    assert!(!output.status.success());
    assert!(output_text(&output).contains("DATABASE_URL is required for migrate"));
}

#[test]
fn route_validate_accepts_the_valid_fixture() {
    let route = format!(
        "{}/tests/fixtures/phala-cloud-pha.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = topup(&["route", "validate", &route]);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("valid at schema level"));
    assert!(stdout.contains("on-chain deployment and Safe control were not checked"));
}

#[test]
fn route_validate_requires_template_mode_for_placeholders() {
    let template = format!(
        "{}/../../examples/phala-cloud-pha.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let normal_output = topup(&["route", "validate", &template]);
    assert!(!normal_output.status.success());
    assert!(String::from_utf8_lossy(&normal_output.stderr).contains("forwarder_factory"));

    let template_output = topup(&["route", "validate", "--template", &template]);
    assert!(template_output.status.success());
    assert!(String::from_utf8_lossy(&template_output.stdout).contains("route template"));
}

#[test]
fn route_show_prints_the_resolved_route_as_json() {
    let route = format!(
        "{}/tests/fixtures/phala-cloud-pha.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = topup(&["route", "show", &route]);
    assert!(output.status.success());
    let resolved: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("route show prints JSON");
    assert_eq!(resolved["merchant"]["quote_ttl_seconds"]["default"], 900);

    let invalid = topup(&["route", "show", "/definitely/missing/route.yaml"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("failed to read"));
}

#[test]
fn route_validate_rejects_invalid_content_and_missing_files() {
    let invalid = format!(
        "{}/../../contracts/test-vectors/create2.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let invalid_output = topup(&["route", "validate", &invalid]);
    assert!(!invalid_output.status.success());
    assert!(String::from_utf8_lossy(&invalid_output.stderr).contains("is invalid"));

    let missing_output = topup(&["route", "validate", "/definitely/missing/route.yaml"]);
    assert!(!missing_output.status.success());
    assert!(String::from_utf8_lossy(&missing_output.stderr).contains("failed to read"));
}

#[test]
fn restore_check_help_requires_a_stopped_service() {
    let output = topup(&["restore-check", "--help"]);
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--config"));
    assert!(help.contains("processes are stopped"));
}
