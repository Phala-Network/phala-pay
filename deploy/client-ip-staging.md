# Staging verification: client IP passthrough

This is an operator recipe for v0.10.9, after v0.10.8 (dstack 0.6.1, ingress 2.8, PROXY off)
has been deployed and verified on staging and production. It is not a record of a completed
staging exercise. Use the normal [Deploy upgrade](README.md#deploy) procedure for the release;
changing a live CVM requires the owner's separate deployment approval.

## Verify the attested path

1. Keep the Deploy run's attestation and rendered service compose. Its normal verification must
   pass with `port_policy` exactly
   `{"ports":[{"port":443,"pp":true}],"restrict_mode":false}` and `public_logs:false`.
   The same app-compose must set ingress `ACCEPT_PROXY_PROTOCOL: "true"`,
   `SEND_PROXY_PROTOCOL: "true"`, and `TARGET_ENDPOINT: topup:8082`, and start topup with
   `--proxy-protocol-bind 0.0.0.0:8082`. No topup port is published. Product and template attest
   `pp:false`. Restore-check inherits the service's exact policy through `instances add`;
   `pp:true` on 443 is inert without an ingress or listener there (only plain HTTP on 8081).
2. Require the Deploy upgrade-monitor result through the public domain to cover the upgrade
   through recovery. `/healthz` must return 200 with an empty body after readiness, and the
   normal Deploy attestation and certificate-evidence checks must pass. Neither the monitor nor
   the HTTPS client sends a PROXY header: the gateway adds it before ingress.

   ```sh
   export DOMAIN=pay-api-staging.phala.com
   export ORIGIN="https://$DOMAIN"
   # APP_ID is the staging app ID verified by this Deploy run.
   curl --fail --silent --show-error --max-time 10 "$ORIGIN/healthz"
   kit/deploy/verify-ingress-evidence.sh "$DOMAIN" "$APP_ID"
   ```

3. Leave the internal `topup healthcheck` and direct topup HTTP probes on 8080. Do not send
   ordinary HTTP or TLS to 8082, or probe ingress's public bind directly without the gateway:
   those listeners require a PROXY header. Never trust `X-Forwarded-For` or `X-Real-IP` as proof
   of the client address.

## Verify two sources without topup IP logging

The least invasive proposal combines private HAProxy access logs with the listener's regression
tests. It adds no endpoint, build flag, packet capture, per-IP metric, or production instrumentation.

**Current gap:** the kit has no documented private log retrieval surface. With `public_logs:false`,
[dstack 0.6.1 omits the external container-log route](https://github.com/Dstack-TEE/dstack/blob/v0.6.1/dstack/guest-agent/src/http_routes.rs#L20-L28),
and this repository's non-dev CVMs expose no logs. The operator must supply an already authorized
private evidence path before the log-comparison step can pass. Without one, report this step as
blocked; do not change `public_logs` or invent an owner-authenticated log endpoint.

1. From the release checkout, run the existing test-only listener checks:

   ```sh
   cargo test --locked -p topup --lib api::proxy_protocol::tests
   ```

   They assert that different IPv4/IPv6 sources in PROXY v2 become Axum `ConnectInfo<SocketAddr>`,
   HTTP forwarding headers cannot override them, and rejected headers do not log client data.
   The test-only handler is not part of the deployed router.
2. Arrange two operator-controlled clients with different public egress addresses, such as two
   networks, or IPv4 and IPv6 where both paths are available. From each, make a short, separate
   burst of `/healthz` requests through `https://$DOMAIN`, closing each connection. Record each
   client's time window in US Pacific time; add UTC when comparing the ingress log timestamps.
   Require HTTP 200 from both clients.
3. The authorized staging operator privately compares those windows with HAProxy's existing TCP
   access logs and confirms two distinct source addresses, matching the clients' known egress
   addresses rather than the gateway or ingress address. Keep `public_logs:false`; never attach
   raw access-log lines or addresses to PRs, workflow artifacts, Sentry, or the verification
   record. Record only the two-source pass/fail, time windows, release, verified compose hash,
   and Deploy run URL. If owner-authenticated private logs are unavailable, this step is blocked:
   report the missing evidence rather than enabling public logs or adding an IP endpoint.

The live evidence is compositional: private ingress logs establish the sources ingress accepts;
its attested SEND setting forwards those sources as PROXY v2; successful health requests on the
attested 8082 path establish header acceptance; the listener tests establish that parsing supplies
those addresses to the router. Topup deliberately has no live API or log exposing `ConnectInfo`,
so this recipe does not directly inspect the live handler's connection information. If direct
in-process observation is required, report that spec gap before adding instrumentation.

## Check evidence and tls-alpn-01 renewal

The pinned ingress's public peek frontend consumes the gateway header before inspecting ALPN.
It carries the address to the normal loopback TLS frontend, sends PROXY v2 only to the application
backend, and forwards ACME challenge TLS and evidence HTTP without PROXY headers. See upstream
[Client address](https://github.com/Dstack-TEE/dstack-examples/blob/dstack-ingress-v2.8/custom-domain/dstack-ingress/README.md#client-address-proxy-protocol).

Initial `/evidences/` verification does not prove renewal. At the next normal staging renewal,
require successful tls-alpn-01 issuance in the private ingress log, a renewed served certificate,
continued public health, and a fresh passing `verify-ingress-evidence.sh`. Keep the existing
renewal settings and certificate volume; do not force a renewal, change DNS, or delete the volume
as part of this recipe. Until observed, record renewal as unverified.

## Rollback

See [rollback compatibility](README.md#rollback-compatibility) for the v0.13.0 floor.
