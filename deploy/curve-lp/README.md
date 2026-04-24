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

---

## Troubleshooting

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
