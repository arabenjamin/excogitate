"""Fetches the document a server serves, with its key, then has Schemathesis
call every operation in it and check each reply against it.

CONTRACT_URL is the server, CONTRACT_KEY its owner key. The requests are the
same every run unless CONTRACT_RANDOM is 1; then they're new, and the seed
printed at the end, given back as CONTRACT_SEED, repeats them.
CONTRACT_EXAMPLES is how many to try for each operation.
"""

import os
import sys
import time
import urllib.request

url = os.environ["CONTRACT_URL"].rstrip("/")
key = os.environ["CONTRACT_KEY"]
document = "/tmp/openapi.json"

for _ in range(60):
    try:
        request = urllib.request.Request(
            f"{url}/api/v1/openapi.json", headers={"Authorization": f"Bearer {key}"}
        )
        with urllib.request.urlopen(request, timeout=5) as reply, open(document, "wb") as out:
            out.write(reply.read())
        break
    except OSError:
        time.sleep(1)
else:
    sys.exit(f"contract: {url} never served its document")

args = [
    "schemathesis",
    "--config-file",
    "/contract/schemathesis.toml",
    "run",
    document,
    "--url",
    url,
    "--header",
    f"Authorization: Bearer {key}",
    "--checks",
    "all",
    "--max-examples",
    os.environ.get("CONTRACT_EXAMPLES", "25"),
    "--report",
    "junit",
    "--report-dir",
    "/tmp/report",
]
if os.environ.get("CONTRACT_RANDOM") == "1":
    # New requests each run, from no database; the seed it prints repeats
    # them, given back as CONTRACT_SEED.
    args += ["--generation-database", "none"]
    if os.environ.get("CONTRACT_SEED"):
        args += ["--seed", os.environ["CONTRACT_SEED"]]
else:
    # The same requests every run, so a failure is the change's, not luck's.
    args += ["--generation-deterministic"]
os.execvp(args[0], args)
