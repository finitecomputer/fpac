# The spike's tiny image (docs/spike.md): what shows a disk works for a
# computer's tools (git, sqlite3, node), e2fsprogs for the face, and
# fpac-probe, the guest's half of the driver. The driver stages this file
# beside a freshly built static fpac-probe and builds from there.
FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
RUN apt-get update \
 && apt-get install -y --no-install-recommends e2fsprogs git sqlite3 nodejs \
 && rm -rf /var/lib/apt/lists/*
COPY fpac-probe /usr/local/bin/fpac-probe
ENTRYPOINT ["/usr/local/bin/fpac-probe", "idle"]
