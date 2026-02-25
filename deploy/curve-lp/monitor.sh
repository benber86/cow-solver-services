#!/bin/bash
# Curve LP Solver Monitor
# Tails solver logs and records winning trades to a log file.
#
# Usage:
#   ./monitor.sh              # tail live + log to file
#   ./monitor.sh --history    # scan existing logs first, then tail
#
# Output: ./trades.log (append-only)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

TRADES_LOG="$SCRIPT_DIR/trades.log"
COMPOSE_FILE="docker-compose.prod.yml"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${GREEN}=== Curve LP Solver Monitor ===${NC}"
echo -e "Trades log: ${CYAN}${TRADES_LOG}${NC}"
echo ""

# Stats
TOTAL_AUCTIONS=0
TOTAL_SOLUTIONS=0
TOTAL_ORDERS=0
TOTAL_ERRORS=0

print_stats() {
    echo -e "\r${CYAN}[stats]${NC}" \
        "auctions=${TOTAL_AUCTIONS}" \
        "orders=${TOTAL_ORDERS}" \
        "solutions=${GREEN}${TOTAL_SOLUTIONS}${NC}" \
        "errors=${TOTAL_ERRORS}    "
}

process_line() {
    local line="$1"

    # Count auctions
    if echo "$line" | grep -q "Curve LP solver completed"; then
        TOTAL_AUCTIONS=$((TOTAL_AUCTIONS + 1))
        local n
        n=$(echo "$line" | grep -oP 'num_solutions=\K[0-9]+' || echo "0")
        if [ "$n" -gt 0 ]; then
            TOTAL_SOLUTIONS=$((TOTAL_SOLUTIONS + n))
        fi
        print_stats
    fi

    # Count processed orders
    if echo "$line" | grep -q "processing Curve LP order"; then
        TOTAL_ORDERS=$((TOTAL_ORDERS + 1))
    fi

    # Count errors
    if echo "$line" | grep -q "failed to solve order"; then
        TOTAL_ERRORS=$((TOTAL_ERRORS + 1))
    fi

    # Winning trade
    if echo "$line" | grep -q '"solved order"'; then
        local ts uid sell_tok buy_tok sell_amt buy_amt
        ts=$(echo "$line" | grep -oP '^\S+' || echo "???")
        uid=$(echo "$line" | grep -oP 'order_uid=\K\S+' || echo "???")
        sell_tok=$(echo "$line" | \
            grep -oP 'sell_token=TokenAddress\(\K0x[a-fA-F0-9]+' \
            || echo "???")
        buy_tok=$(echo "$line" | \
            grep -oP 'buy_token=TokenAddress\(\K0x[a-fA-F0-9]+' \
            || echo "???")
        sell_amt=$(echo "$line" | \
            grep -oP 'sell_amount=\K[0-9]+' || echo "???")
        buy_amt=$(echo "$line" | \
            grep -oP 'buy_amount=\K[0-9]+' || echo "???")

        # Write to log file
        echo "${ts} TRADE uid=${uid}" \
            "sell=${sell_tok} amt=${sell_amt}" \
            "buy=${buy_tok} amt=${buy_amt}" \
            >> "$TRADES_LOG"

        # Print to terminal
        echo ""
        echo -e "${GREEN}*** WINNING TRADE ***${NC}"
        echo -e "  Time:  ${ts}"
        echo -e "  Order: ${YELLOW}${uid}${NC}"
        echo -e "  Sell:  ${sell_tok} (${sell_amt})"
        echo -e "  Buy:   ${buy_tok} (${buy_amt})"
        echo -e "  Explorer: https://explorer.cow.fi/orders/${uid}"
        echo ""
    fi
}

# Scan history first if requested
if [ "${1:-}" = "--history" ]; then
    echo -e "${YELLOW}Scanning historical logs...${NC}"
    while IFS= read -r line; do
        process_line "$line"
    done < <(docker compose -f "$COMPOSE_FILE" logs solver 2>&1)
    echo ""
    echo -e "${GREEN}History scan done.${NC}"
    print_stats
    echo ""
fi

# Tail live logs
echo -e "${YELLOW}Tailing live logs (Ctrl+C to stop)...${NC}"
echo ""

docker compose -f "$COMPOSE_FILE" logs -f solver 2>&1 | \
    while IFS= read -r line; do
        process_line "$line"
    done
