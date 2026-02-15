#!/usr/bin/env bash
#
# GitHub App Scoped Token Generator (pure bash, no Python needed)
#
# Generates repo-scoped GitHub tokens for wirenboard/agent-vm using a GitHub App
# with read/write contents permission.
#
# Dependencies: openssl, curl, jq (optional, for pretty output)
#
# Usage:
#   # Print instructions to create the GitHub App
#   ./github_app_token.sh create-app
#
#   # Generate a scoped token
#   ./github_app_token.sh get-token --app-id 123456 --pem-file key.pem

set -euo pipefail

TARGET_OWNER="${TARGET_OWNER:-wirenboard}"
TARGET_REPO="${TARGET_REPO:-agent-vm}"
API="https://api.github.com"

# --- Helpers ---

die() { echo "Error: $*" >&2; exit 1; }

base64url() {
    openssl base64 -e -A | tr '+/' '-_' | tr -d '='
}

# --- App creation helper ---

cmd_create_app() {
    cat <<'EOF'
============================================================
GitHub App Creation Instructions
============================================================

1. Go to: https://github.com/organizations/wirenboard/settings/apps/new
   (Or for personal account: https://github.com/settings/apps/new)

2. Fill in the following settings:
   - GitHub App name: WB Agent VM Token Generator
   - Homepage URL: https://github.com/wirenboard/agent-vm
   - Uncheck 'Active' under Webhook
   - Under 'Repository permissions':
     - Contents: Read & write
   - Under 'Where can this GitHub App be installed?':
     - Select 'Only on this account'

3. Click 'Create GitHub App'

4. Note the App ID shown on the next page

5. Scroll down to 'Private keys' and click 'Generate a private key'
   Save the downloaded .pem file

6. Install the app:
   - Click 'Install App' in the left sidebar
   - Choose the wirenboard organization
   - Select 'Only select repositories' and pick 'agent-vm'

7. Generate a token:
EOF
    echo "   $0 get-token --app-id <APP_ID> --pem-file <path/to/key.pem>"
    echo
}

# --- JWT generation ---

generate_jwt() {
    local app_id="$1"
    local pem_file="$2"

    local now
    now=$(date +%s)
    local iat=$((now - 60))
    local exp=$((now + 600))

    local header
    header=$(printf '{"alg":"RS256","typ":"JWT"}' | base64url)

    local payload
    payload=$(printf '{"iat":%d,"exp":%d,"iss":"%s"}' "$iat" "$exp" "$app_id" | base64url)

    local signature
    signature=$(printf '%s.%s' "$header" "$payload" \
        | openssl dgst -sha256 -sign "$pem_file" -binary \
        | base64url)

    printf '%s.%s.%s' "$header" "$payload" "$signature"
}

# --- GitHub API ---

github_api() {
    local method="$1"
    local path="$2"
    local token="$3"
    local token_type="${4:-Bearer}"
    local body="${5:-}"

    local curl_args=(
        -s -f
        -X "$method"
        -H "Accept: application/vnd.github+json"
        -H "Authorization: ${token_type} ${token}"
        -H "X-GitHub-Api-Version: 2022-11-28"
    )

    if [[ -n "$body" ]]; then
        curl_args+=(-H "Content-Type: application/json" -d "$body")
    fi

    local response http_code
    response=$(curl -w '\n%{http_code}' "${curl_args[@]}" "${API}${path}" 2>&1) || {
        echo "API request failed: ${method} ${API}${path}" >&2
        echo "$response" >&2
        exit 1
    }

    # Extract body (all lines except last) and http_code (last line)
    http_code=$(echo "$response" | tail -1)
    echo "$response" | sed '$d'
}

# --- Token generation ---

cmd_get_token() {
    local app_id=""
    local pem_file=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --app-id) app_id="$2"; shift 2 ;;
            --pem-file) pem_file="$2"; shift 2 ;;
            *) die "Unknown option: $1" ;;
        esac
    done

    [[ -n "$app_id" ]] || die "Missing --app-id"
    [[ -n "$pem_file" ]] || die "Missing --pem-file"
    [[ -f "$pem_file" ]] || die "PEM file not found: $pem_file"

    echo "Generating JWT for App ID ${app_id}..."
    local jwt
    jwt=$(generate_jwt "$app_id" "$pem_file")

    echo "Looking up installation..."
    local installations
    installations=$(github_api GET /app/installations "$jwt")

    # Find installation for target owner
    local installation_id=""
    if command -v jq &>/dev/null; then
        installation_id=$(echo "$installations" \
            | jq -r --arg owner "$TARGET_OWNER" \
                '.[] | select(.account.login == $owner) | .id // empty' \
            | head -1)
    else
        # Fallback: grep for the installation id near the target owner
        # This is fragile but works for simple cases
        installation_id=$(echo "$installations" \
            | grep -B5 "\"login\":.*\"${TARGET_OWNER}\"" \
            | grep '"id":' | head -1 \
            | sed 's/.*"id": *\([0-9]*\).*/\1/')
    fi

    [[ -n "$installation_id" ]] || die "No installation found for ${TARGET_OWNER}/${TARGET_REPO}. Make sure the app is installed."
    echo "Found installation ID: ${installation_id}"

    echo "Requesting scoped token for ${TARGET_OWNER}/${TARGET_REPO} (contents: write)..."
    local token_body
    token_body=$(printf '{"repositories":["%s"],"permissions":{"contents":"write"}}' "$TARGET_REPO")

    local token_resp
    token_resp=$(github_api POST "/app/installations/${installation_id}/access_tokens" "$jwt" "Bearer" "$token_body")

    local token expires_at
    if command -v jq &>/dev/null; then
        token=$(echo "$token_resp" | jq -r '.token')
        expires_at=$(echo "$token_resp" | jq -r '.expires_at')
    else
        token=$(echo "$token_resp" | grep '"token":' | head -1 | sed 's/.*"token": *"\([^"]*\)".*/\1/')
        expires_at=$(echo "$token_resp" | grep '"expires_at":' | head -1 | sed 's/.*"expires_at": *"\([^"]*\)".*/\1/')
    fi

    [[ -n "$token" && "$token" != "null" ]] || die "Failed to get token from response: ${token_resp}"
    echo "Token expires at: ${expires_at}"
    echo

    echo "Verifying token..."
    local repo_resp
    repo_resp=$(github_api GET "/repos/${TARGET_OWNER}/${TARGET_REPO}" "$token" "token")

    local full_name
    if command -v jq &>/dev/null; then
        full_name=$(echo "$repo_resp" | jq -r '.full_name')
    else
        full_name=$(echo "$repo_resp" | grep '"full_name":' | head -1 | sed 's/.*"full_name": *"\([^"]*\)".*/\1/')
    fi
    echo "Verified: can access ${full_name}"
    echo

    echo "============================================================"
    echo "GITHUB_TOKEN=${token}"
    echo "============================================================"
}

