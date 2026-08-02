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
TG_TRADES_THREAD_BASE=${TG_TRADES_THREAD_BASE:-$TG_TRADES_THREAD}
TG_WINS_THREAD=${TG_WINS_THREAD-$TG_TRADES_THREAD}
TG_WINS_THREAD_MAINNET=${TG_WINS_THREAD_MAINNET-$TG_WINS_THREAD}
TG_WINS_THREAD_ARBITRUM=${TG_WINS_THREAD_ARBITRUM-$TG_WINS_THREAD}
TG_WINS_THREAD_GNOSIS=${TG_WINS_THREAD_GNOSIS-$TG_WINS_THREAD}
TG_WINS_THREAD_BASE=${TG_WINS_THREAD_BASE-$TG_WINS_THREAD}
TG_WIN_MAX_ORDERS=${TG_WIN_MAX_ORDERS:-5}
TG_WIN_STATE_FILE=${TG_WIN_STATE_FILE-./processed/tg-wins-seen.txt}
TG_SEND_TIMEOUT=${TG_SEND_TIMEOUT:-15}
TG_SEND_MAX_ATTEMPTS=${TG_SEND_MAX_ATTEMPTS:-3}

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
    base
    base-staging
)

idle_cycles=0
stats_cycle=0
hourly_auctions=0
hourly_quotes=0
hourly_orders=0
hourly_solutions=0
hourly_errors=0

mkdir -p "$(dirname "$TG_WIN_STATE_FILE")"
touch "$TG_WIN_STATE_FILE" 2>/dev/null || true

send_tg_to_chat() {
    local chat_id="$1"
    local thread_id="$2"
    local text="$3"
    local parse_mode="${4-Markdown}"
    local retry_on_rate_limit="${5:-0}"
    local args=(-d chat_id="$chat_id" -d text="$text")
    local response http_status body attempt retry_after
    [ -z "$chat_id" ] && return
    if [ -n "$parse_mode" ]; then
        args+=(-d parse_mode="$parse_mode")
    fi
    if [ -n "$thread_id" ]; then
        args+=(-d message_thread_id="$thread_id")
    fi

    attempt=1
    while true; do
        if ! response=$(curl -sS --max-time "$TG_SEND_TIMEOUT" -w $'\n%{http_code}' -X POST \
            "https://api.telegram.org/bot${TG_BOT_TOKEN}/sendMessage" \
            "${args[@]}" 2>&1); then
            echo "telegram send transport failed chat=${chat_id} thread=${thread_id:-default} attempt=${attempt}: ${response}" >&2
            return
        fi

        http_status="${response##*$'\n'}"
        body="${response%$'\n'*}"
        if [[ "$http_status" == 2* ]] && [[ "$body" == *'"ok":true'* ]]; then
            return
        fi

        if [[ "$retry_on_rate_limit" = "1" && "$http_status" = "429" && "$attempt" -lt "$TG_SEND_MAX_ATTEMPTS" ]]; then
            retry_after=1
            if [[ "$body" =~ \"retry_after\"[[:space:]]*:[[:space:]]*([0-9]+) ]]; then
                retry_after="${BASH_REMATCH[1]}"
            fi
            echo "telegram send rate limited chat=${chat_id} thread=${thread_id:-default} attempt=${attempt}; retrying after ${retry_after}s" >&2
            sleep "$retry_after"
            attempt=$((attempt + 1))
            continue
        fi

        echo "telegram send rejected chat=${chat_id} thread=${thread_id:-default} attempt=${attempt} http=${http_status}: ${body:0:500}" >&2
        return
    done
}

send_tg() {
    send_tg_to_chat "$TG_CHAT_ID" "$1" "$2" "${3-Markdown}"
}

send_win_tg() {
    local chain="$1"
    local text="$2"
    send_tg_to_chat "$TG_CHAT_ID" "$(chain_wins_thread "$chain")" "$text" "" "1"
}

service_chain() {
    case "$1" in
        solver|solver-staging) echo "mainnet" ;;
        arbitrum|arbitrum-staging) echo "arbitrum" ;;
        gnosis|gnosis-staging) echo "gnosis" ;;
        base|base-staging) echo "base" ;;
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
        base) echo "$TG_TRADES_THREAD_BASE" ;;
        *) echo "$TG_TRADES_THREAD" ;;
    esac
}

