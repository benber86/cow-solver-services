#!/bin/bash
set -euo pipefail

# Curve LP Solver - Deployment Script
#
# Solver and ingress are decoupled. The default rebuilds solver containers
# only and leaves nginx / certbot alone; use --with-ingress or --ingress-only
# to touch ingress explicitly.
#
# Solver selection flags (ignored with --ingress-only):
#   --skip-prod       Drop the `solver` (ETH mainnet prod) service from the
#                     rebuild set.
#   --chains=CSV      Restrict to a chain subset. Comma-separated values from:
#                     mainnet (= solver + solver-staging), arbitrum, gnosis.
#                     Default: all chains.
#
# Ingress flags (mutually exclusive):
#   --with-ingress    Also rebuild nginx + certbot alongside the selected
#                     solvers. Use this when the selected solver services'
#                     public routing might have changed (new chain, new URL).
#   --ingress-only    Rebuild nginx + certbot only. Skip all solver services.
#                     Use this for a pure ingress refresh (e.g. nginx.conf
#                     edit, cert troubleshooting).
#
# Miscellaneous:
#   -h, --help        Print usage.
#
# Examples:
#   ./deploy.sh                                # all solvers, no ingress
#   ./deploy.sh --skip-prod                    # staging + arbitrum + gnosis
#   ./deploy.sh --chains=arbitrum,gnosis       # only those two
#   ./deploy.sh --chains=arbitrum --with-ingress  # arbitrum + nginx + certbot
#   ./deploy.sh --ingress-only                 # pure ingress refresh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

usage() {
    sed -n '3,35p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

# ---------- flag parsing ----------

SKIP_PROD=0
CHAINS_CSV=""
WITH_INGRESS=0
INGRESS_ONLY=0

while [ $# -gt 0 ]; do
    case "$1" in
        --skip-prod)
            SKIP_PROD=1
            shift
            ;;
        --chains=*)
            CHAINS_CSV="${1#--chains=}"
            shift
            ;;
        --chains)
            CHAINS_CSV="${2:-}"
            shift 2
            ;;
        --with-ingress)
            WITH_INGRESS=1
            shift
            ;;
        --ingress-only)
            INGRESS_ONLY=1
            shift
            ;;
        -h|--help)
            usage 0
            ;;
        *)
            echo -e "${RED}ERROR: unknown argument: $1${NC}" >&2
            usage 1
            ;;
    esac
done

# Mutex: --ingress-only conflicts with every solver-selection flag.
if [ "$INGRESS_ONLY" = "1" ]; then
    if [ "$WITH_INGRESS" = "1" ]; then
        echo -e "${RED}ERROR: --ingress-only and --with-ingress are mutually exclusive${NC}" >&2
        exit 1
    fi
    if [ "$SKIP_PROD" = "1" ] || [ -n "$CHAINS_CSV" ]; then
        echo -e "${RED}ERROR: --ingress-only cannot be combined with --skip-prod or --chains${NC}" >&2
        exit 1
    fi
fi

# ---------- compute the solver service set ----------

SOLVER_SERVICES=()
REBUILD_PROD=0
REBUILD_STAGING=0
REBUILD_ARBITRUM=0
REBUILD_GNOSIS=0

