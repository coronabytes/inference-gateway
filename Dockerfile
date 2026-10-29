# syntax=docker/dockerfile:1
# Base images are pinned to digests; the tag before each names the version, and Dependabot
# proposes the bumps.

# --- admin UI -----------------------------------------------------------------
FROM node:24-alpine@sha256:8a32c8cf649a49915a66e81e7a609223c708d0adabf288ee3f4077d765542f85 AS ui
WORKDIR /ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY ui/ ./
RUN npm run build

# --- gateway (static musl binary) ---------------------------------------------
FROM rust:1.99-alpine@sha256:b1d5dd5a22a09404d3e798c0cf051b689077360374eb505fa5bb93c479693d87 AS build
RUN apk add --no-cache musl-dev cmake make perl clang
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# Build dependencies first so source edits don't rebuild the whole tree.
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/deps/inference_gateway*
COPY migrations migrations
# Prepared query metadata: sqlx checks queries against it instead of a live database.
COPY .sqlx .sqlx
COPY build.rs build.rs
COPY src src
ENV SQLX_OFFLINE=true
# The git tag the image is built from (the release workflow passes it); .git is not in the
# build context, so build.rs cannot ask git. Declared here so it does not invalidate the
# dependency layer above.
ARG GATEWAY_VERSION=dev
RUN cargo build --release --locked && cp target/release/inference-gateway /gateway

# --- runtime: distroless static, non-root, no shell ----------------------------
FROM gcr.io/distroless/static-debian13:nonroot@sha256:18f4cc26c9ffa7f20a359298be7deadac5c607c8e03a02b8119f1a5fc2f82c99
COPY --from=build /gateway /gateway
COPY --from=ui /ui/dist /ui
COPY LICENSE /LICENSE
ENV GATEWAY_LISTEN=0.0.0.0:8080 \
    GATEWAY_LOG_FORMAT=json
USER 65532:65532
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s CMD ["/gateway", "healthcheck"]
ENTRYPOINT ["/gateway"]
