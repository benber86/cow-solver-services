//! `RouteProvider` impl backed by *.router.curve.finance/quote.

use {
    super::route_provider::{
        Error,
        ExecutableQuote,
        PreCall,
        QuoteQuality,
        QuoteRequest,
        ReportBank,
        ReportPricing,
        RouteProvider,
    },
    crate::domain::eth,
    alloy::{sol, sol_types::SolCall},
    async_trait::async_trait,
    reqwest::Url,
    serde::{Deserialize, Serialize},
    std::{
        collections::HashMap,
        sync::Mutex,
        time::{Duration, Instant},
    },
};

sol! {
    function deliver_report(bytes update) external payable;
}

/// The bank refuses anything longer; a longer `update` is not a report.
const MAX_REPORT_UPDATE_BYTES: usize = 301;

/// Retries of the binding round trip before we give up on the order. The
/// probe already cost a round trip and the whole `quote` call shares one
/// upstream budget (`route_budget`), so this is deliberately shallow.
const BIND_RETRY_ATTEMPTS: u32 = 1;

/// Cap on cached quotes so a large order book cannot grow the map without
/// bound — the TTL alone never evicts on a chain that goes quiet.
const MAX_QUOTE_CACHE_ENTRIES: usize = 4096;

/// Identity of a quote for caching purposes.
///
/// `min_out` is part of the key even though it looks derived. `bind_floor`
/// clamps the slippage haircut *up* to the order's limit, so the limit reaches
/// the calldata the service builds — two orders on the same pair and size but
/// with different limits genuinely get different quotes. Sharing one between
/// them would hand the higher-limit order a floor below what it needs, which
/// `solve_order` then rejects as `InsufficientOutput` — and that rejection
/// carries a five-minute backoff, so a solvable order would be dropped for
/// ~150 auctions on a 2s-block chain.
///
/// This costs no hit rate for the case the cache exists for: an order's limit
/// is fixed for its lifetime, so re-quoting the same order across consecutive
/// auctions still hits.
///
/// `receiver` stays out of the key — it is always this chain's settlement
/// contract.
type QuoteKey = (eth::Address, eth::Address, eth::U256, Option<eth::U256>);

struct CachedQuote {
    quote: ExecutableQuote,
    stored_at: Instant,
}

pub struct NewRouterClient {
    http: reqwest::Client,
    quote_url: Url,
    router_address: eth::Address,
    slippage_bps: u32,
    /// How long a route may be reused across auctions. Zero disables caching.
    cache_ttl: Duration,
    cache: Mutex<HashMap<QuoteKey, CachedQuote>>,
    /// Without one, no report route is ever asked for or accepted.
    report_bank: Option<ReportBank>,
}

// Failures are classified where we actually know the cause: transport-level
// problems become `Error::Transport` (retryable, no backoff), everything the
// service actually answered becomes `Error::CalldataUnavailable` (a verdict —
// no route, `min_out` unreachable — so retrying only burns the deadline).

