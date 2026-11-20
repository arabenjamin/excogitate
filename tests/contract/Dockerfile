# Schemathesis, pinned with every dependency by hash, to check a running
# server against the OpenAPI document it serves. See run.py.
FROM python:3.12.15-slim-trixie
COPY requirements.txt /contract/requirements.txt
RUN pip install --no-cache-dir --root-user-action=ignore --require-hashes --no-deps \
    -r /contract/requirements.txt
COPY schemathesis.toml run.py /contract/
WORKDIR /contract
USER nobody
ENTRYPOINT ["python", "-I", "/contract/run.py"]
