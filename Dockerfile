# syntax=docker/dockerfile:1
# torrential's engine for a server or NAS: the static Linux binary a release builds, on a small
# Debian. Debian brings the glibc that the ffmpeg torrential fetches needs. CA certificates come
# from a stage that runs on the build machine, so an image for another architecture builds without
# emulating it.
#
# The release job puts each architecture's binary in image/<amd64|arm64>/torrential. By hand, on
# Linux:
#   scripts/release.sh
#   mkdir -p image/amd64 && tar -xzf dist/torrential-x86_64-unknown-linux-musl.tar.gz -C image/amd64
#   docker build -t torrential .
#
# Run it on the host's network: the pairing link names the host's addresses, and finding TVs
# (SSDP) multicasts on the home network. Everything it keeps is in /data:
#   mkdir -p ~/torrential
#   docker run -d --name torrential --network host --user "$(id -u):$(id -g)" \
#     -v ~/torrential:/data ghcr.io/nexxeln/torrential
#   docker exec torrential torrential devices add

FROM --platform=$BUILDPLATFORM debian:13-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && mkdir -p /out/data

FROM debian:13-slim
ARG TARGETARCH
LABEL org.opencontainers.image.source="https://github.com/nexxeln/torrential" \
      org.opencontainers.image.description="Search for a film or show, press Play, and watch it while it downloads." \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build --chown=1000:1000 /out/data /data
COPY --chmod=755 image/${TARGETARCH}/torrential /usr/local/bin/torrential
# Config in /data/config, downloads in /data/Downloads/torrential.
ENV HOME=/data TORRENTIAL_HOME=/data TORRENTIAL_IMAGE=ghcr.io/nexxeln/torrential
USER 1000:1000
WORKDIR /data
VOLUME /data
EXPOSE 47919
ENTRYPOINT ["torrential"]
CMD ["engine", "run", "--server"]