impl NewRouterClient {
    pub fn new(
        quote_url: Url,
        router_address: eth::Address,
        slippage_bps: u32,
        cache_ttl: Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            quote_url,
            router_address,
            slippage_bps,
            cache_ttl,
            cache: Mutex::new(HashMap::new()),
            report_bank: None,
        }
    }

    pub fn with_report_bank(mut self, report_bank: Option<ReportBank>) -> Self {
        self.report_bank = report_bank;
        self
    }

    /// A pre-call runs from the settlement contract, with its funds and its
    /// approvals, and the simulated buy amount says nothing about what else
    /// such a call does. So the service only gets to choose the report bytes:
    /// the target, the function and the value ceiling are fixed here.
    fn check_pre_calls(
        quote: &ExecutableQuote,
        req: &QuoteRequest,
        report_bank: Option<&ReportBank>,
    ) -> Result<(), Error> {
        let reject = |why: &str| Err(Error::CalldataUnavailable(format!("report route: {why}")));
        if quote.pre_calls.is_empty() {
            return Ok(());
        }
        if !req.report_aware || req.is_quote {
            return reject("returned for a request that did not ask for one");
        }
        let Some(bank) = report_bank else {
            return reject("no report bank is configured");
        };
        let [call] = quote.pre_calls.as_slice() else {
            return reject("expected exactly one bank call");
        };
        if call.target != bank.address {
            return reject("bank call targets a contract other than the configured bank");
        }
        if call.value > bank.max_fee {
            return reject("bank call carries more than the configured fee ceiling");
        }
        let Ok(decoded) = deliver_reportCall::abi_decode(&call.calldata) else {
            return reject("bank call is not deliver_report(bytes)");
        };
        if decoded.update.len() > MAX_REPORT_UPDATE_BYTES
            || decoded.abi_encode() != call.calldata
        {
            return reject("bank call is not a canonical deliver_report(bytes)");
        }
        Ok(())
    }

    fn cache_key(req: &QuoteRequest) -> QuoteKey {
        (req.sell_token, req.buy_token, req.sell_amount, req.min_out)
    }

    fn cached_quote(&self, req: &QuoteRequest) -> Option<ExecutableQuote> {
        if self.cache_ttl.is_zero() {
            return None;
        }
        let cache = self.cache.lock().ok()?;
        let entry = cache.get(&Self::cache_key(req))?;
        (entry.stored_at.elapsed() <= self.cache_ttl).then(|| entry.quote.clone())
    }

    fn store_quote(&self, req: &QuoteRequest, quote: &ExecutableQuote) {
        // A report quote is only good for the slot it was priced at: the
        // report ages, and the bank stops accepting it after a minute.
        if self.cache_ttl.is_zero() || !quote.pre_calls.is_empty() {
            return;
        }
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        if cache.len() >= MAX_QUOTE_CACHE_ENTRIES {
            let ttl = self.cache_ttl;
            cache.retain(|_, entry| entry.stored_at.elapsed() <= ttl);
        }
        cache.insert(
            Self::cache_key(req),
            CachedQuote {
                quote: quote.clone(),
                stored_at: Instant::now(),
            },
        );
    }

    fn build_request_body(req: &QuoteRequest) -> RequestBody {
        RequestBody {
            input_token: format!("{:#x}", req.sell_token),
            output_token: format!("{:#x}", req.buy_token),
            amount_in: req.sell_amount.to_string(),
            exact: !req.is_quote,
            receiver: (!req.is_quote).then(|| format!("{:#x}", req.receiver)),
            min_out: req.min_out.map(|v| v.to_string()),
            gas_price_gwei: req.gas_price_gwei,
            // A quote is answered long before any settlement, so a report
            // priced into it would have aged out by the time it mattered.
            report_aware: req.report_aware && !req.is_quote,
        }
    }

    /// Parses a response and verifies router/final_token. The returned
    /// `min_out` is taken from `req.min_out` when present (the value the
    /// server bakes into calldata); for quote-only requests with no
    /// `min_out` we fall back to `expected_output`.
    ///
    /// Critical invariant maintained: `ExecutableQuote.min_out` equals the
    /// calldata-encoded floor. The caller (`quote`) makes a second round
    /// trip on real solves to pull these two values into alignment.
    fn parse_response(
        body: ResponseBody,
        req: &QuoteRequest,
        expected_router: eth::Address,
    ) -> Result<ExecutableQuote, Error> {
        if let Some(err) = body.error {
            return Err(Error::CalldataUnavailable(err));
        }

        let returned_router: eth::Address = body.router_address.parse().map_err(|_| {
            Error::CalldataUnavailable(format!("invalid router_address: {}", body.router_address))
        })?;
        if returned_router != expected_router {
            return Err(Error::RouterAddressMismatch {
                expected: expected_router,
                got: returned_router,
            });
        }

        let final_token: eth::Address = body.final_token.parse().map_err(|_| {
            Error::CalldataUnavailable(format!("invalid final_token: {}", body.final_token))
        })?;
        if final_token != req.buy_token {
            return Err(Error::FinalTokenMismatch {
                expected: req.buy_token,
                got: final_token,
            });
        }

        let expected_output: eth::U256 = body.expected_out.parse().map_err(|_| {
            Error::CalldataUnavailable(format!("invalid expected_out: {}", body.expected_out))
        })?;

        let calldata = if req.is_quote {
            Vec::new()
        } else {
            let hex = body.calldata.ok_or_else(|| {
                Error::CalldataUnavailable("missing calldata on real solve".into())
            })?;
            let stripped = hex.strip_prefix("0x").unwrap_or(&hex);
            alloy::hex::decode(stripped)
                .map_err(|e| Error::CalldataUnavailable(format!("invalid hex calldata: {e}")))?
        };

        // The calldata-encoded floor:
        //   - if the caller sent `min_out`, server bakes that value in;
        //   - else (quote probe path) server falls back to `expected_output`
        //     and we record the same here.
        let min_out = req.min_out.unwrap_or(expected_output);

        let (pre_calls, report) = match body.report {
            Some(report) => Self::parse_report(report, req.is_quote)?,
            None => (Vec::new(), None),
        };

        Ok(ExecutableQuote {
            expected_output,
            min_out,
            router_address: returned_router,
            calldata,
            gas_estimate: Some(body.gas_estimate),
            quality: body.quote_quality.map(Into::into),
            pre_calls,
            report,
        })
    }

    /// The bank calls come from the same response as the swap calldata they
    /// precede: the service priced that calldata on top of exactly these calls.
    fn parse_report(
        report: ReportWire,
        is_quote: bool,
    ) -> Result<(Vec<PreCall>, Option<ReportPricing>), Error> {
        let selected = report.selected == ReportSelectionWire::Report;
        let amount = |raw: Option<String>| raw.and_then(|v| v.parse::<eth::U256>().ok());
        let pre_calls = if selected && !is_quote {
            report
                .bank_calls
                .into_iter()
                .map(|call| {
                    let invalid = |what: &str| {
                        Error::CalldataUnavailable(format!("invalid bank call {what}"))
                    };
                    Ok(PreCall {
                        target: call.to.parse().map_err(|_| invalid("target"))?,
                        value: call.value.parse().map_err(|_| invalid("value"))?,
                        calldata: alloy::hex::decode(
                            call.data.strip_prefix("0x").unwrap_or(&call.data),
                        )
                        .map_err(|_| invalid("data"))?,
                    })
                })
                .collect::<Result<Vec<_>, Error>>()?
        } else {
            Vec::new()
        };
        if selected && !is_quote && pre_calls.is_empty() {
            return Err(Error::CalldataUnavailable(
                "report route selected without a bank call".into(),
            ));
        }
        Ok((
            pre_calls,
            Some(ReportPricing {
                selected,
                observation_time_us: report.observation_time_us,
                inclusion_timestamp: report.inclusion_timestamp,
                age_at_inclusion_secs: report.age_at_inclusion_secs,
                ordinary_out: amount(report.shadow.ordinary_out),
                next_slot_out: amount(report.shadow.next_slot_out),
                one_slot_late_out: amount(report.shadow.one_slot_late_out),
                one_slot_late_failure: report.shadow.one_slot_late_failure,
            }),
        ))
    }

    /// The bind request's floor sits just under the report route's output, so
    /// the ordinary route usually cannot meet it and comes back unpriced. The
    /// probe did price it, and that figure is what the report's gain is
    /// measured against.
    fn keep_probe_ordinary_out(bound: &mut ExecutableQuote, probe: &ExecutableQuote) {
        if let (Some(bound), Some(probe)) = (bound.report.as_mut(), probe.report.as_ref())
            && bound.ordinary_out.is_none()
        {
            bound.ordinary_out = probe.ordinary_out;
        }
    }

    /// Whether an unsuccessful HTTP status is worth retrying.
    fn status_is_transient(status: reqwest::StatusCode) -> bool {
        status.is_server_error()
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
    }

    /// The floor to bind into the calldata: the slippage haircut on the
    /// probe's expected output, never below the order's own limit.
    fn bind_floor(
        expected_output: eth::U256,
        slippage_bps: u32,
        order_floor: Option<eth::U256>,
    ) -> eth::U256 {
        let floor = QuoteRequest::min_out_with_slippage(expected_output, slippage_bps);
        match order_floor {
            Some(order_floor) if floor < order_floor => order_floor,
            _ => floor,
        }
    }

    async fn post_quote(&self, req: &QuoteRequest) -> Result<ExecutableQuote, Error> {
        let body = Self::build_request_body(req);
        let resp = self
            .http
            .post(self.quote_url.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Transport(format!("network: {e}")))?;

        let status = resp.status();
        let raw = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(format!("read body: {e}")))?;

        if !status.is_success() {
            let txt = String::from_utf8_lossy(&raw).to_string();
            let msg = format!("HTTP {status}: {txt}");
            return Err(if Self::status_is_transient(status) {
                Error::Transport(msg)
            } else {
                Error::CalldataUnavailable(msg)
            });
        }

        let parsed: ResponseBody = serde_json::from_slice(&raw).map_err(|e| {
            Error::CalldataUnavailable(format!(
                "parse error: {e}; body: {}",
                String::from_utf8_lossy(&raw)
            ))
        })?;

        let quote = Self::parse_response(parsed, req, self.router_address)?;
        Self::check_pre_calls(&quote, req, self.report_bank.as_ref())?;
        Ok(quote)
    }
}

