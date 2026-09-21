FROM rust:1.89-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p ctx-cli

FROM debian:bookworm-slim
COPY --from=build /src/target/release/ctx /usr/local/bin/ctx
VOLUME ["/data"]
EXPOSE 8788
ENTRYPOINT ["ctx", "relay", "serve", "--data", "/data", "--addr", "0.0.0.0:8788"]
