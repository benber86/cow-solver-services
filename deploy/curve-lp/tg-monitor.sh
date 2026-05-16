#!/bin/bash
# Curve LP Solver — Telegram Monitor
# Sends stats summaries and trade alerts to Telegram.
#
# Usage:
#   # Add TG_BOT_TOKEN, TG_CHAT_ID to .env first
#   nohup ./tg-monitor.sh >/dev/null 2>&1 &

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
TG_STATS_THREAD=${TG_STATS_THREAD:-}                 # General topic (empty = default)
TG_TRADES_THREAD=${TG_TRADES_THREAD:-3}             # Legacy/default trades topic
TG_TRADES_THREAD_MAINNET=${TG_TRADES_THREAD_MAINNET:-$TG_TRADES_THREAD}
TG_TRADES_THREAD_ARBITRUM=${TG_TRADES_THREAD_ARBITRUM:-$TG_TRADES_THREAD}
TG_TRADES_THREAD_GNOSIS=${TG_TRADES_THREAD_GNOSIS:-$TG_TRADES_THREAD}

COMPOSE_FILE="docker-compose.prod.yml"
INTERVAL=300  # 5 minutes
STATS_REPORT_CYCLES=12  # stats every 12 cycles (1 hour)
IDLE_REPORT_CYCLES=6    # report idle every 30 min
SOLVER_SERVICES=(
    solver
    solver-staging
    arbitrum
    arbitrum-staging
    gnosis
    gnosis-staging
)

idle_cycles=0
stats_cycle=0
hourly_auctions=0
hourly_quotes=0
hourly_orders=0
hourly_solutions=0
hourly_errors=0

send_tg() {
    local thread_id="$1"
    local text="$2"
    local parse_mode="${3-Markdown}"
    local args=(-d chat_id="$TG_CHAT_ID" -d text="$text")
    if [ -n "$parse_mode" ]; then
        args+=(-d parse_mode="$parse_mode")
    fi
    if [ -n "$thread_id" ]; then
        args+=(-d message_thread_id="$thread_id")
    fi
    curl -s -X POST \
        "https://api.telegram.org/bot${TG_BOT_TOKEN}/sendMessage" \
        "${args[@]}" \
        > /dev/null 2>&1 || true
}

service_chain() {
    case "$1" in
        solver|solver-staging) echo "mainnet" ;;
        arbitrum|arbitrum-staging) echo "arbitrum" ;;
        gnosis|gnosis-staging) echo "gnosis" ;;
        *) echo "unknown" ;;
    esac
}

service_env() {
    case "$1" in
        *-staging) echo "staging" ;;
        *) echo "prod" ;;
    esac
}

chain_thread() {
    case "$1" in
        mainnet) echo "$TG_TRADES_THREAD_MAINNET" ;;
        arbitrum) echo "$TG_TRADES_THREAD_ARBITRUM" ;;
        gnosis) echo "$TG_TRADES_THREAD_GNOSIS" ;;
        *) echo "$TG_TRADES_THREAD" ;;
    esac
}

normalize_service_name() {
    local name="$1"
    name="${name#"${name%%[![:space:]]*}"}"
    name="${name%"${name##*[![:space:]]}"}"
    name="${name%-1}"
    echo "$name"
}

token_meta() {
    local chain="$1"
    local token="${2,,}"

    case "${chain}:${token}" in
        arbitrum:0xaf88d065e77c8cc2239327c5edb3a432268e5831) echo "USDC|6" ;;
        arbitrum:0xff970a61a04b1ca14834a43f5de4533ebddb5cc8) echo "USDC.e|6" ;;
        arbitrum:0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9) echo "USDT|6" ;;
        arbitrum:0x2f2a2543b76a4166549f7aab2e75bef0aefc5b0f) echo "WBTC|8" ;;
        arbitrum:0x82af49447d8a07e3bd95bd0d56f35241523fbab1) echo "WETH|18" ;;
        arbitrum:0x912ce59144191c1204e64559fe8253a0e49e6548) echo "ARB|18" ;;
        arbitrum:0x11cdb42b0eb46d95f990bedd4695a6e3fa034978) echo "CRV|18" ;;
        arbitrum:0x498bf2b1e120fed3ad3d42ea2165e9b73f99c1e5) echo "crvUSD|18" ;;
        gnosis:0xddafbb505ad214d7b80b1f830fccc89b60fb7a83) echo "USDC|6" ;;
        gnosis:0x4ecaba5870353805a9f068101a40e0f32ed605c6) echo "USDT|6" ;;
        gnosis:0x6a023ccd1ff6f2045c3309768ead9e68f978f6e1) echo "WETH|18" ;;
        gnosis:0x9c58bacc331c9aa871afd802db6379a98e80cedb) echo "GNO|18" ;;
        gnosis:0x44fa8e6f47987339850636f88629646662444217) echo "WXDAI|18" ;;
        gnosis:0xe91d153e0b41518a2ce8dd3d7944fa863463a97d) echo "WXDAI|18" ;;
        gnosis:0xcb444e90d8198415266c6a2724b7900fb12fc56e) echo "EURe|18" ;;
        gnosis:0x83f20f44975d03b1b09e64809b757c47f942beea) echo "sDAI|18" ;;
        gnosis:0x2a22f9c3b484c3629090feed35f17ff8f88f76f0) echo "USDC.e|6" ;;
        mainnet:0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48) echo "USDC|6" ;;
        mainnet:0xdac17f958d2ee523a2206206994597c13d831ec7) echo "USDT|6" ;;
        mainnet:0x6b175474e89094c44da98b954eedeac495271d0f) echo "DAI|18" ;;
        mainnet:0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2) echo "WETH|18" ;;
        mainnet:0xd533a949740bb3306d119cc777fa900ba034cd52) echo "CRV|18" ;;
        *) echo "raw|18" ;;
    esac
}

