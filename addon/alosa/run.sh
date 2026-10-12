#!/bin/sh
# Starts the server with the add-on options from /data/options.json, then
# tells Home Assistant where it is and which API token to use.
set -e

OPTIONS=/data/options.json
PORT=8765

if [ -f "$OPTIONS" ]; then
    ALOSA_AUTO_ADOPT=$(jq -r '.auto_adopt' "$OPTIONS")
    RUST_LOG=$(jq -r '.log_level' "$OPTIONS")
    export ALOSA_AUTO_ADOPT RUST_LOG
fi
export ALOSA_PORT="$PORT"

/usr/local/bin/alosa-server &
SERVER=$!
trap 'kill -TERM "$SERVER" 2>/dev/null' TERM INT

# Announce only once the server is listening, so Home Assistant's setup flow
# can connect right away. Runs on every start; Home Assistant updates the
# existing entry if the token or address changed.
if [ -n "$SUPERVISOR_TOKEN" ]; then
    for _ in $(seq 1 30); do
        if curl -fs "http://127.0.0.1:$PORT/health" >/dev/null; then
            jq -n --arg token "$(cat /data/api_token)" --argjson port "$PORT" \
                '{service: "alosa", config: {host: "127.0.0.1", port: $port, api_token: $token}}' |
                curl -fsS -X POST \
                    -H "Authorization: Bearer $SUPERVISOR_TOKEN" \
                    -H "Content-Type: application/json" \
                    --data @- http://supervisor/discovery >/dev/null &&
                echo "Announced the server to Home Assistant" ||
                echo "Could not announce the server to Home Assistant; add the integration manually"
            break
        fi
        sleep 1
    done
fi

wait "$SERVER"