if [ "$INGRESS_ONLY" = "0" ]; then
    declare -A CHAIN_ENABLED=([mainnet]=0 [arbitrum]=0 [gnosis]=0)
    if [ -z "$CHAINS_CSV" ]; then
        CHAIN_ENABLED[mainnet]=1
        CHAIN_ENABLED[arbitrum]=1
        CHAIN_ENABLED[gnosis]=1
    else
        IFS=',' read -r -a _CHAIN_LIST <<< "$CHAINS_CSV"
        for c in "${_CHAIN_LIST[@]}"; do
            c_trimmed="$(echo "$c" | tr -d '[:space:]')"
            if [ -z "$c_trimmed" ]; then continue; fi
            if [ -z "${CHAIN_ENABLED[$c_trimmed]+set}" ]; then
                echo -e "${RED}ERROR: unknown chain '$c_trimmed' in --chains. Expected: mainnet, arbitrum, gnosis${NC}" >&2
                exit 1
            fi
            CHAIN_ENABLED[$c_trimmed]=1
        done
    fi

    if [ "${CHAIN_ENABLED[mainnet]}" = "1" ]; then
        if [ "$SKIP_PROD" = "0" ]; then
            SOLVER_SERVICES+=(solver)
            REBUILD_PROD=1
        fi
        SOLVER_SERVICES+=(solver-staging)
        REBUILD_STAGING=1
    fi
    if [ "${CHAIN_ENABLED[arbitrum]}" = "1" ]; then
        SOLVER_SERVICES+=(arbitrum)
        REBUILD_ARBITRUM=1
    fi
    if [ "${CHAIN_ENABLED[gnosis]}" = "1" ]; then
        SOLVER_SERVICES+=(gnosis)
        REBUILD_GNOSIS=1
    fi

    if [ ${#SOLVER_SERVICES[@]} -eq 0 ]; then
        echo -e "${RED}ERROR: no solver services selected. Check --chains / --skip-prod combination.${NC}" >&2
        exit 1
    fi
fi

INGRESS_SERVICES=()
if [ "$WITH_INGRESS" = "1" ] || [ "$INGRESS_ONLY" = "1" ]; then
    INGRESS_SERVICES+=(nginx certbot)
fi

echo -e "${GREEN}=== Curve LP Solver Deployment ===${NC}"
if [ ${#SOLVER_SERVICES[@]} -gt 0 ]; then
    echo "Solver services to rebuild: ${SOLVER_SERVICES[*]}"
else
    echo "Solver services to rebuild: (none)"
fi
if [ ${#INGRESS_SERVICES[@]} -gt 0 ]; then
    echo "Ingress services to rebuild: ${INGRESS_SERVICES[*]}"
else
    echo "Ingress services to rebuild: (none)"
fi
if [ "$SKIP_PROD" = "1" ]; then
    echo -e "${YELLOW}--skip-prod set: ETH mainnet prod (solver) will NOT be rebuilt.${NC}"
fi
echo ""

# ---------- .env loading + validation ----------

if [ ! -f .env ]; then
    echo -e "${RED}ERROR: .env file not found${NC}"
    echo "Copy .env.example to .env and fill in your values:"
    echo "  cp .env.example .env"
    exit 1
fi

set -a
# shellcheck disable=SC1091
source .env
set +a

# Required vars depend on which services we're building.
REQUIRED_VARS=()
if [ "$REBUILD_PROD" = "1" ] || [ "$REBUILD_STAGING" = "1" ]; then
    REQUIRED_VARS+=("NODE_URL")
fi
if [ "$REBUILD_ARBITRUM" = "1" ]; then
    REQUIRED_VARS+=("NODE_URL_ARBITRUM")
fi
if [ "$REBUILD_GNOSIS" = "1" ]; then
    REQUIRED_VARS+=("NODE_URL_GNOSIS")
fi
# Ingress needs DOMAIN and SSL_EMAIL; also needed for the DOMAIN placeholder
# in nginx.template that certbot-init and nginx substitute at startup.
if [ ${#INGRESS_SERVICES[@]} -gt 0 ]; then
    REQUIRED_VARS+=("DOMAIN" "SSL_EMAIL")
fi

MISSING_VARS=()
for var in "${REQUIRED_VARS[@]}"; do
    if [ -z "${!var:-}" ]; then
        MISSING_VARS+=("$var")
    fi
done
if [ ${#MISSING_VARS[@]} -ne 0 ]; then
    echo -e "${RED}ERROR: Missing required environment variables:${NC}"
    for var in "${MISSING_VARS[@]}"; do
        echo "  - $var"
    done
    exit 1
fi

# URL format check for any NODE_URL we're about to use.
for url_var in NODE_URL NODE_URL_ARBITRUM NODE_URL_GNOSIS; do
    val="${!url_var:-}"
    if [ -n "$val" ] && [[ ! "$val" =~ ^https?:// ]]; then
        echo -e "${RED}ERROR: $url_var must be a valid HTTP(S) URL${NC}"
        exit 1
    fi
done

if [ ${#INGRESS_SERVICES[@]} -gt 0 ]; then
    if [[ ! "$DOMAIN" =~ ^[a-zA-Z0-9]([a-zA-Z0-9-]*\.)+[a-zA-Z]{2,}$ ]]; then
        echo -e "${RED}ERROR: DOMAIN must be a valid domain name${NC}"
        exit 1
    fi
    if [[ ! "$SSL_EMAIL" =~ ^[^@]+@[^@]+\.[^@]+$ ]]; then
        echo -e "${RED}ERROR: SSL_EMAIL must be a valid email address${NC}"
        exit 1
    fi
fi

echo -e "${GREEN}✓ Environment variables validated${NC}"
if [ ${#INGRESS_SERVICES[@]} -gt 0 ]; then
    echo "  DOMAIN: $DOMAIN"
    echo "  SSL_EMAIL: $SSL_EMAIL"
fi

# ---------- config substitution ----------

if [ ${#SOLVER_SERVICES[@]} -gt 0 ]; then
    mkdir -p ./processed
    echo "Processing config files..."

    # Scoped envsubst: set NODE_URL to the right per-chain value before each
    # call, and pass the explicit var list so unrelated $vars in the TOML are
    # left alone. Only process the configs whose services are being rebuilt.

    if [ "$REBUILD_PROD" = "1" ]; then
        NODE_URL="$NODE_URL" envsubst '${NODE_URL}' \
            < curve-lp.prod.toml > ./processed/curve-lp.toml
    fi
    if [ "$REBUILD_STAGING" = "1" ]; then
        NODE_URL="$NODE_URL" envsubst '${NODE_URL}' \
            < curve-lp.staging.toml > ./processed/curve-lp-staging.toml
    fi
    if [ "$REBUILD_ARBITRUM" = "1" ]; then
        NODE_URL="$NODE_URL_ARBITRUM" envsubst '${NODE_URL}' \
            < curve-lp.arbitrum.toml > ./processed/curve-lp-arbitrum.toml
    fi
    if [ "$REBUILD_GNOSIS" = "1" ]; then
        NODE_URL="$NODE_URL_GNOSIS" envsubst '${NODE_URL}' \
            < curve-lp.gnosis.toml > ./processed/curve-lp-gnosis.toml
    fi

    echo -e "${GREEN}✓ Config files processed${NC}"

    if grep -l "YOUR_API_KEY" ./processed/*.toml 2>/dev/null; then
        echo -e "${RED}ERROR: Placeholder values found in processed config${NC}"
        exit 1
    fi
    echo -e "${GREEN}✓ No placeholder values in configs${NC}"
fi

# ---------- build + bring up ----------

export DOCKER_BUILDKIT=1
export COMPOSE_DOCKER_CLI_BUILD=1

COMPOSE=(docker compose -f docker-compose.prod.yml)

# --no-deps: we manage the dependency graph explicitly via flags, so tell
# compose not to pull in anything implicit. This is what makes
# `--chains=arbitrum` actually mean "only arbitrum".
if [ ${#SOLVER_SERVICES[@]} -gt 0 ]; then
    echo "Starting solver services: ${SOLVER_SERVICES[*]}"
    "${COMPOSE[@]}" up -d --build --force-recreate --no-deps "${SOLVER_SERVICES[@]}"
fi

if [ ${#INGRESS_SERVICES[@]} -gt 0 ]; then
    # certbot-init has to complete before nginx starts (it seeds a dummy cert
    # if there isn't one). Run it explicitly here rather than via depends_on
    # so the solver-only path doesn't pay that cost.
    echo "Bootstrapping certbot-init..."
    "${COMPOSE[@]}" up -d certbot-init
    echo "Starting ingress services: ${INGRESS_SERVICES[*]}"
    "${COMPOSE[@]}" up -d --build --force-recreate --no-deps "${INGRESS_SERVICES[@]}"
fi

echo ""
echo -e "${GREEN}=== Deployment Complete ===${NC}"
echo ""
if [ ${#SOLVER_SERVICES[@]} -gt 0 ]; then
    echo "Rebuilt solvers: ${SOLVER_SERVICES[*]}"
fi
if [ ${#INGRESS_SERVICES[@]} -gt 0 ]; then
    echo "Rebuilt ingress: ${INGRESS_SERVICES[*]}"
fi
if [ ${#INGRESS_SERVICES[@]} -eq 0 ]; then
    echo -e "${YELLOW}Ingress was not touched; use --with-ingress if you need to pick up${NC}"
    echo -e "${YELLOW}new public routes or nginx.conf changes.${NC}"
fi
echo ""
echo "Check status:"
echo "  docker compose -f docker-compose.prod.yml ps"
echo ""
echo "View logs:"
echo "  docker compose -f docker-compose.prod.yml logs -f"
echo ""
if [ ${#INGRESS_SERVICES[@]} -gt 0 ] && [ -n "${DOMAIN:-}" ]; then
    echo "Health checks (per chain; each returns 200 if that container is up):"
    if [ "$REBUILD_PROD" = "1" ] || [ "$INGRESS_ONLY" = "1" ]; then
        echo "  curl https://$DOMAIN/prod/mainnet/healthz"
    fi
    if [ "$REBUILD_ARBITRUM" = "1" ] || [ "$INGRESS_ONLY" = "1" ]; then
        echo "  curl https://$DOMAIN/prod/arbitrum/healthz"
    fi
    if [ "$REBUILD_GNOSIS" = "1" ] || [ "$INGRESS_ONLY" = "1" ]; then
        echo "  curl https://$DOMAIN/prod/gnosis/healthz"
    fi
    echo "  curl https://$DOMAIN/healthz   # back-compat: probes mainnet prod"
fi
