# Сборка сервера Clipwire. Образ собирается из корня репозитория:
#   docker build -t clipwire-server .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p clipwire-server \
 && cp target/release/clipwire-server /clipwire-server  && mkdir /data

# В рантайме только glibc и сертификаты, без шелла и пакетного менеджера.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /clipwire-server /usr/local/bin/clipwire-server
# Каталог данных принадлежит пользователю nonroot (65532). Именованный том
# Docker при создании унаследует эти права.
COPY --from=build --chown=65532:65532 /data /data
ENV CLIPWIRE_DATA_DIR=/data \
    CLIPWIRE_RELAY_BIND=0.0.0.0:3340 \
    CLIPWIRE_QUIC_BIND=0.0.0.0:7843
VOLUME /data
EXPOSE 3340/tcp 7843/udp
ENTRYPOINT ["/usr/local/bin/clipwire-server"]
CMD ["run"]
