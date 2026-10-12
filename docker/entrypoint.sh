#!/usr/bin/env bash
# Adds the image's envd to the mounted config (/etc/lightspeed/smolvm-provider.json,
# which has none), then runs the provider on the result.
set -euo pipefail

[ -r /dev/kvm ] && [ -w /dev/kvm ] || {
    echo "entrypoint: no read/write access to /dev/kvm (is KVM_GID its group id?)" >&2
    exit 1
}

config=/tmp/provider.json
jq --slurpfile envd /opt/lightspeed/envd/envd.json '.envd = $envd[0]' \
    /etc/lightspeed/smolvm-provider.json >"$config"
exec lightspeed-provider-smolvm --config "$config"
