FROM docker.io/flyway/flyway:10.7.1 AS migrations
COPY database/ /flyway/
CMD ["migrate"]

FROM docker.io/rust:1-slim-bookworm AS cargo-build
WORKDIR /src/

# Accept build arguments for enabling features
ARG CARGO_BUILD_FEATURES=""
ARG RUSTFLAGS=""

# Install dependencies
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked apt-get update && \
    apt-get install -y git libssl-dev pkg-config build-essential

# Copy and Build Code
COPY . .
# Build with the toolchain already present in the Rust image instead of the
# floating `stable` alias, which can trigger an in-build rustup update.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    TOOLCHAIN="$(rustup show active-toolchain | cut -d' ' -f1)" && \
    CARGO_PROFILE_RELEASE_DEBUG=1 RUSTFLAGS="${RUSTFLAGS}" cargo +"${TOOLCHAIN}" build --release --bin solvers ${CARGO_BUILD_FEATURES} && \
    cp target/release/solvers /

# Create an intermediate image to extract the binaries
FROM docker.io/debian:bookworm-slim AS intermediate
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked apt-get update && \
    apt-get install -y ca-certificates tini gettext-base && \
    apt-get clean

FROM intermediate AS solvers
COPY --from=cargo-build /solvers /usr/local/bin/solvers
ENTRYPOINT [ "solvers" ]
