//! YAML parsing at the command-line I/O boundary.

use topup_core::route::RouteFile;

pub(crate) fn parse_and_validate(yaml: &str, template: bool) -> Result<RouteFile, String> {
    let route: RouteFile =
        serde_saphyr::from_str(yaml).map_err(|error| format!("invalid route YAML: {error}"))?;
    let validation = if template {
        route.validate_template()
    } else {
        route.validate()
    };
    validation.map_err(|error| error.to_string())?;
    Ok(route)
}

/// The resolved route as JSON, which is also a YAML route file: every default written out,
/// parsing back to the same route.
pub(crate) fn resolved_json(route: &RouteFile) -> Result<String, String> {
    serde_json::to_string_pretty(route)
        .map(|json| json + "\n")
        .map_err(|error| format!("failed to write the route: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = include_str!("../tests/fixtures/phala-cloud-pha.yaml");
    const TEMPLATE: &str = include_str!("../../../examples/phala-cloud-pha.yaml");
    const USDT_TEMPLATE: &str = include_str!("../../../examples/phala-cloud-usdt.yaml");
    const STAGING: &str =
        include_str!("../../../deploy/environments/phala-network/staging/topup/topup.yaml");
    const DEPLOY_ROUTE: &str = "phala-cloud-sepolia-pha-usd";
    const DEPLOY_USDC_ROUTE: &str = "phala-cloud-sepolia-usdc-usd";
    const DEPLOY_BASE_PHA_ROUTE: &str = "phala-cloud-base-sepolia-pha-usd";
    const DEPLOY_BASE_USDC_ROUTE: &str = "phala-cloud-base-sepolia-usdc-usd";
    const DEPLOY_USDT_ROUTE: &str = "phala-cloud-sepolia-usdt-usd";
    const DEPLOY_BASE_USDT_ROUTE: &str = "phala-cloud-base-sepolia-usdt-usd";

    /// A route of Phala's staging configuration, by name.
    fn staging(name: &str) -> Result<RouteFile, String> {
        topup::config::Config::parse(STAGING)?
            .routes
            .into_iter()
            .find(|route| route.route == name)
            .ok_or_else(|| format!("staging has no route {name}"))
    }

    #[test]
    fn legacy_pricing_section_is_rejected() {
        let legacy = include_str!("../tests/fixtures/legacy-price-route.yaml");
        let error = parse_and_validate(legacy, false).expect_err("legacy pricing must be rejected");
        assert!(error.contains("unknown field `pricing`"), "{error}");
    }

    #[test]
    fn price_section_is_required() {
        let (before_price, price_and_merchant) =
            VALID.split_once("\nprice:").expect("price section");
        let (_, merchant) = price_and_merchant
            .split_once("\nmerchant:")
            .expect("merchant section");
        let yaml = format!("{before_price}\nmerchant:{merchant}");
        let error = parse_and_validate(&yaml, false).expect_err("price must be required");
        assert!(error.contains("missing field `price`"), "{error}");
    }

    #[test]
    fn valid_fixture_parses_and_validates() {
        parse_and_validate(VALID, false).expect("valid fixture must pass");
    }

    #[test]
    fn deployment_template_only_passes_in_template_mode() {
        for template in [TEMPLATE, USDT_TEMPLATE] {
            assert!(
                parse_and_validate(template, false)
                    .expect_err("zero placeholders must fail normal validation")
                    .contains("forwarder_factory")
            );
            parse_and_validate(template, true).expect("template placeholders must be allowed");
        }
    }

    #[test]
    fn mainnet_usdt_template_is_a_stablecoin_route_in_address_mode() {
        let usdt = parse_and_validate(USDT_TEMPLATE, true).expect("USDT template");
        assert!(usdt.livemode, "Ethereum is a mainnet");
        assert_eq!(
            (
                format!("{:#x}", usdt.asset.contract),
                usdt.asset.symbol.as_str(),
                usdt.asset.decimals
            ),
            (
                "0xdac17f958d2ee523a2206206994597c13d831ec7".to_owned(),
                "usdt",
                6
            )
        );

        assert_eq!(
            usdt.pricing.mode,
            topup_core::route::PricingMode::Stablecoin
        );
        assert!(
            usdt.pricing
                .sources
                .iter()
                .all(|s| s.asset() == usdt.asset.symbol)
        );
        assert_eq!(usdt.merchant.quote_spread_bps.default.value(), 0);
    }

    #[test]
    fn staging_route_resolves_to_the_reviewed_values() {
        let route = staging(DEPLOY_ROUTE).expect("staging route must pass");
        assert!(!route.livemode, "Sepolia is a test route");
        assert_eq!(
            route.chain.confirmations,
            topup_core::route::Confirmations::Depth(2)
        );

        assert_eq!(
            format!("{:#x}", route.chain.contracts.implementation),
            "0x49f2f1f1a25269ea0c6ff2ab1c7b09dcbe9c5ba9"
        );
        assert_eq!(route.chain.rpc_providers, ["provider-a", "provider-b"]);
        assert_eq!(route.pricing.primary[0].company(), "uniswap-v2-onchain");
        assert!(matches!(
            route.pricing.primary[0],
            topup_core::price::Source::UniswapV2Twap { .. }
        ));
        assert_eq!(route.pricing.check[0].company(), "kraken");
        assert!(
            route.pricing.validate_licensing(false).is_err(),
            "the staging opt-in never grants Kraken commercial permission"
        );
        assert_eq!(route.pricing.fx[0].asset(), "usdt");
        assert!(route.merchant.min_deposit_atomic.default.value().is_zero());
        assert_eq!(route.merchant.quote_ttl_seconds.default, 900);
        assert_eq!(route.merchant.quote_spread_bps.default.value(), 50);
        assert_eq!(route.alerts.stuck_after_s.confirmed, 1_800);
    }

    #[test]
    fn staging_usdc_route_is_a_stablecoin_route_beside_pha() {
        let pha = staging(DEPLOY_ROUTE).expect("staging route must pass");
        let usdc = staging(DEPLOY_USDC_ROUTE).expect("USDC route must pass");
        assert!(!usdc.livemode, "Sepolia is a test route");
        assert_eq!(
            usdc.chain, pha.chain,
            "one chain has one set of chain settings"
        );
        assert_eq!(
            (usdc.asset.symbol.as_str(), usdc.asset.decimals),
            ("usdc", 6)
        );

        assert_eq!(
            usdc.pricing.mode,
            topup_core::route::PricingMode::Stablecoin
        );
        assert!(
            usdc.pricing
                .sources
                .iter()
                .all(|s| s.asset() == usdc.asset.symbol)
        );
        assert!(usdc.pricing.check.is_empty());
        assert_eq!(
            usdc.merchant.quote_spread_bps.default.value(),
            0,
            "a fixed price needs no spread"
        );

        // The service loads both, and the USDC route puts the whole chain in address mode.
        let routes = topup::routes::RouteSet::new(vec![pha, usdc]).expect("both routes load");
        assert_eq!(routes.current_in(false).count(), 2);
        let chains = topup::scanner::chain_routes(&routes);
        assert_eq!(chains.len(), 1);
    }

    #[test]
    fn staging_usdt_routes_are_stablecoin_routes_beside_usdc() {
        for (usdt_route, usdc_route) in [
            (DEPLOY_USDT_ROUTE, DEPLOY_USDC_ROUTE),
            (DEPLOY_BASE_USDT_ROUTE, DEPLOY_BASE_USDC_ROUTE),
        ] {
            let usdt = staging(usdt_route).expect("USDT route must pass");
            let usdc = staging(usdc_route).expect("USDC route must pass");
            assert!(!usdt.livemode, "{usdt_route} is a test route");
            assert_eq!(
                usdt.chain, usdc.chain,
                "one chain has one set of chain settings"
            );
            assert_eq!(
                (usdt.asset.symbol.as_str(), usdt.asset.decimals),
                ("usdt", 6)
            );
            assert_eq!(
                usdt.pricing.mode,
                topup_core::route::PricingMode::Stablecoin
            );
            assert!(
                usdt.pricing
                    .sources
                    .iter()
                    .all(|s| s.asset() == usdt.asset.symbol)
            );
            assert!(usdt.pricing.check.is_empty());
            assert_eq!(usdt.merchant.quote_spread_bps.default.value(), 0);
            assert_eq!(
                (
                    usdt.merchant.max_deposit_atomic.default,
                    usdt.merchant.min_refund_atomic.default
                ),
                (
                    usdc.merchant.max_deposit_atomic.default,
                    usdc.merchant.min_refund_atomic.default
                ),
                "a dollar stablecoin has the USDC route's limits"
            );
        }
    }

    #[test]
    fn base_sepolia_routes_credit_at_the_op_stack_depth_on_their_own_providers() {
        let pha = staging(DEPLOY_BASE_PHA_ROUTE).expect("Base PHA route");
        let usdc = staging(DEPLOY_BASE_USDC_ROUTE).expect("Base USDC route");
        assert!(!pha.livemode, "Base Sepolia is a test route");
        assert_eq!(
            pha.chain, usdc.chain,
            "one chain has one set of chain settings"
        );
        // An OP-stack chain credits at the family's depth on the unsafe head (design D1).
        assert_eq!(
            pha.chain.confirmations,
            topup_core::route::ChainFamily::OpStack.default_confirmations()
        );
        assert_eq!(pha.chain.rpc_providers, ["provider-a", "provider-b"]);
        let sepolia = staging(DEPLOY_ROUTE).expect("staging route must pass");
        assert_eq!(pha.chain.contracts, sepolia.chain.contracts);
        assert_eq!(
            usdc.pricing.mode,
            topup_core::route::PricingMode::Stablecoin
        );
        assert_eq!(usdc.merchant.quote_spread_bps.default.value(), 0);

        // All six staging routes load together: two chains, each in address mode.
        let usdc_sepolia = staging(DEPLOY_USDC_ROUTE).expect("USDC route");
        let usdt_sepolia = staging(DEPLOY_USDT_ROUTE).expect("USDT route");
        let usdt = staging(DEPLOY_BASE_USDT_ROUTE).expect("Base USDT route");
        let routes = topup::routes::RouteSet::new(vec![
            sepolia,
            usdc_sepolia,
            usdt_sepolia,
            pha,
            usdc,
            usdt,
        ])
        .expect("the staging routes load");
        assert_eq!(routes.current_in(false).count(), 6);
        let chains = topup::scanner::chain_routes(&routes);
        assert_eq!(
            chains
                .iter()
                .map(|chain| chain.chain.chain_id)
                .collect::<Vec<_>>(),
            [84_532, 11_155_111]
        );
    }

    #[test]
    fn resolved_json_parses_back_to_the_same_route() {
        let staging_routes = topup::config::Config::parse(STAGING)
            .expect("the staging configuration is valid")
            .routes;
        let valid = parse_and_validate(VALID, false).expect("the fixture is valid");
        for route in std::iter::once(valid).chain(staging_routes) {
            let resolved = resolved_json(&route).expect("route serializes");
            assert!(
                resolved.contains("\"implementation\"")
                    && resolved.contains("\"quote_ttl_seconds\"")
            );
            assert_eq!(parse_and_validate(&resolved, false), Ok(route));
        }
    }

    #[test]
    fn chain_defaults_are_required_where_the_chain_has_none() {
        let without_chain_overrides = VALID.replace(
            "  sanctions_oracle: \"0x40C57923924B5c5c5455c48D93317139ADDaC8fb\"\n",
            "",
        );
        let mainnet =
            parse_and_validate(&without_chain_overrides, false).expect("chain 1 defaults");
        assert_eq!(
            mainnet.screening.sanctions_oracle,
            topup_core::route::default_sanctions_oracle(1).expect("mainnet oracle")
        );
        let sepolia = without_chain_overrides.replace("  chain_id: 1\n", "  chain_id: 11155111\n");
        assert!(
            parse_and_validate(&sepolia, false)
                .expect_err("no Chainalysis oracle on Sepolia")
                .contains("chain.sanctions_oracle")
        );
        let not_usdt = VALID.replace("symbol: PHAUSDT", "symbol: PHABTC");
        assert!(
            parse_and_validate(&not_usdt, false)
                .expect_err("a non-USDT market needs its FX leg")
                .contains("price")
        );
    }

    #[test]
    fn route_files_with_removed_keys_fail_with_the_key_name() {
        for (yaml, key) in [
            (
                VALID.replace("  chain_id: 1\n", "  chain_id: 1\n  finality: finalized\n"),
                "finality",
            ),
            // Routes belong to no product: any account quotes on the routes of its mode.
            (
                VALID.replace("livemode: true\n", "livemode: true\nproduct: phala-cloud\n"),
                "product",
            ),
            (
                VALID.replace(
                    "  min_amount: { default: 100 }\n",
                    "  min_amount: { default: 100 }\n  enabled: true\n",
                ),
                "enabled",
            ),
            // Each account's terms moved to its payment settings, within `merchant` bounds.
            (
                format!("{VALID}limits:\n  min_credit_minor: 100\n"),
                "limits",
            ),
            (format!("{VALID}quote:\n  window_s: 900\n"), "quote"),
            (
                VALID.replace(
                    "  min_amount: { default: 100 }\n",
                    "  min_amount: { default: 100, value: 1 }\n",
                ),
                "value",
            ),
            // The service sends no transactions: no operator key, flush schedule, or gas policy.
            (
                VALID.replace(
                    "  chain_id: 1\n",
                    "  chain_id: 1\n  operator_key_version: 1\n",
                ),
                "operator_key_version",
            ),
            (
                VALID.replace(
                    "  chain_id: 1\n",
                    "  chain_id: 1\n  flush:\n    schedule: \"0 * * * *\"\n",
                ),
                "flush",
            ),
            (
                VALID.replace(
                    "  min_amount: { default: 100 }\n",
                    "  min_amount: { default: 100 }\n  min_flush_atomic: { default: \"1\" }\n",
                ),
                "min_flush_atomic",
            ),
            (
                format!("{VALID}alerts:\n  stuck_after_s:\n    credited: 60\n"),
                "credited",
            ),
        ] {
            assert_ne!(yaml, VALID, "fixture edit for `{key}` must apply");
            let error =
                parse_and_validate(&yaml, false).expect_err("removed keys must be rejected");
            assert!(
                error.contains("unknown field") && error.contains(&format!("`{key}`")),
                "unclear error for `{key}`: {error}"
            );
        }
    }

    #[test]
    fn invalid_route_boundaries_fail_with_field_context() {
        for (yaml, field) in [
            (
                VALID.replace("decimals: 18", "decimals: 37"),
                "asset.decimals",
            ),
            (
                VALID.replace(
                    "merchant:\n",
                    "merchant:\n  quote_ttl_seconds: { default: 10 }\n",
                ),
                "merchant.quote_ttl_seconds",
            ),
            (
                VALID.replace(
                    "merchant:\n",
                    "merchant:\n  quote_ttl_seconds: { default: 900, max: 172800 }\n",
                ),
                "merchant.quote_ttl_seconds",
            ),
            (
                VALID.replace(
                    "merchant:\n",
                    "merchant:\n  quote_spread_bps: { default: 50, max: 6000 }\n",
                ),
                "merchant.quote_spread_bps",
            ),
            // A tolerance near 100% would match a payment of nothing.
            (
                VALID.replace(
                    "merchant:\n",
                    "merchant:\n  quote_tolerance_bps: { default: 100, max: 2000 }\n",
                ),
                "merchant.quote_tolerance_bps",
            ),
            (
                VALID.replace(
                    "merchant:\n",
                    "merchant:\n  quote_spread_bps: { default: 600, max: 500 }\n",
                ),
                "merchant.quote_spread_bps",
            ),
            (
                VALID.replace(
                    "  min_amount: { default: 100 }\n",
                    "  min_amount: { default: 100, min: 0 }\n",
                ),
                "merchant.min_amount",
            ),
            (
                VALID.replace(
                    "  min_refund_atomic: { default: \"20\" }\n",
                    "  min_refund_atomic: { default: \"20\", max: \"300000000000000000000000\" }\n",
                ),
                "merchant.min_refund_atomic",
            ),
            (
                VALID.replace("  min_amount: { default: 100 }\n", ""),
                "min_amount",
            ),
            (
                VALID.replace("symbol: pha\n", "symbol: PHA\n"),
                "asset.symbol",
            ),
            // Chain 1 is a mainnet, so its route is live.
            (
                VALID.replace("livemode: true\n", "livemode: false\n"),
                "livemode",
            ),
            (VALID.replace("livemode: true\n", ""), "livemode"),
            (
                VALID.replace(
                    "  chain_id: 1\n",
                    "  chain_id: 1\n  implementation: \"0x0000000000000000000000000000000000000000\"\n",
                ),
                "chain.implementation",
            ),
        ] {
            assert!(
                parse_and_validate(&yaml, false)
                    .expect_err("invalid route must fail")
                    .contains(field),
                "expected error for {field}"
            );
        }
    }

    #[test]
    fn volatile_requires_two_roles_and_stablecoin_forbids_them() {
        let mut route = parse_and_validate(VALID, false).expect("valid fixture");
        assert_eq!(route.pricing.mode, topup_core::route::PricingMode::Spot);
        route.pricing.check.clear();
        assert!(route.validate().is_err());
        route.pricing.mode = topup_core::route::PricingMode::Stablecoin;
        assert!(route.validate().is_err());
    }
}
