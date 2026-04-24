# Curve LP Solver — Ops Memo

Workflow notes for running the Curve LP solver against CoW Protocol. Lives on a
single VPS, multi-chain (Ethereum, Arbitrum, Gnosis), decoupled solver/ingress
deploy.

For **first-time-from-scratch** setup (AWS instance creation, DNS, initial
certbot bootstrap, CoW onboarding) see [`COW_README.md`](./COW_README.md). That
flow still works; this file assumes the VPS exists and Docker is installed.

---

## What runs on the VPS

One compose project, six services. Each solver container runs the same binary
with a per-chain TOML.

| Service          | Chain    | Role                 | Public path               | CoW registered |
|------------------|----------|----------------------|---------------------------|----------------|
| `solver`         | Ethereum | Prod (0x9008… settl.) | `/prod/mainnet/`          | **Yes**        |
| `solver-staging` | Ethereum | Shadow/staging (0xf553… settl.) | `/staging/mainnet/`, `/shadow/mainnet/` | Yes            |
| `arbitrum`       | Arbitrum | Prod                 | `/prod/arbitrum/`         | No (pending smoke test) |
| `gnosis`         | Gnosis   | Prod                 | `/prod/gnosis/`           | No (pending smoke test) |
| `nginx`          | —        | Ingress + TLS        | —                         | —              |
| `certbot`        | —        | Cert renewal loop    | —                         | —              |

Public URL pattern follows CoW's convention: `https://$DOMAIN/{env}/{network}/...`.

**Legacy `/healthz`** at the domain root still probes the mainnet `solver`
(back-compat). Per-chain liveness is at `/prod/{chain}/healthz`.

### Chains supported

Chain support lives entirely in config (no Rust changes needed to add a 4th).
To add a new chain you need: a Curve Router deployment, a Curve Price API slug
(`ethereum`/`arbitrum`/`xdai` today), wrapped-native token, and a compose
service + nginx location. See `crates/solvers/src/domain/solver/curve_lp.rs`
for `ChainConfig`.

---

## Day-2 deploy workflow

The whole thing is `./deploy.sh` with flags.

```
./deploy.sh [--skip-prod] [--chains=CSV] [--with-ingress | --ingress-only]
```

Solver and ingress are **decoupled**: `./deploy.sh` with no flags rebuilds
solver containers only. Nginx/certbot are untouched unless you ask. This is
intentional — a chain deploy should never churn ingress, and an ingress
refresh should never churn solvers.

### Flag matrix

| invocation                                       | rebuilds                              |
|--------------------------------------------------|---------------------------------------|
| `./deploy.sh`                                    | all four solvers                       |
| `./deploy.sh --skip-prod`                        | staging + arbitrum + gnosis            |
| `./deploy.sh --chains=arbitrum,gnosis`           | those two only                         |
| `./deploy.sh --chains=mainnet --skip-prod`       | staging only                           |
| `./deploy.sh --chains=arbitrum --with-ingress`   | arbitrum + nginx + certbot             |
| `./deploy.sh --with-ingress`                     | all solvers + nginx + certbot          |
| `./deploy.sh --ingress-only`                     | nginx + certbot, no solvers            |

**When to use `--with-ingress`**: new chain, new public route, or any
`nginx.conf` edit. Otherwise nginx won't know about your new upstream.

**When to use `--ingress-only`**: nginx/cert troubleshooting, TLS settings,
adding headers.

`--ingress-only` is mutually exclusive with `--skip-prod` / `--chains` /
`--with-ingress`; the script errors out if you combine them.

### Typical cutover flows

**Chain code change to arbitrum only** (no routing change):
```
git pull
./deploy.sh --chains=arbitrum
```

**New chain added to code + compose + nginx**:
```
git pull
./deploy.sh --chains=<new-chain> --with-ingress
```

**Nginx-only edit (e.g. new header, path rewrite)**:
```
git pull
./deploy.sh --ingress-only
```

**Emergency: rebuild everything**:
```
./deploy.sh --with-ingress
```

### Nginx resolves backends lazily

`nginx.conf` uses Docker's embedded DNS (`resolver 127.0.0.11`) plus
variable `proxy_pass`. Consequence: **nginx starts even when a solver
container is missing**, and a missing backend 502s that one path until the
backend returns. This is what makes `--chains=arbitrum` safely skip
touching mainnet containers. It also means you can `docker compose stop
arbitrum` without nginx falling over.

The `--no-deps` flag on every `docker compose up` call in `deploy.sh`
prevents compose from implicitly dragging in services.

---

## Configuration

Per-chain TOMLs in `deploy/curve-lp/`:

