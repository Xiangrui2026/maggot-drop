#!/usr/bin/env bash
set -euo pipefail

if [[ ! ${SSH_ORIGINAL_COMMAND:-} =~ ^deploy[[:blank:]]([a-f0-9]{40})$ ]]; then
    echo "Expected: deploy <commit SHA>" >&2
    exit 2
fi
revision=${BASH_REMATCH[1]}
export GIT_TERMINAL_PROMPT=0

exec 9>/run/lock/maggotdrop-deploy.lock
flock -w 180 9
cd /opt/maggot-drop
test -z "$(git status --porcelain)"
git fetch --no-tags origin refs/heads/main:refs/remotes/origin/main
if [[ $(git rev-parse refs/remotes/origin/main) != "$revision" ]]; then
    printf 'Skipping %s because main has moved.\n' "$revision"
    exit 0
fi

previous_revision=$(git rev-parse HEAD)
git merge-base --is-ancestor "$previous_revision" "$revision"
previous_container=$(docker compose ps --all --quiet server)
previous_image=$(docker inspect --format '{{.Image}}' "$previous_container")
staging=$(mktemp -d /var/tmp/maggotdrop-deploy.XXXXXX)
candidate="maggotdrop-server:deploy-$revision"
service_image=maggotdrop-server:latest
restore=0
compose_up=(docker compose up --no-build --pull never --detach --wait --wait-timeout 90 server)

cleanup() {
    local result=$?
    trap - EXIT
    if (( restore )); then
        printf 'Rolling back to %s.\n' "$previous_revision" >&2
        docker compose logs --tail 40 server >&2 || true
        if ! git switch --detach "$previous_revision" ||
            ! docker image tag "$previous_image" "$service_image" ||
            ! "${compose_up[@]}"; then
            echo "Rollback failed. Inspect the Compose project on the host." >&2
            result=1
        fi
    fi
    docker image rm "$candidate" >/dev/null 2>&1 || true
    rm -rf -- "$staging"
    exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

git archive "$revision" | tar -x -C "$staging"
docker build --progress plain --tag "$candidate" "$staging"
restore=1
git switch --detach "$revision"
docker compose config --quiet
docker image tag "$candidate" "$service_image"
"${compose_up[@]}"
restore=0
docker compose ps server
printf 'Deployed %s.\n' "$revision"