# --- User access token via device flow ---

json_val() {
    # Extract a JSON string value by key (simple, no jq fallback)
    local json="$1" key="$2"
    if command -v jq &>/dev/null; then
        echo "$json" | jq -r ".$key // empty"
    else
        echo "$json" | sed -n 's/.*"'"$key"'": *"\{0,1\}\([^",}]*\)"\{0,1\}.*/\1/p' | head -1
    fi
}

cmd_user_token() {
    local client_id=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --client-id) client_id="$2"; shift 2 ;;
            *) die "Unknown option: $1" ;;
        esac
    done

    [[ -n "$client_id" ]] || die "Missing --client-id"

    echo "Requesting device code..."
    local device_resp
    device_resp=$(curl -s -X POST \
        -H "Accept: application/json" \
        -d "client_id=${client_id}" \
        "https://github.com/login/device/code")

    local device_code user_code verification_uri expires_in interval
    device_code=$(json_val "$device_resp" "device_code")
    user_code=$(json_val "$device_resp" "user_code")
    verification_uri=$(json_val "$device_resp" "verification_uri")
    expires_in=$(json_val "$device_resp" "expires_in")
    interval=$(json_val "$device_resp" "interval")
    interval=${interval:-5}

    [[ -n "$device_code" ]] || die "Failed to get device code: ${device_resp}"

    echo
    echo "============================================================"
    echo "  Open:  ${verification_uri}"
    echo "  Enter: ${user_code}"
    echo "============================================================"
    echo
    echo "Waiting for authorization (expires in ${expires_in}s)..."

    # Poll for the token
    while true; do
        sleep "$interval"
        local token_resp
        token_resp=$(curl -s -X POST \
            -H "Accept: application/json" \
            -d "client_id=${client_id}&device_code=${device_code}&grant_type=urn:ietf:params:oauth:grant-type:device_code" \
            "https://github.com/login/oauth/access_token")

        local error
        error=$(json_val "$token_resp" "error")

        case "$error" in
            authorization_pending) continue ;;
            slow_down)
                interval=$(json_val "$token_resp" "interval")
                interval=${interval:-$((interval + 5))}
                continue
                ;;
            expired_token) die "Device code expired. Please try again." ;;
            access_denied) die "Authorization was denied by the user." ;;
            "") ;; # No error — success
            *) die "${error}: $(json_val "$token_resp" "error_description")" ;;
        esac

        # Success
        local token expires_in_tok refresh_token
        token=$(json_val "$token_resp" "access_token")
        expires_in_tok=$(json_val "$token_resp" "expires_in")
        refresh_token=$(json_val "$token_resp" "refresh_token")

        [[ -n "$token" ]] || die "No access_token in response: ${token_resp}"

        echo "Authorization successful!"
        echo

        [[ -z "$expires_in_tok" ]] || echo "Token expires in: ${expires_in_tok}s"
        [[ -z "$refresh_token" ]] || echo "Refresh token: ${refresh_token}"
        echo

        echo "Verifying token..."
        local repo_resp
        repo_resp=$(curl -s \
            -H "Accept: application/vnd.github+json" \
            -H "Authorization: Bearer ${token}" \
            "${API}/repos/${TARGET_OWNER}/${TARGET_REPO}")

        local full_name
        full_name=$(json_val "$repo_resp" "full_name")
        echo "Verified: can access ${full_name}"
        echo

        echo "============================================================"
        echo "GITHUB_TOKEN=${token}"
        echo "============================================================"
        break
    done
}

# --- Main ---

case "${1:-}" in
    create-app) cmd_create_app ;;
    get-token) shift; cmd_get_token "$@" ;;
    user-token) shift; cmd_user_token "$@" ;;
    *)
        echo "Usage: $0 {create-app|get-token|user-token}"
        echo
        echo "Commands:"
        echo "  create-app   Print instructions to create the GitHub App"
        echo "  get-token    Generate a scoped installation token"
        echo "                 --app-id <ID>       GitHub App ID"
        echo "                 --pem-file <path>   Path to private key PEM"
        echo "  user-token   Generate a user access token via device flow"
        echo "                 --client-id <ID>    GitHub App Client ID"
        exit 1
        ;;
esac