format_amount() {
    local raw="$1"
    local decimals="$2"

    if [[ ! "$raw" =~ ^[0-9]+$ ]] || [[ ! "$decimals" =~ ^[0-9]+$ ]]; then
        echo "$raw"
        return
    fi

    awk -v raw="$raw" -v decimals="$decimals" '
        BEGIN {
            if (decimals == 0) {
                print raw;
                exit;
            }
            if (length(raw) <= decimals) {
                whole = "0";
                frac = raw;
                while (length(frac) < decimals) {
                    frac = "0" frac;
                }
            } else {
                whole = substr(raw, 1, length(raw) - decimals);
                frac = substr(raw, length(raw) - decimals + 1);
            }
            sub(/0+$/, "", frac);
            if (frac == "") {
                print whole;
            } else {
                print whole "." frac;
            }
        }'
}

format_token_amount() {
    local chain="$1"
    local token="$2"
    local raw="$3"
    local meta symbol decimals normalized

    meta="$(token_meta "$chain" "$token")"
    symbol="${meta%%|*}"
    decimals="${meta##*|}"
    normalized="$(format_amount "$raw" "$decimals")"

    if [ "$symbol" = "raw" ]; then
        echo "${normalized} (raw ${raw}, assumed 18 decimals)"
    else
        echo "${normalized} ${symbol} (raw ${raw})"
    fi
}

# Startup message
send_tg "$TG_STATS_THREAD" "🟢 Solver monitor started

Watching: mainnet, arbitrum, gnosis
Trades threads: mainnet=${TG_TRADES_THREAD_MAINNET:-default}, arbitrum=${TG_TRADES_THREAD_ARBITRUM:-default}, gnosis=${TG_TRADES_THREAD_GNOSIS:-default}"

