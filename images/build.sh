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
    if docker inspect --type container "${REGISTRY_NAME}" >/dev/null 2>&1; then
        if [ "$(docker inspect -f '{{.State.Running}}' "${REGISTRY_NAME}")" != "true" ]; then
            echo "==> Starting existing registry container ${REGISTRY_NAME}"
            docker start "${REGISTRY_NAME}" >/dev/null
        fi
        return
    fi
    echo "==> Creating local registry ${REGISTRY_NAME} on 127.0.0.1:${REGISTRY_PORT}"
    docker run -d \
        --name "${REGISTRY_NAME}" \
        --restart=always \
        -p "127.0.0.1:${REGISTRY_PORT}:5000" \
        registry:2 >/dev/null
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
