FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/rust-api /app
EXPOSE 8080
ENTRYPOINT ["/app"]