- `curve-lp.prod.toml`    — ETH mainnet prod
- `curve-lp.staging.toml` — ETH mainnet staging (different settlement contract)
- `curve-lp.arbitrum.toml`
- `curve-lp.gnosis.toml`

These are the source of truth. `deploy.sh` runs them through `envsubst` (only
`${NODE_URL}` is substituted, scoped per-chain) into `./processed/` and mounts
the result into the container.

Compile-time tests (`cargo test -p solvers --lib infra::config::curve_lp`)
parse every deploy TOML at build time, so a malformed or mis-chained config
breaks the build rather than the running container. Add a test when you add
a chain.

### Token filters

Three independent filters can narrow what the solver engages with. All are
optional; any combination can be set. Applied together (AND).

| filter                 | semantics                                                            | use when                                          |
|------------------------|----------------------------------------------------------------------|---------------------------------------------------|
| `lp-tokens`            | either-side — at least one side must be in this list                 | "I'm the LP specialist; other side unconstrained" |
| `allowed-buy-tokens`   | either-side — at least one side must be in this list (misleadingly named; symmetric) | historical crvUSD-style filter                    |
| `token-allowlist`      | **both-sides** — reject if either `sell.token` or `buy.token` is absent | "confine the solver to a known universe"          |

Leaving all three omitted attempts every order and can cause deadline
timeouts. Mainnet uses `lp-tokens`; Arbitrum/Gnosis are set up to use
`token-allowlist` instead.

### Secrets

All secrets live in `deploy/curve-lp/.env` on the VPS. Not a secrets manager,
not committed. `.env.example` is the template. Required vars:

| var                  | when required                           |
|----------------------|-----------------------------------------|
| `NODE_URL`           | rebuilding `solver` or `solver-staging` |
| `NODE_URL_ARBITRUM`  | rebuilding `arbitrum`                   |
| `NODE_URL_GNOSIS`    | rebuilding `gnosis`                     |
| `DOMAIN`             | ingress (nginx/certbot)                 |
| `SSL_EMAIL`          | ingress                                 |
| `TG_BOT_TOKEN`, `TG_CHAT_ID`, `TG_*_THREAD` | telegram monitor (optional) |

Moving to AWS Secrets Manager / SSM is a future TODO; see
`deploy/curve-lp/AWS_SECRETS.md`.

**Quote values containing shell metacharacters.** `deploy.sh` sources `.env`
with `set -a; source .env; set +a`, so unquoted values get interpreted by
bash. If your RPC URL has `?`, `&`, `=`, `#`, or spaces in it — typical for
query-string API keys — wrap the entire value in double quotes:

```
NODE_URL_ARBITRUM="https://rpc.example.com/v1?api_key=abc&chain=arb"   # safe
NODE_URL_ARBITRUM=https://rpc.example.com/v1?api_key=abc&chain=arb     # broken
```

Symptom of forgetting to quote: `./deploy.sh` errors with `Missing required
environment variables: - NODE_URL_ARBITRUM` even though the line is in
`.env`. What happened: `&` forked `chain=arb` into the background, the
value silently truncated at the `?`, and the `-z` check saw it as empty.
When in doubt, quote.

---

## Monitoring

`tg-monitor.sh` tails logs and posts to Telegram. Start it once:

```
nohup ./tg-monitor.sh > tg-monitor.log 2>&1 &
```

What it reports (every 5 min tick):
- Nginx 4xx/5xx over the last 5 min.
- Solver-candidate trade notifications (per-order, with CoW explorer link).
- Hourly summary: auctions, quotes, orders processed, solution candidates, errors.
- Idle heartbeat every 30 min if no activity.

**Caveat**: `tg-monitor.sh` and `monitor.sh` only watch the mainnet
`solver` container. They do not currently watch `arbitrum`, `gnosis`, or
`solver-staging`. To extend, the `docker compose logs` calls need to run
per-container (or use `docker compose logs solver arbitrum gnosis` with
per-line service prefixes).

`monitor.sh` is an interactive console tail with trade extraction to
`trades.log`.

### Monitor UI (`/monitor/`)

Browser-side day-2 sanity dashboard at `https://$DOMAIN/monitor/`, behind
HTTP Basic auth. Shows per-chain health + latency, renders each chain's
`token-allowlist`, and runs preset `/solve` smoke queries. v1 doesn't have
logs, metrics history, or a form-based payload builder — use `tg-monitor`
for logs, `curl` for custom payloads.