#[async_trait]
impl RouteProvider for NewRouterClient {
    async fn quote(&self, req: &QuoteRequest) -> Result<ExecutableQuote, Error> {
        // Quote probes: single call, no calldata needed. Never served from
        // cache — these answer CoW's /quote requests and the native-price
        // probe's amount search, both of which want a fresh price.
        //
        // There is no bind trip here, yet the floor we report is still what
        // the bid gets clamped to. CoW's /quote carries a 1 wei buy amount
        // as a placeholder, so echoing `req.min_out` as the floor collapsed
        // every sell-side quote to a 1 wei bid. Report the floor a real
        // solve would bind instead.
        if req.is_quote {
            let mut quote = self.post_quote(req).await?;
            quote.min_out = Self::bind_floor(quote.expected_output, self.slippage_bps, req.min_out);
            return Ok(quote);
        }

        // The same order is re-quoted every auction, and on a 2s-block chain
        // that is two round trips per order every two seconds against a
        // service we also depend on for latency. A hit here costs the freshness
        // of `expected_output`; because the bid is clamped to the calldata
        // floor baked into this same quote, a stale one cannot make us promise
        // more than the calldata guarantees — it can only make the settlement
        // fail to simulate, which costs the auction rather than the buffers.
        if let Some(quote) = self.cached_quote(req) {
            tracing::debug!(
                sell_token = ?req.sell_token,
                buy_token = ?req.buy_token,
                "serving route from cache"
            );
            return Ok(quote);
        }

        // Real solves need ONE floor that's both the bid and the calldata
        // revert threshold. The server bakes the request's `min_out` into
        // the returned calldata verbatim, so we can't know what floor to
        // bid until we've seen `expected_output`. Two trips:
        //
        // 1. Probe with `min_out = req.min_out` (the order's hard floor)
        //    to fetch `expected_output` cheaply.
        // 2. Bind with `min_out = slippage(expected_output)` clamped to
        //    `req.min_out` so we never bid below the order floor. Server
        //    re-encodes calldata with that exact value.
        //
        // If step 2 fails we retry it once and then drop the order.
        //
        // We must NOT fall back to a quote bound at the order's limit price.
        // The bid is clamped to the calldata floor (`sell_order_bid_output`),
        // so a limit-priced floor becomes a limit-priced bid, and an order
        // whose limit is nominal rather than economic then gets bid at that
        // nominal value. Curve's fee collector posts every order with a flat
        // 5 crvUSD minimum, so the fallback bid one of those at 5 crvUSD and
        // lost the auction for certain; before the bid was clamped the same
        // fallback instead promised a real price against a 5 crvUSD floor,
        // i.e. an unbounded overdraft. Neither is acceptable — the only safe
        // outcome when we cannot bind a real floor is to not bid at all.
        match self.probe_and_bind(req).await {
            // The floor came from a report-priced probe, and by the time the
            // route was bound the report no longer carried it. That says
            // nothing about the ordinary route, which may still clear the
            // order's own limit.
            Err(BindError {
                error,
                report_priced: true,
            }) if !error.is_transient() => {
                tracing::debug!(%error, "report route could not be bound; trying the ordinary route");
                let ordinary = QuoteRequest {
                    report_aware: false,
                    ..req.clone()
                };
                self.probe_and_bind(&ordinary)
                    .await
                    .map_err(|bind| bind.error)
            }
            result => result.map_err(|bind| bind.error),
        }
    }
}

