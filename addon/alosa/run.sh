#!/bin/sh
# Starts the server with the add-on options from /data/options.json and tells
# Home Assistant where it is and which API token to use.
set -e

OPTIONS=/data/options.json
PORT=8765

if [ -f "$OPTIONS" ]; then
    ALOSA_AUTO_ADOPT=$(jq -r '.auto_adopt' "$OPTIONS")
    LOG_LEVEL=$(jq -r '.log_level' "$OPTIONS")
    # The level applies to the server's own messages; libraries only report
    # warnings, or "debug" would be drowned out by mDNS chatter.
    RUST_LOG="warn,alosa_server=$LOG_LEVEL"
    export ALOSA_AUTO_ADOPT RUST_LOG
fi
export ALOSA_PORT="$PORT"

# Announce once the server is listening, so Home Assistant's setup flow can
# connect right away. Runs on every start; Home Assistant updates the existing
# entry if the token or address changed.
announce() {
    for _ in $(seq 1 30); do
        if curl -fs "http://127.0.0.1:$PORT/health" >/dev/null; then
            if jq -n --arg token "$(cat /data/api_token)" --argjson port "$PORT" \
                '{service: "alosa", config: {host: "127.0.0.1", port: $port, api_token: $token}}' |
                curl -fsS -X POST \
                    -H "Authorization: Bearer $SUPERVISOR_TOKEN" \
                    -H "Content-Type: application/json" \
                    --data @- http://supervisor/discovery >/dev/null; then
                echo "Announced the server to Home Assistant"
            else
                echo "Could not announce the server to Home Assistant; add the integration manually"
            fi
            return
        fi
        sleep 1
    done
}
if [ -n "${SUPERVISOR_TOKEN:-}" ]; then
    announce &
fi

# The server replaces this script, so the Supervisor's stop signal reaches it directly.
exec /usr/local/bin/alosa-server