**Enable**:
1. Set `MONITOR_USER` and `MONITOR_PASSWORD` in `.env` (both, or neither).
2. `./deploy.sh --ingress-only` — regenerates the htpasswd file and
   sanitized config JSONs, restarts nginx. Solver containers untouched.
3. Navigate to `https://$DOMAIN/monitor/` and log in.

**Rotate the password**: edit `.env`, rerun `./deploy.sh --ingress-only`.

**Disable**: remove both env vars, rerun `./deploy.sh --ingress-only`. The
htpasswd file becomes empty and nginx rejects every login.

**Freshness**: the `/monitor/config/*.json` files are regenerated on any
deploy that rebuilds the corresponding solver — so `./deploy.sh
--chains=arbitrum` updates `arbitrum.json` too, even though it doesn't
touch ingress. This keeps the UI honest after a solver-only config edit.
The trigger is "monitor was set up at some point" (non-empty
`./processed/htpasswd`); if you've never enabled the monitor, solver-only
deploys skip the regen to stay fast.

**Security notes**:
- The `/monitor/config/*.json` files are NOT the processed TOMLs. They're
  a positive-allowlist subset (six fields: chain_id, router_address,
  wrapped_native_token, settlement_contract, price_api_chain,
  token_allowlist). `node-url` — which contains the RPC API key after
  envsubst — is never included.
- Preset payloads POST to `/prod/{chain}/solve`, which is already public
  (CoW drivers hit it). Basic auth leaking doesn't escalate blast radius.
- If you add a new field to the deploy TOMLs that the UI should show,
  update both the generator in `deploy.sh` (`emit_monitor_json`) and the
  UI in `nginx/monitor/index.html`. No auto-sync.
- Preset payloads are hard-coded JSON blobs in the HTML. If CoW's `/solve`
  request schema changes, update the blobs by hand.

---

## Troubleshooting

### `./deploy.sh` errors with "Missing required environment variables" for a var that IS in `.env`

Almost always an unquoted value containing `?`, `&`, `=`, `#`, or a space
— classic for RPC URLs with query-string keys. Bash interprets those when
`source`-ing `.env`, truncating the value. Wrap the whole value in double
quotes: `NODE_URL_FOO="https://..."`. See Secrets section above.

Quick check on the VPS:
```
cd ~/cow-solver-services/deploy/curve-lp
set -a; source .env; set +a
echo "[$NODE_URL_ARBITRUM]"   # empty brackets => quoting bug
```

Other (rarer) causes: CRLF line endings if the file was edited on Windows
(fix: `sed -i 's/\r$//' .env`); UTF-8 BOM (`file .env` reports "UTF-8
Unicode (with BOM)"; fix: re-save without BOM).

### `./deploy.sh --chains=arbitrum` fails with mount errors on clean host
Something else is also trying to start. Check `docker compose ps` — any
service with a `Created`/`Restarting` status and a bind-mount to a file in
`./processed/` that doesn't exist will crash. If deploy.sh is behaving
correctly it uses `--no-deps`, so this shouldn't happen; if it does,
confirm you're on the current `deploy.sh`.

### Nginx won't start
With lazy DNS this should almost never happen for a missing backend — only
for syntax errors or missing certs. Check:
```
docker compose -f docker-compose.prod.yml logs nginx
```

### A `/prod/{chain}/...` path returns 502
The backend container isn't running or doesn't resolve. Check:
```
docker compose -f docker-compose.prod.yml ps {chain}
docker compose -f docker-compose.prod.yml logs {chain} --tail 100
```

### Cert renewal failing
Cert lives in the `certbot-etc` volume, renewed in a loop by the `certbot`
service. Logs:
```
docker compose -f docker-compose.prod.yml logs certbot
```
Force a refresh by recreating the ingress: `./deploy.sh --ingress-only`.

### CoW driver sends traffic that all 404s or times out
Confirm CoW has the right URL(s) registered. For mainnet today that's
`https://$DOMAIN/prod/mainnet/` and `/staging/mainnet/`. Arbitrum and
Gnosis are **not yet registered** — smoke test first, then onboard via CoW
discord.

### Swap / memory pressure on a t3.medium
Three parallel Rust rebuilds can OOM on 4 GB. Up the swap file or deploy
serially with `--chains` one at a time.

---

## Pointers

- Compose file: `docker-compose.prod.yml`
- Nginx template: `nginx/nginx.conf` (the literal `$DOMAIN` is substituted
  by the nginx container's entrypoint at startup).
- Rust solver code: `crates/solvers/src/domain/solver/curve_lp.rs`
- Config loader: `crates/solvers/src/infra/config/curve_lp.rs`
- Chain-abstract type: `ChainConfig` in the solver crate.
