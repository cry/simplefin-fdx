# simplefin-server

A Rust server that polls [SimpleFIN](https://www.simplefin.org/) and/or [LunchFlow](https://lunchflow.app) for account and transaction data, stores it in SQLite, and serves it via a [FDX v6](https://financialdataexchange.org/) compatible REST API.

When both sources are configured the server reconciles their data automatically — matching accounts and transactions across providers and surfacing the reconciliation status in API responses.

Designed to run behind a reverse proxy (nginx, Caddy, Authelia, etc.) — no built-in authentication.

## Operating modes

The server detects which providers are configured at startup and operates accordingly:

| Mode | Required env vars | Account ID prefix |
|---|---|---|
| SimpleFIN only | `SIMPLEFIN_SETUP_TOKEN` (first run only) | `SIMPLEFIN-` |
| LunchFlow only | `LUNCHFLOW_API_KEY` | `LUNCHFLOW-` |
| Both (with reconciliation) | Both of the above | `SIMPLEFIN-`, `LUNCHFLOW-`, or `REC-` |

In **both** mode, accounts and transactions matched across providers are promoted to `REC-` IDs and the transaction endpoint includes reconciliation metadata.

## Getting started

### SimpleFIN

1. Log in to your SimpleFIN bridge and generate a setup token (a base64-encoded URL like `aHR0cHM6...`).
2. Set `SIMPLEFIN_SETUP_TOKEN` on first run. The server claims a persistent access URL and saves it to the database — the token is not needed again after that.

### LunchFlow

1. Obtain a LunchFlow API key from your LunchFlow account.
2. Set `LUNCHFLOW_API_KEY`.

### Configure and run

```bash
# SimpleFIN only
export SIMPLEFIN_SETUP_TOKEN=<your token>

# LunchFlow only
export LUNCHFLOW_API_KEY=<your key>

# Both (enables reconciliation)
export SIMPLEFIN_SETUP_TOKEN=<your token>
export LUNCHFLOW_API_KEY=<your key>

# Common options (all have defaults)
export DATABASE_URL=sqlite://simplefin.db
export SERVER_ADDR=127.0.0.1:8080
export FETCH_INTERVAL_SECS=3600
export START_DATE_DAYS_BACK=90

cargo run --release
```

On first run (SimpleFIN) the server claims a persistent access URL from the setup token and stores it in the database. Subsequent runs use the stored URL — `SIMPLEFIN_SETUP_TOKEN` is no longer needed.

Fetch state is persisted so restarting the server will not trigger an immediate re-fetch if one has occurred recently.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `SIMPLEFIN_SETUP_TOKEN` | — | One-time setup token from the SimpleFIN bridge. Only needed on first run; the claimed access URL is stored in the database. |
| `LUNCHFLOW_API_KEY` | — | LunchFlow API key. If absent the LunchFlow fetcher is disabled. |
| `FETCH_INTERVAL_SECS` | `3600` | Seconds between polls. Applies to both fetchers. |
| `SERVER_ADDR` | `0.0.0.0:8080` | TCP address the HTTP server binds to. |
| `DATABASE_URL` | `sqlite://simplefin.db` | SQLite connection string. The file is created automatically. |
| `START_DATE_DAYS_BACK` | `90` | Days of history to fetch on the very first run (both sources). |

## Docker

### Build

```bash
docker build -t simplefin-server .
```

### Run

Mount a host directory so the database persists across container restarts:

```bash
mkdir -p /path/to/data

# SimpleFIN only
docker run -d \
  --name simplefin-server \
  -p 8080:8080 \
  -v /path/to/data:/data \
  -e DATABASE_URL=sqlite:///data/simplefin.db \
  -e SIMPLEFIN_SETUP_TOKEN=<your token> \
  simplefin-server

# LunchFlow only
docker run -d \
  --name simplefin-server \
  -p 8080:8080 \
  -v /path/to/data:/data \
  -e DATABASE_URL=sqlite:///data/simplefin.db \
  -e LUNCHFLOW_API_KEY=<your key> \
  simplefin-server
```

After the first SimpleFIN run the access URL is stored in the database. You can drop `SIMPLEFIN_SETUP_TOKEN` from subsequent runs.

> **Note:** Use an absolute path for `DATABASE_URL` inside the container (`sqlite:///data/...` with three slashes). A relative path like `sqlite://simplefin.db` would write the database inside the container and lose it on restart.

### Docker Compose

```yaml
services:
  simplefin-server:
    build: .
    ports:
      - "8080:8080"
    volumes:
      - ./data:/data
    environment:
      DATABASE_URL: sqlite:///data/simplefin.db
      SIMPLEFIN_SETUP_TOKEN: <your token>   # remove after first run
      LUNCHFLOW_API_KEY: <your key>         # optional
      FETCH_INTERVAL_SECS: 3600
    restart: unless-stopped
```

## Web UI

A browser-based UI is served at `GET /`. It shows all accounts in a sidebar and lets you browse transactions (with a date range filter) and holdings for each account.

## API

All responses are `application/json`. The full OpenAPI spec is available at `GET /openapi.json` and can be loaded into any compatible viewer (Swagger UI, Redoc, Stoplight, Postman).

### Account ID prefixes

Every account ID in the API is prefixed to identify its source:

| Prefix | Meaning |
|---|---|
| `SIMPLEFIN-{id}` | SimpleFIN account with no matching LunchFlow account |
| `LUNCHFLOW-{id}` | LunchFlow account with no matching SimpleFIN account |
| `REC-{sfin_id}` | Pair matched by reconciliation; transactions include data from both sources |

### `GET /`

Returns the web UI (HTML).

### `GET /health`

```json
{
  "status": "ok",
  "lastFetched": "2024-01-15T10:00:00+00:00",
  "lfLastFetched": "2024-01-15T10:05:00+00:00"
}
```

`status` is `"degraded"` if any active fetcher's last cycle failed; stale data is still served. `lastFetched` / `lfLastFetched` are omitted when the corresponding provider is not configured. `fetchError` / `lfFetchError` appear only when there is an active error.

### `GET /fdx/v6/accounts`

```json
{
  "accounts": [
    {
      "accountId": "REC-abc123",
      "accountType": "OTHER",
      "displayName": "My Checking",
      "currency": { "currencyCode": "USD" },
      "currentBalance": 1234.56,
      "availableBalance": 1200.00,
      "balanceDate": "2024-01-15T00:00:00+00:00"
    },
    {
      "accountId": "LUNCHFLOW-42",
      "accountType": "OTHER",
      "displayName": "Savings",
      "currency": { "currencyCode": "USD" },
      "currentBalance": 5000.00,
      "balanceDate": "2024-01-15T00:00:00+00:00"
    }
  ],
  "page": { "total": 2 }
}
```

### `GET /fdx/v6/accounts/{accountId}`

Returns a single account by its prefixed ID. `404` if not found, `503` if no data has been fetched yet.

### `GET /fdx/v6/accounts/{accountId}/transactions`

Optional query parameters:

| Parameter | Format | Description |
|---|---|---|
| `startTime` | RFC 3339 | Include transactions posted on or after this time |
| `endTime` | RFC 3339 | Include transactions posted on or before this time |

**For `SIMPLEFIN-` and `LUNCHFLOW-` accounts** the response is standard FDX:

```json
{
  "transactions": [
    {
      "transactionId": "txn_001",
      "postedTimestamp": "2024-01-10T12:00:00+00:00",
      "amount": -42.00,
      "description": "Coffee shop",
      "payee": "Blue Bottle Coffee",
      "status": "POSTED",
      "debitCreditMemo": "DEBIT"
    }
  ],
  "page": { "total": 1 }
}
```

**For `REC-` accounts** each transaction also includes reconciliation metadata. When both sources have the transaction, a nested `lunchflowTransaction` object is included:

```json
{
  "transactions": [
    {
      "transactionId": "sfin_txn_001",
      "postedTimestamp": "2024-01-10T12:00:00+00:00",
      "amount": -42.00,
      "description": "Coffee shop",
      "status": "POSTED",
      "debitCreditMemo": "DEBIT",
      "reconciliationStatus": "matched",
      "matchConfidence": 0.9,
      "lunchflowTransaction": {
        "id": "lf_txn_xyz",
        "amount": 42.00,
        "currency": "USD",
        "date": "2024-01-10",
        "merchant": "Blue Bottle Coffee",
        "isPending": false
      }
    },
    {
      "transactionId": "sfin_txn_002",
      "postedTimestamp": "2024-01-09T08:00:00+00:00",
      "amount": -15.00,
      "description": "Parking",
      "status": "POSTED",
      "debitCreditMemo": "DEBIT",
      "reconciliationStatus": "sfin_only"
    },
    {
      "transactionId": "lf_txn_abc",
      "postedTimestamp": "2024-01-08T00:00:00+00:00",
      "amount": -8.50,
      "description": "Bakery",
      "status": "POSTED",
      "debitCreditMemo": "DEBIT",
      "reconciliationStatus": "lf_only"
    }
  ],
  "page": { "total": 3 }
}
```

**Reconciliation status values:**

| Value | Meaning |
|---|---|
| `"matched"` | Transaction found in both SimpleFIN and LunchFlow |
| `"sfin_only"` | Only in SimpleFIN (reconciliation ran, no LunchFlow match) |
| `"lf_only"` | Only in LunchFlow (reconciliation ran, no SimpleFIN match) |
| `"unreconciled"` | Reconciliation has not yet run for this transaction |

### `GET /fdx/v6/accounts/{accountId}/holdings`

Returns investment holdings. For `SIMPLEFIN-` and `REC-` accounts, data comes from SimpleFIN. For `LUNCHFLOW-` accounts, returns an empty list.

```json
{
  "holdings": [
    {
      "holdingId": "h_001",
      "currency": { "currencyCode": "USD" },
      "positionDate": "2024-01-15T00:00:00+00:00",
      "symbol": "AAPL",
      "description": "Apple Inc.",
      "marketValue": 15234.56,
      "costBasis": 12000.00,
      "purchasePrice": 120.00,
      "units": 100.0
    }
  ],
  "page": { "total": 1 }
}
```

### `GET /openapi.json`

Returns the OpenAPI 3.x spec for this server. Load it into any OpenAPI viewer:

```bash
# Serve locally with Redoc (requires Node/npx)
npx @redocly/cli preview-docs http://localhost:8080/openapi.json

# Or open in Swagger Editor
open https://editor.swagger.io/?url=http://localhost:8080/openapi.json
```

## Fetch behaviour

### SimpleFIN

- **First run**: fetches `START_DATE_DAYS_BACK` days of history (default 90), split into ≤ 90-day batches.
- **Subsequent fetches**: fetches from 24 hours before the last successful poll to catch late-arriving transactions.
- **Restart**: waits out any remaining interval before polling again; polls immediately if the interval has elapsed.
- **Failed fetch**: retries the same window on the next cycle. Stale data continues to be served.

### LunchFlow

- **First run**: fetches `START_DATE_DAYS_BACK` days of history.
- **Subsequent fetches**: fetches from 24 hours before the last successful poll.
- **Holdings**: fetched and replaced per account on each cycle. Accounts that don't support holdings (non-investment accounts) are skipped silently.
- **Reconciliation**: runs automatically after each successful LunchFlow fetch cycle.

## Reconciliation

When both SimpleFIN and LunchFlow are configured, the server reconciles accounts and transactions after each LunchFlow fetch:

1. **Account matching** — pairs accounts from both sources by name similarity and currency. High-confidence matches become `REC-` accounts in the API.
2. **Transaction matching** — for each matched account pair, links transactions with the same amount (within $0.01) and date (within ±2 days). Confidence is boosted further when merchant/description names overlap.

Unmatched transactions from either source are recorded as `sfin_only` or `lf_only` and appear in the `REC-` account's transaction list alongside matched ones.

## Data notes

- `accountType` is always `"OTHER"` — neither provider exposes a standardised account type.
- `status` is `"PENDING"` when a transaction is pending; otherwise `"POSTED"`.
- `payee` and `memo` are omitted from transaction responses when not provided by the source.
- SimpleFIN amounts are stored as strings in SQLite to preserve decimal precision; LunchFlow amounts are stored as `REAL`.
- Transactions and holdings are upserted by ID, so overlapping fetch windows never create duplicates.

## Reverse proxy example (Caddy)

```
simplefin.example.com {
    basicauth {
        user <bcrypt-hash>
    }
    reverse_proxy 127.0.0.1:8080
}
```
