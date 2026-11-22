#!/usr/bin/env bash
set -euo pipefail

: "${INGRESS_DOMAIN:?}"
: "${INGRESS_NETWORK:?}"
: "${CADDY_PROJECT:?}"
: "${CADDY_SERVICE:?}"

labels=(
    --filter "label=com.docker.compose.project=$CADDY_PROJECT"
    --filter "label=com.docker.compose.service=$CADDY_SERVICE"
)

reconcile() {
    local proxy active config connected
    proxy=$(docker ps --quiet "${labels[@]}")
    [[ -n "$proxy" ]] || return 0
    if [[ "$proxy" == *$'\n'* ]]; then
        echo "Expected a single Caddy container." >&2
        return 1
    fi

    connected=$(docker inspect --format "{{if index .NetworkSettings.Networks \"$INGRESS_NETWORK\"}}yes{{end}}" "$proxy")
    if [[ "$connected" != yes ]]; then
        docker network connect --gw-priority -1 "$INGRESS_NETWORK" "$proxy"
    fi

    for (( attempt=0; attempt<20; attempt++ )); do
        if active=$(docker exec "$proxy" wget -qO- http://127.0.0.1:2019/config/); then
            break
        fi
        sleep 1
    done
    [[ -n "$active" ]]
    [[ "$active" != *"\"$INGRESS_DOMAIN\""* ]] || return 0

    config=/etc/caddy/.maggotdrop.Caddyfile
    {
        docker exec "$proxy" cat /etc/caddy/Caddyfile
        printf '\n%s {\n    reverse_proxy maggotdrop:3000\n}\n' "$INGRESS_DOMAIN"
    } | docker exec -i "$proxy" sh -c 'cat > "$1"' sh "$config"
    docker exec "$proxy" caddy validate --config "$config" --adapter caddyfile
    docker exec "$proxy" caddy reload --config "$config" --adapter caddyfile
}

since=$(date -u +%FT%TZ)
reconcile
docker events --since "$since" --filter type=container --filter event=start "${labels[@]}" |
    while IFS= read -r _; do
        reconcile
    done
