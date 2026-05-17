#!/usr/bin/env bash
# Build the agent-vm OCI image and push it to a host-local registry.
#
# microsandbox pulls images from registries by reference, so we run a tiny
# registry:2 container bound to 127.0.0.1:5000 and treat it as our local
# image store. The registry is shared across `agent-vm setup` runs; we
# create it on demand and never tear it down.

set -euo pipefail

REGISTRY_NAME="${AGENT_VM_REGISTRY_NAME:-agent-vm-registry}"
REGISTRY_PORT="${AGENT_VM_REGISTRY_PORT:-5000}"
IMAGE_TAG="${AGENT_VM_IMAGE_TAG:-localhost:${REGISTRY_PORT}/agent-vm:latest}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ensure_registry() {
    local state
    state=$(docker inspect --type container -f '{{.State.Status}}' "${REGISTRY_NAME}" 2>/dev/null || echo missing)
    case "${state}" in
        running)
            ;;
        missing)
            echo "==> Creating local registry ${REGISTRY_NAME} on 127.0.0.1:${REGISTRY_PORT}"
            docker run -d \
                --name "${REGISTRY_NAME}" \
                --restart=always \
                -p "127.0.0.1:${REGISTRY_PORT}:5000" \
                registry:2 >/dev/null
            ;;
        *)
            # exited, created, paused, restarting, dead — all need a kick.
            echo "==> Starting existing registry container ${REGISTRY_NAME} (was: ${state})"
            docker start "${REGISTRY_NAME}" >/dev/null
            ;;
    esac
    wait_for_registry
}

# `docker start` returns when the container has launched, but registry:2's
# HTTP listener takes another ~100ms to bind. A push fired immediately after
# loses that race with ECONNREFUSED, so poll until the v2 API answers.
wait_for_registry() {
    local i
    for i in $(seq 1 50); do
        if curl -fsS "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    echo "Registry did not become reachable on 127.0.0.1:${REGISTRY_PORT} after 10s" >&2
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
