#!/bin/bash
# Curve LP Solver — Telegram Monitor
# Sends stats summaries and trade alerts to Telegram.
#
# Usage:
#   # Add TG_BOT_TOKEN, TG_CHAT_ID to .env first
#   nohup ./tg-monitor.sh > tg-monitor.log 2>&1 &

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Load env
if [ -f .env ]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi

: "${TG_BOT_TOKEN:?Set TG_BOT_TOKEN in .env}"
: "${TG_CHAT_ID:?Set TG_CHAT_ID in .env}"
TG_STATS_THREAD=${TG_STATS_THREAD:-}    # General topic (empty = default)
TG_TRADES_THREAD=${TG_TRADES_THREAD:-3}  # Wins topic

COMPOSE_FILE="docker-compose.prod.yml"
INTERVAL=300  # 5 minutes
IDLE_REPORT_CYCLES=6  # report idle every 30 min

idle_cycles=0

send_tg() {
    local thread_id="$1"
    local text="$2"
    local args=(-d chat_id="$TG_CHAT_ID" -d text="$text" -d parse_mode="Markdown")
    if [ -n "$thread_id" ]; then
        args+=(-d message_thread_id="$thread_id")
    fi
    curl -s -X POST \
        "https://api.telegram.org/bot${TG_BOT_TOKEN}/sendMessage" \
        "${args[@]}" \
        > /dev/null 2>&1 || true
}

# Startup message
send_tg "$TG_STATS_THREAD" "🟢 Solver monitor started"

while true; do
    sleep "$INTERVAL"

    # Grab last 5 min of solver logs
    logs=$(docker compose -f "$COMPOSE_FILE" logs --since 5m solver 2>&1 || true)

    if [ -z "$logs" ]; then
        idle_cycles=$((idle_cycles + 1))
        if [ $((idle_cycles % IDLE_REPORT_CYCLES)) -eq 0 ]; then
            mins=$((idle_cycles * INTERVAL / 60))
            send_tg "$TG_STATS_THREAD" "💤 Solver idle — 0 auctions in last ${mins}m"
        fi
        continue
    fi

    # Count stats
    auctions=$(echo "$logs" | grep -c "Curve LP solver completed" || true)
    solutions=$(echo "$logs" | grep -oP 'num_solutions=\K[0-9]+' | awk '{s+=$1} END {print s+0}' || true)
    orders=$(echo "$logs" | grep -c "processing Curve LP order" || true)
    errors=$(echo "$logs" | grep -c "failed to solve order" || true)

    # Send winning trade alerts immediately
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        uid=$(echo "$line" | grep -oP 'order_uid=\K\S+' || echo "???")
        sell_tok=$(echo "$line" | grep -oP 'sell_token=TokenAddress\(\K0x[a-fA-F0-9]+' || echo "???")
        buy_tok=$(echo "$line" | grep -oP 'buy_token=TokenAddress\(\K0x[a-fA-F0-9]+' || echo "???")
        sell_amt=$(echo "$line" | grep -oP 'sell_amount=\K[0-9]+' || echo "???")
        buy_amt=$(echo "$line" | grep -oP 'buy_amount=\K[0-9]+' || echo "???")

        msg="🎯 *Winning Trade!*
Order: \`${uid}\`
Sell: \`${sell_tok}\` (${sell_amt})
Buy: \`${buy_tok}\` (${buy_amt})
https://explorer.cow.fi/orders/${uid}"
        send_tg "$TG_TRADES_THREAD" "$msg"
    done < <(echo "$logs" | grep '"solved order"' || true)

    # Send stats if there was activity
    if [ "$auctions" -gt 0 ] || [ "$orders" -gt 0 ] || [ "$errors" -gt 0 ]; then
        idle_cycles=0
        stats="📊 *Solver Stats (last 5m)*
Auctions: ${auctions}
Orders: ${orders}
Solutions: ${solutions}
Errors: ${errors}"
        send_tg "$TG_STATS_THREAD" "$stats"
    else
        idle_cycles=$((idle_cycles + 1))
        if [ $((idle_cycles % IDLE_REPORT_CYCLES)) -eq 0 ]; then
            mins=$((idle_cycles * INTERVAL / 60))
            send_tg "$TG_STATS_THREAD" "💤 Solver idle — 0 auctions in last ${mins}m"
        fi
    fi
done