chain_wins_thread() {
    case "$1" in
        mainnet) echo "$TG_WINS_THREAD_MAINNET" ;;
        arbitrum) echo "$TG_WINS_THREAD_ARBITRUM" ;;
        gnosis) echo "$TG_WINS_THREAD_GNOSIS" ;;
        base) echo "$TG_WINS_THREAD_BASE" ;;
        *) echo "$TG_WINS_THREAD" ;;
    esac
}

cow_api_chain() {
    case "$1" in
        mainnet) echo "mainnet" ;;
        arbitrum) echo "arbitrum_one" ;;
        gnosis) echo "xdai" ;;
        base) echo "base" ;;
        *) echo "" ;;
    esac
}

explorer_order_url() {
    local chain="$1"
    local uid="$2"
    case "$chain" in
        arbitrum) echo "https://explorer.cow.fi/arb1/orders/${uid}" ;;
        gnosis) echo "https://explorer.cow.fi/gc/orders/${uid}" ;;
        base) echo "https://explorer.cow.fi/base/orders/${uid}" ;;
        *) echo "https://explorer.cow.fi/orders/${uid}" ;;
    esac
}

valid_order_uid() {
    [[ "$1" =~ ^0x[0-9a-fA-F]{112}$ ]]
}

explorer_tx_url() {
    local chain="$1"
    local tx="$2"
    case "$chain" in
        arbitrum) echo "https://arbiscan.io/tx/${tx}" ;;
        gnosis) echo "https://gnosisscan.io/tx/${tx}" ;;
        base) echo "https://basescan.io/tx/${tx}" ;;
        *) echo "https://etherscan.io/tx/${tx}" ;;
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
        base:0x833589fcd6edb6e08f4c7c32d4f71b54bda02913) echo "USDC|6" ;;
        base:0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca) echo "USDbC|6" ;;
        base:0x60a3e35cc302bfa44cb288bc5a4f316fdb1adb42) echo "EURC|6" ;;
        base:0x4200000000000000000000000000000000000006) echo "WETH|18" ;;
        base:0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22) echo "cbETH|18" ;;
        base:0xdbfefd2e8460a6ee4955a68582f85708baea60a3) echo "superOETHb|18" ;;
        base:0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf) echo "cbBTC|8" ;;
        base:0x0555e30da8f98308edb960aa94c0db47230d2b9c) echo "WBTC|8" ;;
        base:0x236aa50979d5f3de3bd1eeb40e81137f22ab794b) echo "tBTC|18" ;;
        base:0x417ac0e078398c154edfadd9ef675d30be60af93) echo "crvUSD|18" ;;
        base:0x646a737b9b6024e49f5908762b3ff73e65b5160c) echo "scrvUSD|18" ;;
        base:0x59d9356e565ab3a36dd77763fc0d87feaf85508c) echo "USDM|18" ;;
        base:0x50c5725949a6f0c72e6c4a641f24049a917db0cb) echo "DAI|18" ;;
        base:0x8ee73c484a26e0a5df2ee2a4960b789967dd0415) echo "CRV|18" ;;
        base:0x940181a94a35a4569e4529a3cdfb74e38fd98631) echo "AERO|18" ;;
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

win_seen() {
    local key="$1"
    [ -f "$TG_WIN_STATE_FILE" ] && grep -qxF "$key" "$TG_WIN_STATE_FILE"
}

mark_win_seen() {
    local key="$1"
    if ! win_seen "$key"; then
        echo "$key" >> "$TG_WIN_STATE_FILE" 2>/dev/null || true
        tail -n 2000 "$TG_WIN_STATE_FILE" > "${TG_WIN_STATE_FILE}.tmp" 2>/dev/null \
            && mv "${TG_WIN_STATE_FILE}.tmp" "$TG_WIN_STATE_FILE" 2>/dev/null || true
    fi
}

