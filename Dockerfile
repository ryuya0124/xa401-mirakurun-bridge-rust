FROM rust:1.99.0-bookworm AS build
WORKDIR /code
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY assets ./assets
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN cargo build --release --locked

FROM debian:trixie-slim
COPY --from=build /code/target/release/xa401-bridge /usr/local/bin/xa401-bridge
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/xa401-bridge"]
CMD ["serve", "--config", "/private/bridge.json"]