while true; do
    sleep "$INTERVAL"

    # Grab last 5 min of logs
    logs=$(docker compose -f "$COMPOSE_FILE" logs --since 5m "${SOLVER_SERVICES[@]}" 2>&1 || true)

    if [ -z "$logs" ]; then
        idle_cycles=$((idle_cycles + 1))
        if [ $((idle_cycles % IDLE_REPORT_CYCLES)) -eq 0 ]; then
            mins=$((idle_cycles * INTERVAL / 60))
            send_tg "$TG_STATS_THREAD" "💤 Solver idle — 0 auctions in last ${mins}m"
        fi
        continue
    fi

    # Count stats (JSON log format — exclude quotes from auction counts)
    auctions=$(echo "$logs" | grep '"solve_completed"' | grep -c '"is_quote":false' || true)
    quotes=$(echo "$logs" | grep '"solve_completed"' | grep -c '"is_quote":true' || true)
    solutions=$(echo "$logs" | grep '"solve_completed"' | grep '"is_quote":false' | grep -oP '"num_solutions":\K[0-9]+' | awk '{s+=$1} END {print s+0}' || true)
    orders=$(echo "$logs" | grep -c '"processing Curve LP order"' || true)
    errors=$(echo "$logs" | grep -c '"failed to solve order"' || true)

    # Log candidate solutions for real auctions (not quotes).
    # Note: "solved order" means the solver produced a candidate, NOT that it
    # won the competition or was settled on-chain. The driver selects among
    # competing solvers; we have no visibility into that outcome here.
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        service_prefix="${line%%|*}"
        service_name="$(normalize_service_name "$service_prefix")"
        log_line="$line"
        if [[ "$line" == *"|"* ]]; then
            log_line="${line#*| }"
        fi

        chain="$(service_chain "$service_name")"
        env_name="$(service_env "$service_name")"
        thread_id="$(chain_thread "$chain")"

        uid=$(echo "$log_line" | grep -oP '"order_uid":"\K[^"]+' || echo "???")
        sell_tok=$(echo "$log_line" | grep -oP '"sell_token":"TokenAddress\(\K0x[a-fA-F0-9]+' || echo "???")
        buy_tok=$(echo "$log_line" | grep -oP '"buy_token":"TokenAddress\(\K0x[a-fA-F0-9]+' || echo "???")
        sell_amt=$(echo "$log_line" | grep -oP '"sell_amount":"\K[0-9]+' || echo "???")
        input_amt=$(echo "$log_line" | grep -oP '"solution_input":"\K[0-9]+' || true)
        if [ -z "$input_amt" ]; then
            input_amt="$sell_amt"
        fi
        buy_amt=$(echo "$log_line" | grep -oP '"solution_output":"\K[0-9]+' || echo "???")
        side=$(echo "$log_line" | grep -oP '"side":"\K[^"]+' || echo "???")
        # New-router / legacy telemetry (sidechain only). Empty if absent.
        quality=$(echo "$log_line" | grep -oP '"new_router_quality":"\K[^"]+' || true)
        legacy_out=$(echo "$log_line" | grep -oP '"legacy_output":"\K[0-9]+' || true)
        delta_bps=$(echo "$log_line" | grep -oP '"delta_bps":\K-?[0-9]+' || true)

        # Shorten addresses for readability
        sell_short="${sell_tok:0:6}...${sell_tok: -4}"
        buy_short="${buy_tok:0:6}...${buy_tok: -4}"
        input_display="$(format_token_amount "$chain" "$sell_tok" "$input_amt")"
        output_display="$(format_token_amount "$chain" "$buy_tok" "$buy_amt")"

        msg="Solution Candidate
Chain: ${chain} | Env: ${env_name}
${sell_short} -> ${buy_short}
Side: ${side}
Input: ${input_display}
Output: ${output_display}"
        if [ -n "$quality" ]; then
            msg+="
Quality: ${quality}"
        fi
        if [ -n "$legacy_out" ]; then
            msg+="
Legacy ref: ${legacy_out}"
            if [ -n "$delta_bps" ]; then
                msg+=" (Δ ${delta_bps}bps)"
            fi
        fi
        msg+="
Order: https://explorer.cow.fi/orders/${uid}"
        send_tg "$thread_id" "$msg" ""
    done < <(echo "$logs" | grep '"solved order"' | grep '"is_quote":false' || true)

    # Accumulate hourly stats
    hourly_auctions=$((hourly_auctions + auctions))
    hourly_quotes=$((hourly_quotes + quotes))
    hourly_orders=$((hourly_orders + orders))
    hourly_solutions=$((hourly_solutions + solutions))
    hourly_errors=$((hourly_errors + errors))
    stats_cycle=$((stats_cycle + 1))

    # Track idle (only real auctions count as activity)
    if [ "$auctions" -gt 0 ]; then
        idle_cycles=0
    else
        idle_cycles=$((idle_cycles + 1))
        if [ $((idle_cycles % IDLE_REPORT_CYCLES)) -eq 0 ]; then
            mins=$((idle_cycles * INTERVAL / 60))
            send_tg "$TG_STATS_THREAD" "💤 Solver idle — 0 auctions in last ${mins}m"
        fi
    fi

    # Send hourly stats summary
    if [ $((stats_cycle % STATS_REPORT_CYCLES)) -eq 0 ]; then
        stats="📊 *Solver Stats (last 1h)*
Auctions: ${hourly_auctions}
Quotes: ${hourly_quotes}
Orders processed: ${hourly_orders}
Solution candidates: ${hourly_solutions}
Errors: ${hourly_errors}"
        send_tg "$TG_STATS_THREAD" "$stats"
        hourly_auctions=0
        hourly_quotes=0
        hourly_orders=0
        hourly_solutions=0
        hourly_errors=0
    fi
done