fetch_success_notifications() {
    command -v python3 >/dev/null 2>&1 || return

    python3 -c '
import json
import sys


def clean(value):
    if value is None:
        return ""
    if isinstance(value, (dict, list)):
        value = json.dumps(value, separators=(",", ":"))
    return str(value).replace("\t", " ").replace("\n", " ")


for raw_line in sys.stdin:
    raw_line = raw_line.rstrip("\n")
    if "driver notification" not in raw_line:
        continue

    service = ""
    payload = raw_line
    if "|" in raw_line:
        service, payload = raw_line.split("|", 1)
        service = service.strip().split()[0] if service.strip() else ""
    payload = payload.strip()

    try:
        outer = json.loads(payload)
    except Exception:
        continue

    fields = outer.get("fields") or {}
    if fields.get("message") != "driver notification":
        continue

    notification = fields.get("notification")
    if isinstance(notification, str):
        try:
            notification = json.loads(notification)
        except Exception:
            continue
    if not isinstance(notification, dict):
        continue
    if notification.get("kind") != "success":
        continue

    tx_hash = notification.get("transaction") or ""
    if not tx_hash:
        continue

    print("\t".join(clean(value) for value in [
        service,
        outer.get("timestamp", ""),
        notification.get("auctionId", ""),
        notification.get("solutionId", ""),
        tx_hash,
    ]))
' || true
}

fetch_transaction_orders() {
    local chain="$1"
    local tx_hash="$2"
    local api_chain

    command -v python3 >/dev/null 2>&1 || return
    api_chain="$(cow_api_chain "$chain")"
    [ -z "$api_chain" ] && return

    if ! python3 -c '
import json
import sys
import urllib.request

api_chain, tx_hash = sys.argv[1:]
url = f"https://api.cow.fi/{api_chain}/api/v1/transactions/{tx_hash}/orders"


def clean(value):
    if value is None:
        return ""
    return str(value).replace("\t", " ").replace("\n", " ")


try:
    req = urllib.request.Request(url, headers={"user-agent": "curve-lp-tg-monitor"})
    with urllib.request.urlopen(req, timeout=8) as response:
        orders = json.load(response)
except Exception:
    sys.exit(0)

if not isinstance(orders, list):
    sys.exit(0)

for order in orders:
    if not isinstance(order, dict):
        continue
    print("\t".join(clean(value) for value in [
        order.get("uid") or order.get("orderUid") or order.get("id") or "",
        order.get("sellToken", ""),
        order.get("buyToken", ""),
        order.get("executedSellAmount") or order.get("sellAmount") or "",
        order.get("executedBuyAmount") or order.get("buyAmount") or "",
        order.get("kind", ""),
        order.get("status", ""),
    ]))
' "$api_chain" "$tx_hash"
    then
        true
    fi
}

send_win_notification_for_success() {
    local service_name="$1"
    local timestamp="$2"
    local auction_id="$3"
    local solution_id="$4"
    local tx_hash="$5"
    local normalized_service chain env_name key

    normalized_service="$(normalize_service_name "$service_name")"
    chain="$(service_chain "$normalized_service")"
    env_name="$(service_env "$normalized_service")"
    [ "$env_name" = "prod" ] || return
    [ "$chain" != "unknown" ] || return
    [ -n "$tx_hash" ] || return

    key="notify-success:${chain}:${tx_hash}:${auction_id}:${solution_id}"
    if win_seen "$key"; then
        return
    fi
    mark_win_seen "$key"

    local msg order_count shown uid sell_token buy_token sell_amount buy_amount kind status
    msg="Auction Won
Chain: ${chain}"
    if [ -n "$auction_id" ]; then
        msg+="
Auction: ${auction_id}"
    fi
    if [ -n "$solution_id" ]; then
        msg+="
Solution: ${solution_id}"
    fi
    if [ -n "$timestamp" ]; then
        msg+="
Notified: ${timestamp}"
    fi
    msg+="
Tx: $(explorer_tx_url "$chain" "$tx_hash")"

    order_count=0
    shown=0
    while IFS=$'\t' read -r uid sell_token buy_token sell_amount buy_amount kind status; do
        [ -n "$uid$sell_token$buy_token$sell_amount$buy_amount" ] || continue
        order_count=$((order_count + 1))
        if [ "$shown" -ge "$TG_WIN_MAX_ORDERS" ]; then
            continue
        fi
        shown=$((shown + 1))

        local sell_short buy_short sell_display buy_display
        sell_short="${sell_token:0:6}...${sell_token: -4}"
        buy_short="${buy_token:0:6}...${buy_token: -4}"
        sell_display="$(format_token_amount "$chain" "$sell_token" "$sell_amount")"
        buy_display="$(format_token_amount "$chain" "$buy_token" "$buy_amount")"

        if [ "$shown" -eq 1 ]; then
            msg+="
Orders:"
        fi
        msg+="
- ${sell_short} -> ${buy_short}: ${sell_display} -> ${buy_display}"
        if [ -n "$kind$status" ]; then
            msg+=" (${kind}${status:+, ${status}})"
        fi
        if valid_order_uid "$uid"; then
            msg+="
  $(explorer_order_url "$chain" "$uid")"
        fi
    done < <(fetch_transaction_orders "$chain" "$tx_hash")

    if [ "$order_count" -eq 0 ]; then
        msg+="
Orders: unavailable from CoW API"
    elif [ "$order_count" -gt "$shown" ]; then
        msg+="
... plus $((order_count - shown)) more order(s)"
    fi

    send_win_tg "$chain" "$msg"
}

