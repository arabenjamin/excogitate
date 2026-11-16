# The runtime, and a test fixture that shares its build. Nothing is downloaded
# when the image runs: the DuckDB library and extension are in it.
FROM rust:1.99.1-trixie AS build
WORKDIR /src
COPY . .
# Optional features, such as app/neo4j for the Neo4j store.
ARG FEATURES=""
RUN cargo build --locked --release -p app -p chaos-model --bin excogitate --bin fixture \
    --features "$FEATURES"
RUN mkdir /out \
    && cp target/release/excogitate target/release/fixture /out/ \
    && cp "$(find target/release/build -name libduckdb.so -print -quit)" /out/ \
    && scripts/fetch_duckdb_extensions.sh /out/extensions

FROM debian:trixie-slim AS excogitate
# upgrade: a base image cached here can be weeks behind Debian's security fixes.
RUN apt-get update \
    && apt-get upgrade --yes \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home excogitate \
    && install -d -o excogitate -g excogitate /data
COPY --from=build /out/excogitate /usr/local/bin/excogitate
COPY --from=build /out/libduckdb.so /usr/local/lib/libduckdb.so
COPY --from=build /out/extensions /opt/duckdb-extensions
RUN ldconfig
ENV EXCOGITATE_DATA_DIR=/data \
    EXCOGITATE_EXTENSION_DIR=/opt/duckdb-extensions \
    EXCOGITATE_MODELS=/config/models.yaml \
    EXCOGITATE_CONFIG=/config/config.yaml \
    EXCOGITATE_DEFINITIONS=/config/definitions
USER excogitate
VOLUME /data
# Asks /health at EXCOGITATE_BIND; with no bind set (stdio) it passes.
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
    CMD ["excogitate", "health"]
ENTRYPOINT ["excogitate"]
CMD ["serve"]

# A scripted model and a web page, for scripts/done_test.sh only.
FROM debian:trixie-slim AS fixture
COPY --from=build /out/fixture /usr/local/bin/fixture
USER 1000
ENTRYPOINT ["fixture"]
