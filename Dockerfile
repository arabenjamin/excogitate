# The brain, and a test fixture that shares its build. Nothing is downloaded
# when the image runs: the DuckDB library and extension are in it.
FROM rust:1.99.1-trixie AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p app -p chaos-model --bin excogitate --bin fixture
RUN mkdir /out \
    && cp target/release/excogitate target/release/fixture /out/ \
    && cp "$(find target/release/build -name libduckdb.so -print -quit)" /out/ \
    && scripts/fetch_duckdb_extensions.sh /out/extensions

FROM debian:trixie-slim AS brain
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home brain \
    && install -d -o brain -g brain /data
COPY --from=build /out/excogitate /usr/local/bin/excogitate
COPY --from=build /out/libduckdb.so /usr/local/lib/libduckdb.so
COPY --from=build /out/extensions /opt/duckdb-extensions
RUN ldconfig
ENV EXCOGITATE_DATA_DIR=/data \
    EXCOGITATE_EXTENSION_DIR=/opt/duckdb-extensions \
    EXCOGITATE_MODELS=/config/models.yaml \
    EXCOGITATE_CONFIG=/config/brain.yaml
USER brain
VOLUME /data
ENTRYPOINT ["excogitate"]
CMD ["serve"]

# A scripted model and a web page, for scripts/done_test.sh only.
FROM debian:trixie-slim AS fixture
COPY --from=build /out/fixture /usr/local/bin/fixture
USER 1000
ENTRYPOINT ["fixture"]
