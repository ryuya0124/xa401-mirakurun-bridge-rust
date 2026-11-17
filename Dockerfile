FROM rust:1.100.0-bookworm AS build
WORKDIR /code
COPY openssl-build.conf ./
COPY scripts/build-openssl.sh ./scripts/build-openssl.sh
RUN sh scripts/build-openssl.sh && . ./openssl-build.conf && ln -s "/code/target/openssl-$OPENSSL_VERSION/install" /opt/openssl
ENV OPENSSL_DIR=/opt/openssl OPENSSL_STATIC=1
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY assets ./assets
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN cargo test --release --locked && cargo build --release --locked

FROM debian:trixie-slim
COPY --from=build /code/target/release/xa401-bridge /usr/local/bin/xa401-bridge
COPY licenses /usr/share/doc/xa401-bridge/licenses
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/xa401-bridge"]
CMD ["serve", "--config", "/private/bridge.json"]