# Startup message
send_tg "$TG_STATS_THREAD" "🟢 Solver monitor started

Watching: mainnet, arbitrum, gnosis, base
Trades threads: mainnet=${TG_TRADES_THREAD_MAINNET:-default}, arbitrum=${TG_TRADES_THREAD_ARBITRUM:-default}, gnosis=${TG_TRADES_THREAD_GNOSIS:-default}, base=${TG_TRADES_THREAD_BASE:-default}
Wins threads: mainnet=${TG_WINS_THREAD_MAINNET:-default}, arbitrum=${TG_WINS_THREAD_ARBITRUM:-default}, gnosis=${TG_WINS_THREAD_GNOSIS:-default}, base=${TG_WINS_THREAD_BASE:-default}"

while true; do
    sleep "$INTERVAL"

    # Grab last 5 min of logs
    logs=$(docker compose -f "$COMPOSE_FILE" logs --since 5m "${SOLVER_SERVICES[@]}" 2>&1 | tr -d '\000' || true)

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

    # The driver sends /notify only to this solver endpoint. A success
    # notification is the ownership signal for a settled win; the competition API
    # can include the same user order in other solvers' winning settlements.
    while IFS=$'\t' read -r service_name timestamp auction_id solution_id tx_hash; do
        send_win_notification_for_success "$service_name" "$timestamp" "$auction_id" "$solution_id" "$tx_hash"
    done < <(printf '%s\n' "$logs" | fetch_success_notifications)

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
        effective_buy_amt=$(echo "$log_line" | grep -oP '"effective_buy_amount":"\K[0-9]+' || true)
        fee_amt=$(echo "$log_line" | grep -oP '"fee_in_sell_token":"\K[0-9]+' || true)
        estimated_gas=$(echo "$log_line" | grep -oP '"estimated_gas":"\K[0-9]+' || true)
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
        effective_output_display="$output_display"
        if [ -n "$effective_buy_amt" ]; then
            effective_output_display="$(format_token_amount "$chain" "$buy_tok" "$effective_buy_amt")"
        fi

        msg="Solution Candidate
Chain: ${chain} | Env: ${env_name}
${sell_short} -> ${buy_short}
Side: ${side}
Input: ${input_display}
Output: ${effective_output_display}"
        if [ -n "$effective_buy_amt" ] && [ "$effective_buy_amt" != "$buy_amt" ]; then
            msg+="
Route output: ${output_display}"
        fi
        if [ -n "$fee_amt" ] && [ "$fee_amt" != "0" ]; then
            msg+="
Fee: $(format_token_amount "$chain" "$sell_tok" "$fee_amt")"
        fi
        if [ -n "$estimated_gas" ]; then
            msg+="
Gas: ${estimated_gas}"
        fi
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
        if valid_order_uid "$uid"; then
            msg+="
Order: $(explorer_order_url "$chain" "$uid")"
        else
            msg+="
Order UID: unavailable"
        fi
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
