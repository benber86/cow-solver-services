#!/bin/bash
set -euo pipefail

# Curve LP Solver - Deployment Script
#
# This script:
# 1. Validates environment variables are set
# 2. Substitutes variables into config files
# 3. Starts the services

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m' # No Color

echo -e "${GREEN}=== Curve LP Solver Deployment ===${NC}"

# Check for .env file
if [ ! -f .env ]; then
    echo -e "${RED}ERROR: .env file not found${NC}"
    echo "Copy .env.example to .env and fill in your values:"
    echo "  cp .env.example .env"
    exit 1
fi

# Load environment variables
set -a
source .env
set +a

# Validate required variables
REQUIRED_VARS=("NODE_URL" "SOLVER_ACCOUNT" "DOMAIN" "SSL_EMAIL")
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

# Validate NODE_URL looks like a URL
if [[ ! "$NODE_URL" =~ ^https?:// ]]; then
    echo -e "${RED}ERROR: NODE_URL must be a valid HTTP(S) URL${NC}"
    exit 1
fi

# Validate SOLVER_ACCOUNT looks like a private key (64 hex chars after 0x)
if [[ ! "$SOLVER_ACCOUNT" =~ ^0x[a-fA-F0-9]{64}$ ]]; then
    echo -e "${RED}ERROR: SOLVER_ACCOUNT must be a valid private key (0x + 64 hex chars)${NC}"
    exit 1
fi

# Check if it's the placeholder value
if [ "$SOLVER_ACCOUNT" = "0x0000000000000000000000000000000000000000000000000000000000000000" ]; then
    echo -e "${RED}ERROR: SOLVER_ACCOUNT is still the placeholder value${NC}"
    echo "Please set your actual private key in .env"
    exit 1
fi

# Validate DOMAIN looks like a domain
if [[ ! "$DOMAIN" =~ ^[a-zA-Z0-9]([a-zA-Z0-9-]*\.)+[a-zA-Z]{2,}$ ]]; then
    echo -e "${RED}ERROR: DOMAIN must be a valid domain name${NC}"
    exit 1
fi

# Validate SSL_EMAIL looks like an email
if [[ ! "$SSL_EMAIL" =~ ^[^@]+@[^@]+\.[^@]+$ ]]; then
    echo -e "${RED}ERROR: SSL_EMAIL must be a valid email address${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Environment variables validated${NC}"
echo "  DOMAIN: $DOMAIN"
echo "  SSL_EMAIL: $SSL_EMAIL"

# Create processed config directory
mkdir -p ./processed

# Substitute environment variables in config files
echo "Processing config files..."

envsubst < driver.toml > ./processed/driver.toml
envsubst < curve-lp.prod.toml > ./processed/curve-lp.toml

echo -e "${GREEN}✓ Config files processed${NC}"

# Verify no secrets leaked into processed files (sanity check)
if grep -q "YOUR_API_KEY" ./processed/driver.toml ./processed/curve-lp.toml 2>/dev/null; then
    echo -e "${RED}ERROR: Placeholder values found in processed config${NC}"
    exit 1
fi

echo -e "${GREEN}✓ No placeholder values in configs${NC}"

# Start services
echo "Starting services..."

docker-compose -f docker-compose.prod.yml up -d --build

echo ""
echo -e "${GREEN}=== Deployment Complete ===${NC}"
echo ""
echo "Services started. Check status with:"
echo "  docker-compose -f docker-compose.prod.yml ps"
echo ""
echo "View logs with:"
echo "  docker-compose -f docker-compose.prod.yml logs -f"
echo ""
echo "Health checks:"
echo "  curl https://$DOMAIN/healthz"
echo ""
