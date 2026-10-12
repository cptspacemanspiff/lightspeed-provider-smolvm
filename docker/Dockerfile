# lightspeed-provider-smolvm, its envd and smolvm, copied from the host
# (docker/stage puts them in dist/). The provider is built on this
# machine and linked against its glibc, so the base image matches the host OS.
FROM ubuntu:26.04
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl jq \
 && rm -rf /var/lib/apt/lists/*
COPY dist/smolvm/ /opt/smolvm/
COPY dist/envd/ /opt/lightspeed/envd/
COPY dist/bin/ /opt/lightspeed/bin/
COPY entrypoint.sh /opt/lightspeed/bin/
# HOME is the data mount: smolvm's machine database, image cache and VM disks,
# and the provider's default stateDir, all follow it.
ENV PATH=/opt/lightspeed/bin:/opt/smolvm:$PATH \
    HOME=/data
EXPOSE 19190
ENTRYPOINT ["entrypoint.sh"]