struct BindError {
    error: Error,
    /// The floor being bound came from a probe that used a report route.
    report_priced: bool,
}

impl NewRouterClient {
    async fn probe_and_bind(&self, req: &QuoteRequest) -> Result<ExecutableQuote, BindError> {
        let probe = self.post_quote(req).await.map_err(|error| BindError {
            error,
            report_priced: false,
        })?;
        let report_priced = !probe.pre_calls.is_empty();

        let mut bind_req = req.clone();
        bind_req.min_out = Some(Self::bind_floor(
            probe.expected_output,
            self.slippage_bps,
            req.min_out,
        ));

        let mut attempt = 0u32;
        loop {
            match self.post_quote(&bind_req).await {
                Ok(mut quote) => {
                    Self::keep_probe_ordinary_out(&mut quote, &probe);
                    self.store_quote(req, &quote);
                    return Ok(quote);
                }
                Err(error) if error.is_transient() && attempt < BIND_RETRY_ATTEMPTS => {
                    attempt += 1;
                    tracing::debug!(attempt, %error, "retrying router bind request");
                }
                Err(error) => {
                    return Err(BindError {
                        error,
                        report_priced,
                    });
                }
            }
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct RequestBody {
    input_token: String,
    output_token: String,
    amount_in: String,
    exact: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_out: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gas_price_gwei: Option<f64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    report_aware: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ResponseBody {
    expected_out: String,
    quote_quality: Option<QuoteQualityWire>,
    gas_estimate: u64,
    final_token: String,
    router_address: String,
    calldata: Option<String>,
    error: Option<String>,
    report: Option<ReportWire>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ReportWire {
    selected: ReportSelectionWire,
    observation_time_us: u64,
    inclusion_timestamp: u64,
    age_at_inclusion_secs: f64,
    #[serde(default)]
    bank_calls: Vec<BankCallWire>,
    shadow: ReportShadowWire,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReportSelectionWire {
    Report,
    Ordinary,
}

#[derive(Debug, Deserialize)]
struct BankCallWire {
    to: String,
    data: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct ReportShadowWire {
    ordinary_out: Option<String>,
    next_slot_out: Option<String>,
    one_slot_late_out: Option<String>,
    one_slot_late_failure: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuoteQualityWire {
    InterpolatedEstimate,
    RouteEstimate,
    RouterExecutionEstimate,
}

impl From<QuoteQualityWire> for QuoteQuality {
    fn from(w: QuoteQualityWire) -> Self {
        match w {
            QuoteQualityWire::InterpolatedEstimate => Self::Interpolated,
            QuoteQualityWire::RouteEstimate => Self::Route,
            QuoteQualityWire::RouterExecutionEstimate => Self::RouterExecution,
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, alloy::primitives::Address};

    fn dummy_req(is_quote: bool) -> QuoteRequest {
        QuoteRequest {
            sell_token: Address::repeat_byte(0x11),
            buy_token: Address::repeat_byte(0x22),
            sell_amount: eth::U256::from(1_000_000u64),
            is_quote,
            receiver: Address::repeat_byte(0x90),
            min_out: Some(eth::U256::from(900_000u64)),
            gas_price_gwei: Some(1.5),
            report_aware: false,
        }
    }

    fn ok_body() -> ResponseBody {
        ResponseBody {
            expected_out: "1000000".into(),
            quote_quality: Some(QuoteQualityWire::RouterExecutionEstimate),
            gas_estimate: 350_000,
            final_token: format!("{:#x}", Address::repeat_byte(0x22)),
            router_address: format!("{:#x}", Address::repeat_byte(0xCA)),
            calldata: Some("0xdeadbeef".into()),
            error: None,
            report: None,
        }
    }

    #[test]
    fn build_request_body_real_solve_sets_exact_receiver_and_min_out() {
        let req = dummy_req(false);
        let body = NewRouterClient::build_request_body(&req);
        assert!(body.exact);
        assert!(body.receiver.is_some());
        assert_eq!(body.min_out.as_deref(), Some("900000"));
        assert_eq!(body.amount_in, "1000000");
    }

    #[test]
    fn build_request_body_quote_omits_receiver_and_disables_exact() {
        let req = dummy_req(true);
        let body = NewRouterClient::build_request_body(&req);
        assert!(!body.exact);
        assert!(body.receiver.is_none());
    }

    #[test]
    fn parse_response_returns_executable_quote_on_real_solve() {
        let req = dummy_req(false);
        let q = NewRouterClient::parse_response(ok_body(), &req, Address::repeat_byte(0xCA))
            .expect("ok");
        assert_eq!(q.expected_output, eth::U256::from(1_000_000u64));
        // Invariant: artifact min_out matches what's encoded in calldata,
        // i.e. the request's min_out.
        assert_eq!(q.min_out, eth::U256::from(900_000u64));
        assert_eq!(q.router_address, Address::repeat_byte(0xCA));
        assert_eq!(q.calldata, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(q.gas_estimate, Some(350_000));
        assert_eq!(q.quality, Some(QuoteQuality::RouterExecution));
    }

    #[test]
    fn parse_response_min_out_echoes_request_when_present() {
        // Critical invariant: bid_floor == calldata_floor == req.min_out.
        // The two-round-trip `quote()` path is what reconciles this with
        // slippage-adjusted expected_output. `parse_response` itself just
        // surfaces whatever floor went on the wire.
        let mut req = dummy_req(false);
        req.min_out = Some(eth::U256::from(987_654u64));
        let q = NewRouterClient::parse_response(ok_body(), &req, Address::repeat_byte(0xCA))
            .expect("ok");
        assert_eq!(q.min_out, eth::U256::from(987_654u64));
    }

    #[test]
    fn parse_response_min_out_falls_back_to_expected_when_request_lacks_it() {
        // Quote-only path with no request min_out: server bakes
        // expected_output as the calldata floor; we mirror that here.
        let mut req = dummy_req(true);
        req.min_out = None;
        let q = NewRouterClient::parse_response(ok_body(), &req, Address::repeat_byte(0xCA))
            .expect("ok");
        assert_eq!(q.min_out, q.expected_output);
    }

    #[test]
    fn parse_response_rejects_router_address_mismatch() {
        let req = dummy_req(false);
        let err = NewRouterClient::parse_response(ok_body(), &req, Address::repeat_byte(0xBB))
            .expect_err("must fail");
        assert!(matches!(err, Error::RouterAddressMismatch { .. }));
    }

    #[test]
    fn parse_response_rejects_final_token_mismatch() {
        let req = dummy_req(false);
        let mut body = ok_body();
        body.final_token = format!("{:#x}", Address::repeat_byte(0xFF));
        let err = NewRouterClient::parse_response(body, &req, Address::repeat_byte(0xCA))
            .expect_err("must fail");
        assert!(matches!(err, Error::FinalTokenMismatch { .. }));
    }

    #[test]
    fn parse_response_real_solve_fails_when_calldata_missing() {
        let req = dummy_req(false);
        let mut body = ok_body();
        body.calldata = None;
        let err = NewRouterClient::parse_response(body, &req, Address::repeat_byte(0xCA))
            .expect_err("must fail");
        assert!(matches!(err, Error::CalldataUnavailable(_)));
    }

    #[test]
    fn parse_response_quote_request_tolerates_missing_calldata() {
        let req = dummy_req(true);
        let mut body = ok_body();
        body.calldata = None;
        let q =
            NewRouterClient::parse_response(body, &req, Address::repeat_byte(0xCA)).expect("ok");
        assert!(q.calldata.is_empty());
        assert_eq!(q.expected_output, eth::U256::from(1_000_000u64));
    }

    #[test]
    fn parse_response_surfaces_server_error_field() {
        let req = dummy_req(false);
        let mut body = ok_body();
        body.error = Some("min_out_exceeds_route_output".into());
        let err = NewRouterClient::parse_response(body, &req, Address::repeat_byte(0xCA))
            .expect_err("must fail");
        assert!(matches!(err, Error::CalldataUnavailable(_)));
    }

    fn report_body(selected: &str, bank_calls: serde_json::Value) -> ResponseBody {
        let mut body = ok_body();
        body.report = Some(
            serde_json::from_value(serde_json::json!({
                "selected": selected,
                "observation_time_us": 1_795_626_006_000_000u64,
                "inclusion_timestamp": 1_795_626_012u64,
                "age_at_inclusion_secs": 6.0,
                "bank_calls": bank_calls,
                "bank_gas": 75_659,
                "shadow": {
                    "ordinary_out": "990000",
                    "next_slot_out": "1000000",
                    "one_slot_late_out": null,
                },
            }))
            .unwrap(),
        );
        body
    }

    fn bank_call() -> serde_json::Value {
        serde_json::json!([{
            "to": format!("{:#x}", Address::repeat_byte(0xBA)),
            "data": "0xbeef",
            "value": "7",
        }])
    }

    #[test]
    fn build_request_body_asks_for_reports_on_real_solves_only() {
        let mut real = dummy_req(false);
        real.report_aware = true;
        assert!(NewRouterClient::build_request_body(&real).report_aware);

        let mut quote = dummy_req(true);
        quote.report_aware = true;
        assert!(!NewRouterClient::build_request_body(&quote).report_aware);

        let plain =
            serde_json::to_value(NewRouterClient::build_request_body(&dummy_req(false))).unwrap();
        assert!(plain.get("report_aware").is_none());
    }

    #[test]
    fn parse_response_report_route_carries_its_bank_calls() {
        let req = dummy_req(false);
        let q = NewRouterClient::parse_response(
            report_body("report", bank_call()),
            &req,
            Address::repeat_byte(0xCA),
        )
        .unwrap();

        assert_eq!(
            q.pre_calls,
            vec![PreCall {
                target: Address::repeat_byte(0xBA),
                value: eth::U256::from(7),
                calldata: vec![0xbe, 0xef],
            }]
        );
        let report = q.report.unwrap();
        assert!(report.selected);
        assert_eq!(report.inclusion_timestamp, 1_795_626_012);
        assert_eq!(report.ordinary_out, Some(eth::U256::from(990_000u64)));
        assert_eq!(report.next_slot_out, Some(eth::U256::from(1_000_000u64)));
        assert_eq!(report.one_slot_late_out, None);
    }

    #[test]
    fn parse_response_ordinary_route_has_no_bank_calls() {
        let req = dummy_req(false);
        let q = NewRouterClient::parse_response(
            report_body("ordinary", serde_json::json!([])),
            &req,
            Address::repeat_byte(0xCA),
        )
        .unwrap();

        assert!(q.pre_calls.is_empty());
        assert!(!q.report.unwrap().selected);
    }

    #[test]
    fn parse_response_rejects_a_report_route_without_its_bank_call() {
        let req = dummy_req(false);
        let err = NewRouterClient::parse_response(
            report_body("report", serde_json::json!([])),
            &req,
            Address::repeat_byte(0xCA),
        )
        .unwrap_err();
        assert!(matches!(err, Error::CalldataUnavailable(_)));
    }

    #[test]
    fn bound_quote_keeps_the_ordinary_output_the_probe_priced() {
        let req = dummy_req(false);
        let router = Address::repeat_byte(0xCA);
        let probe =
            NewRouterClient::parse_response(report_body("report", bank_call()), &req, router)
                .unwrap();
        let mut bound_body = report_body("report", bank_call());
        bound_body.report.as_mut().unwrap().shadow.ordinary_out = None;
        let mut bound = NewRouterClient::parse_response(bound_body, &req, router).unwrap();
        assert_eq!(bound.report.as_ref().unwrap().ordinary_out, None);

        NewRouterClient::keep_probe_ordinary_out(&mut bound, &probe);

        assert_eq!(
            bound.report.unwrap().ordinary_out,
            Some(eth::U256::from(990_000u64))
        );
    }

    #[test]
    fn report_quotes_are_never_cached() {
        let client = client_with_ttl(Duration::from_secs(30));
        let req = dummy_req(false);
        let mut quote = sample_quote();
        quote.pre_calls = vec![PreCall {
            target: Address::repeat_byte(0xBA),
            value: eth::U256::ZERO,
            calldata: vec![0xbe, 0xef],
        }];

        client.store_quote(&req, &quote);

        assert!(client.cached_quote(&req).is_none());
    }

    fn client_with_ttl(ttl: Duration) -> NewRouterClient {
        NewRouterClient::new(
            "https://router.invalid/quote".parse().unwrap(),
            Address::repeat_byte(0xCA),
            100,
            ttl,
        )
    }

    fn sample_quote() -> ExecutableQuote {
        ExecutableQuote {
            expected_output: eth::U256::from(1_000_000u64),
            min_out: eth::U256::from(990_000u64),
            router_address: Address::repeat_byte(0xCA),
            calldata: vec![0xde, 0xad],
            gas_estimate: Some(350_000),
            quality: Some(QuoteQuality::RouterExecution),
            pre_calls: Vec::new(),
            report: None,
        }
    }

    #[test]
    fn quote_cache_returns_a_fresh_entry() {
        let client = client_with_ttl(Duration::from_secs(30));
        let req = dummy_req(false);
        assert!(client.cached_quote(&req).is_none());

        client.store_quote(&req, &sample_quote());
        let hit = client.cached_quote(&req).expect("should hit");
        assert_eq!(hit.expected_output, eth::U256::from(1_000_000u64));
        assert_eq!(hit.min_out, eth::U256::from(990_000u64));
    }

    #[test]
    fn quote_cache_is_disabled_at_zero_ttl() {
        let client = client_with_ttl(Duration::ZERO);
        let req = dummy_req(false);
        client.store_quote(&req, &sample_quote());
        assert!(
            client.cached_quote(&req).is_none(),
            "a zero TTL must neither store nor serve"
        );
    }

    #[test]
    fn quote_cache_still_hits_for_a_repeated_order() {
        // Keying on the limit must not cost the hit rate the cache exists for:
        // an order's limit is fixed for its lifetime, so the same order
        // re-quoted in the next auction is byte-identical and must hit.
        let client = client_with_ttl(Duration::from_secs(30));
        let req = dummy_req(false);
        client.store_quote(&req, &sample_quote());

        let next_auction = req.clone();
        assert!(client.cached_quote(&next_auction).is_some());
    }

    #[test]
    fn quote_cache_expires_entries() {
        // A one-nanosecond TTL is already elapsed by the time we look.
        let client = client_with_ttl(Duration::from_nanos(1));
        let req = dummy_req(false);
        client.store_quote(&req, &sample_quote());
        assert!(client.cached_quote(&req).is_none());
    }

    #[test]
    fn quote_cache_discriminates_by_pair_and_amount() {
        let client = client_with_ttl(Duration::from_secs(30));
        let req = dummy_req(false);
        client.store_quote(&req, &sample_quote());

        let mut other_amount = req.clone();
        other_amount.sell_amount = eth::U256::from(999u64);
        assert!(client.cached_quote(&other_amount).is_none());

        let mut other_pair = req.clone();
        other_pair.buy_token = Address::repeat_byte(0x33);
        assert!(client.cached_quote(&other_pair).is_none());

        // Different order limit on the same pair and size. `bind_floor` clamps
        // up to the limit, so the limit reaches the calldata — sharing an entry
        // would give this order a floor below what it needs, which solve_order
        // rejects as InsufficientOutput *and* backs off for five minutes.
        let mut higher_limit = req.clone();
        higher_limit.min_out = Some(eth::U256::from(999_999u64));
        assert!(client.cached_quote(&higher_limit).is_none());

        // The original key still hits — the misses above were real misses, not
        // the cache having been clobbered.
        assert!(client.cached_quote(&req).is_some());
    }

    const BANK: Address = Address::repeat_byte(0xBA);

    fn configured_bank() -> ReportBank {
        ReportBank {
            address: BANK,
            max_fee: eth::U256::ZERO,
        }
    }

    fn deliver_report(update: Vec<u8>) -> Vec<u8> {
        deliver_reportCall {
            update: update.into(),
        }
        .abi_encode()
    }

    fn report_quote(pre_call: PreCall) -> ExecutableQuote {
        let mut quote = sample_quote();
        quote.pre_calls = vec![pre_call];
        quote
    }

    fn bank_pre_call() -> PreCall {
        PreCall {
            target: BANK,
            value: eth::U256::ZERO,
            calldata: deliver_report(vec![0x2a; 139]),
        }
    }

    fn report_req() -> QuoteRequest {
        let mut req = dummy_req(false);
        req.report_aware = true;
        req
    }

    fn checked(quote: &ExecutableQuote, req: &QuoteRequest) -> Result<(), Error> {
        NewRouterClient::check_pre_calls(quote, req, Some(&configured_bank()))
    }

    #[test]
    fn delivery_to_the_configured_bank_is_accepted() {
        assert!(checked(&report_quote(bank_pre_call()), &report_req()).is_ok());
        assert!(checked(&sample_quote(), &dummy_req(false)).is_ok());
    }

    #[test]
    fn bank_call_to_any_other_contract_is_rejected() {
        let mut call = bank_pre_call();
        call.target = Address::repeat_byte(0xEE);
        assert!(checked(&report_quote(call), &report_req()).is_err());
    }

    #[test]
    fn bank_call_that_is_not_a_plain_report_delivery_is_rejected() {
        // another function on the right contract
        let mut other_function = bank_pre_call();
        other_function.calldata[..4].copy_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
        assert!(checked(&report_quote(other_function), &report_req()).is_err());

        // the right function with bytes appended after its arguments
        let mut trailing = bank_pre_call();
        trailing.calldata.extend_from_slice(&[0u8; 32]);
        assert!(checked(&report_quote(trailing), &report_req()).is_err());

        // an update longer than the bank accepts
        let mut oversized = bank_pre_call();
        oversized.calldata = deliver_report(vec![0x2a; MAX_REPORT_UPDATE_BYTES + 1]);
        assert!(checked(&report_quote(oversized), &report_req()).is_err());
    }

    #[test]
    fn bank_call_carrying_more_than_the_fee_ceiling_is_rejected() {
        let mut call = bank_pre_call();
        call.value = eth::U256::from(1);
        assert!(checked(&report_quote(call), &report_req()).is_err());
    }

    #[test]
    fn more_than_one_bank_call_is_rejected() {
        let mut quote = report_quote(bank_pre_call());
        quote.pre_calls.push(bank_pre_call());
        assert!(checked(&quote, &report_req()).is_err());
    }

    #[test]
    fn report_route_is_rejected_when_it_was_not_asked_for() {
        let quote = report_quote(bank_pre_call());
        assert!(checked(&quote, &dummy_req(false)).is_err());
        assert!(
            NewRouterClient::check_pre_calls(&quote, &report_req(), None).is_err(),
            "no bank configured"
        );
    }

    /// A router whose report route is on offer to a probe but gone by the
    /// bind: report-aware requests with the probe's floor fail, everything
    /// else gets the ordinary route.
    async fn serve_report_route_that_cannot_be_bound() -> Url {
        let app = axum::Router::new().route(
            "/quote",
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    let ok = |out: &str| {
                        serde_json::json!({
                            "expected_out": out,
                            "gas_estimate": 350_000,
                            "final_token": format!("{:#x}", Address::repeat_byte(0x22)),
                            "router_address": format!("{:#x}", Address::repeat_byte(0xCA)),
                            "calldata": "0xdeadbeef",
                        })
                    };
                    let report_aware = body["report_aware"].as_bool().unwrap_or(false);
                    let min_out: u64 = body["min_out"].as_str().unwrap().parse().unwrap();
                    let response = if !report_aware {
                        ok("1000000")
                    } else if min_out <= 900_000 {
                        let mut probe = ok("1010000");
                        probe["report"] = serde_json::json!({
                            "selected": "report",
                            "observation_time_us": 1u64,
                            "inclusion_timestamp": 7u64,
                            "age_at_inclusion_secs": 6.0,
                            "bank_calls": [{
                                "to": format!("{BANK:#x}"),
                                "data": format!(
                                    "0x{}",
                                    alloy::hex::encode(deliver_report(vec![0x2a; 139]))
                                ),
                                "value": "0",
                            }],
                            "shadow": {
                                "ordinary_out": "1000000",
                                "next_slot_out": "1010000",
                                "one_slot_late_out": null,
                            },
                        });
                        probe
                    } else {
                        serde_json::json!({
                            "expected_out": "0",
                            "gas_estimate": 0,
                            "final_token": "0x0",
                            "router_address": "",
                            "error": "min_out exceeds selected route output",
                        })
                    };
                    axum::Json(response)
                },
            ),
        );
        let server =
            axum::Server::bind(&"127.0.0.1:0".parse().unwrap()).serve(app.into_make_service());
        let addr = server.local_addr();
        tokio::spawn(server);
        format!("http://{addr}/quote").parse().unwrap()
    }

    #[tokio::test]
    async fn report_route_lost_between_probe_and_bind_falls_back_to_the_ordinary_route() {
        let url = serve_report_route_that_cannot_be_bound().await;
        let client = NewRouterClient::new(url, Address::repeat_byte(0xCA), 30, Duration::ZERO)
            .with_report_bank(Some(configured_bank()));

        // dummy_req's limit is 900_000: the ordinary route clears it
        let quote = client.quote(&report_req()).await.expect("ordinary route");

        assert!(quote.pre_calls.is_empty());
        assert_eq!(quote.expected_output, eth::U256::from(1_000_000u64));
        assert!(quote.min_out >= eth::U256::from(900_000u64));
    }

    async fn serve_fixed_quote(expected_out: &'static str) -> Url {
        let app = axum::Router::new().route(
            "/quote",
            axum::routing::post(move || async move {
                axum::Json(serde_json::json!({
                    "expected_out": expected_out,
                    "gas_estimate": 350_000,
                    "final_token": format!("{:#x}", Address::repeat_byte(0x22)),
                    "router_address": format!("{:#x}", Address::repeat_byte(0xCA)),
                }))
            }),
        );
        let server =
            axum::Server::bind(&"127.0.0.1:0".parse().unwrap()).serve(app.into_make_service());
        let addr = server.local_addr();
        tokio::spawn(server);
        format!("http://{addr}/quote").parse().unwrap()
    }

    #[tokio::test]
    async fn quote_only_floor_mirrors_the_bind_floor_not_the_placeholder_limit() {
        // CoW's /quote sends a sell order with a 1 wei buy amount. The
        // quote-only path makes no bind trip, so the floor it reports is what
        // `sell_order_bid_output` clamps the bid to. Echoing the 1 wei
        // placeholder there collapsed every sell-side quote to a 1 wei bid.
        let url = serve_fixed_quote("1000000").await;
        let client = NewRouterClient::new(url, Address::repeat_byte(0xCA), 30, Duration::ZERO);
        let mut req = dummy_req(true);
        req.min_out = Some(eth::U256::from(1u64));

        let quote = client.quote(&req).await.expect("quote");

        assert_eq!(quote.expected_output, eth::U256::from(1_000_000u64));
        assert_eq!(quote.min_out, eth::U256::from(997_000u64));
    }

    #[test]
    fn bind_floor_applies_slippage_to_expected_output() {
        let floor = NewRouterClient::bind_floor(eth::U256::from(1_000_000u64), 100, None);
        assert_eq!(floor, eth::U256::from(990_000u64));
    }

    #[test]
    fn bind_floor_never_drops_below_the_order_limit() {
        // Slippage haircut would land under the order's own limit, so the
        // limit wins — the router must not be asked for less than the order
        // requires.
        let floor = NewRouterClient::bind_floor(
            eth::U256::from(1_000_000u64),
            100,
            Some(eth::U256::from(995_000u64)),
        );
        assert_eq!(floor, eth::U256::from(995_000u64));
    }

    #[test]
    fn bind_floor_ignores_a_nominal_order_limit() {
        // The regression that motivated removing the retry fallback: Curve's
        // fee collector posts orders with a flat 5 crvUSD minimum. The bound
        // floor must track the route's real output, not that nominal limit.
        let expected = eth::U256::from(7_107_000_000_000_000_000_000u128);
        let nominal_limit = eth::U256::from(5_000_000_000_000_000_000u128);
        let floor = NewRouterClient::bind_floor(expected, 100, Some(nominal_limit));
        assert_eq!(floor, eth::U256::from(7_035_930_000_000_000_000_000u128));
        assert!(floor > nominal_limit);
    }

    #[test]
    fn transient_statuses_are_retryable() {
        for status in [
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            reqwest::StatusCode::BAD_GATEWAY,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            reqwest::StatusCode::GATEWAY_TIMEOUT,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::REQUEST_TIMEOUT,
        ] {
            assert!(
                NewRouterClient::status_is_transient(status),
                "{status} should be retryable"
            );
        }
    }

    #[test]
    fn deterministic_statuses_are_not_retryable() {
        // Retrying these only burns the shared route deadline.
        for status in [
            reqwest::StatusCode::BAD_REQUEST,
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            assert!(
                !NewRouterClient::status_is_transient(status),
                "{status} should not be retryable"
            );
        }
    }
}
