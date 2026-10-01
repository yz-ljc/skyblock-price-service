FROM rust:1.97-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --create-home price
WORKDIR /app
COPY --from=builder /src/target/release/skyblock-price-service /app/skyblock-price-service
COPY config.example.toml /app/config.toml
COPY THIRD_PARTY_NOTICES.txt /app/THIRD_PARTY_NOTICES.txt
RUN mkdir /app/data && chown price:price /app/data
USER price
EXPOSE 25577
VOLUME ["/app/data"]
ENTRYPOINT ["/app/skyblock-price-service"]
