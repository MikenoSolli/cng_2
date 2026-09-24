FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
COPY static ./static
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/cng-dashboard /usr/local/bin/cng-dashboard

# MariaDB runs outside this container, pass DATABASE_URL in at run time.
# TCP_ADDR must be 0.0.0.0 here: the container cannot bind the host's public IP.
ENV PORT=8080 TCP_PORT=5000 TCP_ADDR=0.0.0.0
EXPOSE 8080 5000
CMD ["cng-dashboard"]
