FROM rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7 AS builder

ARG SOURCE_DATE_EPOCH=0
# The commit the image is built from, compiled in as the Sentry release (unset: none).
ARG SOURCE_COMMIT=""
# Bounds the compiler's parallelism on a shared host; the binary does not depend on it.
ARG BUILD_JOBS=""
ENV CARGO_INCREMENTAL=0 \
    SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}

# Embed the locked Rust dependency inventory for final-image scanning and SBOM generation.
RUN cargo install cargo-auditable --version 0.7.7 --locked

WORKDIR /workspace
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .sqlx ./.sqlx
COPY crates ./crates
RUN SQLX_OFFLINE=true SOURCE_COMMIT="$SOURCE_COMMIT" \
    RUSTFLAGS="--remap-path-prefix=/workspace=. -C link-arg=-Wl,--build-id=none" \
    cargo auditable build --release --locked -p topup ${BUILD_JOBS:+--jobs "$BUILD_JOBS"}

# Stripe's smokescreen, the webhook egress proxy (docs/design/multi-tenant.md §8), which the
# compose runs from this image as the `smokescreen` sidecar, so it is pinned and attested with
# the service's digest (deploy/README.md, "Webhook egress"). Stripe publishes no image: this builds
# tag v0.1.0's commit with pinned Logrus and x/net security updates and the local toolchain.
FROM golang:1.27.2-trixie@sha256:e58d6f83b3416618d8bcac2b3dde1b7f7e3c4a77d25e88637f8bbae81536c48d AS smokescreen

WORKDIR /src
RUN git init -q . \
    && git fetch -q --depth 1 https://github.com/stripe/smokescreen.git \
        609eb8931420453daf5893509be0b25b21bd9edb \
    && git checkout -q FETCH_HEAD \
    && GOTOOLCHAIN=local go get github.com/sirupsen/logrus@v1.9.3 golang.org/x/net@v0.60.0 \
    && GOTOOLCHAIN=local go mod vendor \
    && CGO_ENABLED=0 GOTOOLCHAIN=local GOFLAGS=-mod=vendor \
        go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o /out/smokescreen .

FROM gcr.io/distroless/cc-debian13:latest@sha256:159783207c2cd44c2aa5715961d13c8612368ac9bd450f887e3f08fc8ea461e3

COPY --from=builder --chown=nonroot:nonroot /workspace/target/release/topup /usr/local/bin/topup
COPY --from=smokescreen --chown=nonroot:nonroot /out/smokescreen /usr/local/bin/smokescreen
USER nonroot:nonroot
CMD ["topup", "--help"]
