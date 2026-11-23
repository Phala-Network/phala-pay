FROM rust:1.100.0-slim-trixie@sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe AS builder

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
FROM golang:1.27.4-trixie@sha256:f18c3d73a184fb0582828510722e723b63a93f7188d11dd223d9be6f6880e65d AS smokescreen

WORKDIR /src
RUN git init -q . \
    && git fetch -q --depth 1 https://github.com/stripe/smokescreen.git \
        609eb8931420453daf5893509be0b25b21bd9edb \
    && git checkout -q FETCH_HEAD \
    && GOTOOLCHAIN=local go get github.com/sirupsen/logrus@v1.9.3 golang.org/x/net@v0.62.0 \
    && GOTOOLCHAIN=local go mod vendor \
    && CGO_ENABLED=0 GOTOOLCHAIN=local GOFLAGS=-mod=vendor \
        go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o /out/smokescreen .

FROM gcr.io/distroless/cc-debian13:latest@sha256:d74d144baa124dd9defd01d71872bb56c49d4441dba6153ccf790364f97e64b1

COPY --from=builder --chown=nonroot:nonroot /workspace/target/release/topup /usr/local/bin/topup
COPY --from=smokescreen --chown=nonroot:nonroot /out/smokescreen /usr/local/bin/smokescreen
USER nonroot:nonroot
CMD ["topup", "--help"]
