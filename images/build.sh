#!/usr/bin/env bash
# Build the agent-vm OCI image and push it to a host-local registry.
#
# microsandbox pulls images from registries by reference, so we run a tiny
# registry:2 container bound to 127.0.0.1:5000 and treat it as our local
# image store. The registry is shared across `agent-vm setup` runs; we
# create it on demand and recover by hand if a prior session left it in a
# bad state (running but no port published, crashed inside, etc.).

set -euo pipefail

REGISTRY_NAME="${AGENT_VM_REGISTRY_NAME:-agent-vm-registry}"
REGISTRY_PORT="${AGENT_VM_REGISTRY_PORT:-5000}"
IMAGE_TAG="${AGENT_VM_IMAGE_TAG:-localhost:${REGISTRY_PORT}/agent-vm:latest}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Returns 0 if /v2/ on the registry port answers within the timeout.
# Quiet — caller decides whether to log.
poll_registry() {
    local attempts="${1:-50}" i
    for ((i = 0; i < attempts; i++)); do
        curl -fsS "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1 && return 0
        sleep 0.2
    done
    return 1
}

dump_registry_diagnostics() {
    {
        echo "Container state:"
        docker ps -a --filter "name=${REGISTRY_NAME}" \
            --format '  status={{.Status}}  ports={{.Ports}}' 2>/dev/null || true
        echo "Port bindings:"
        docker inspect "${REGISTRY_NAME}" \
            --format '  {{json .NetworkSettings.Ports}}' 2>/dev/null || true
        echo "Last 30 lines of container logs:"
        docker logs --tail 30 "${REGISTRY_NAME}" 2>&1 | sed 's/^/  /' || true
    } >&2
}

create_registry() {
    echo "==> Creating local registry ${REGISTRY_NAME} on 127.0.0.1:${REGISTRY_PORT}"
    docker run -d \
        --name "${REGISTRY_NAME}" \
        --restart=always \
        -p "127.0.0.1:${REGISTRY_PORT}:5000" \
        registry:2 >/dev/null
}

recreate_registry() {
    echo "==> Removing stale ${REGISTRY_NAME} container"
    dump_registry_diagnostics
    docker rm -f "${REGISTRY_NAME}" >/dev/null 2>&1 || true
    create_registry
}

# Idempotent: bring the registry container into a "running and answering on
# 127.0.0.1:${REGISTRY_PORT}" state, no matter how it was left.
#
# Cases:
#   - missing            → create.
#   - running, healthy   → nothing.
#   - running, unhealthy → recreate (was probably started in a past session
#     without the right `-p` mapping, or the registry process crashed).
#   - stopped/etc        → start; recreate if still unhealthy after start.
ensure_registry() {
    local state
    state=$(docker inspect --type container -f '{{.State.Status}}' "${REGISTRY_NAME}" 2>/dev/null || echo missing)

    case "${state}" in
        running)
            if poll_registry 5; then
                return 0
            fi
            echo "==> ${REGISTRY_NAME} is running but 127.0.0.1:${REGISTRY_PORT} is unresponsive"
            recreate_registry
            ;;
        missing)
            create_registry
            ;;
        *)
            echo "==> Starting existing registry container ${REGISTRY_NAME} (was: ${state})"
            docker start "${REGISTRY_NAME}" >/dev/null
            # registry:2 takes ~100ms after start to bind 5000 — short poll
            # is normal. If the container was misconfigured (no port mapping)
            # this longer poll will time out, and we recreate from scratch.
            if poll_registry 25; then
                return 0
            fi
            echo "==> ${REGISTRY_NAME} did not become reachable after restart"
            recreate_registry
            ;;
    esac

    echo "==> Waiting for registry on 127.0.0.1:${REGISTRY_PORT} to accept connections"
    if poll_registry 50; then
        return 0
    fi
    {
        echo "Registry did not become reachable on 127.0.0.1:${REGISTRY_PORT} after 10s."
        echo "This is past our auto-recovery — Docker itself is probably misbehaving."
    } >&2
    dump_registry_diagnostics
    return 1
}

build_and_push() {
    echo "==> Building ${IMAGE_TAG}"
    docker build -t "${IMAGE_TAG}" -f "${SCRIPT_DIR}/Dockerfile" "${SCRIPT_DIR}"
    echo "==> Pushing ${IMAGE_TAG}"
    docker push "${IMAGE_TAG}"
}

main() {
    ensure_registry
    build_and_push
    echo "==> ${IMAGE_TAG} ready"
}

main "$@"
