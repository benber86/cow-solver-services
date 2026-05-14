//! `RouteProvider` impl backed by *.router.curve.finance/quote.

use {
    super::route_provider::{Error, ExecutableQuote, QuoteQuality, QuoteRequest, RouteProvider},
    crate::domain::eth,
    async_trait::async_trait,
    reqwest::Url,
    serde::{Deserialize, Serialize},
    std::time::Duration,
};

pub struct NewRouterClient {
    http: reqwest::Client,
    quote_url: Url,
    router_address: eth::Address,
    slippage_bps: u32,
}

impl NewRouterClient {
    pub fn new(quote_url: Url, router_address: eth::Address, slippage_bps: u32) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            quote_url,
            router_address,
            slippage_bps,
        }
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

        Ok(ExecutableQuote {
            expected_output,
            min_out,
            router_address: returned_router,
            calldata,
            gas_estimate: Some(body.gas_estimate),
            quality: body.quote_quality.map(Into::into),
        })
    }

    async fn post_quote(&self, req: &QuoteRequest) -> Result<ExecutableQuote, Error> {
        let body = Self::build_request_body(req);
        let resp = self
            .http
            .post(self.quote_url.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::CalldataUnavailable(format!("network: {e}")))?;

        let status = resp.status();
        let raw = resp
            .bytes()
            .await
            .map_err(|e| Error::CalldataUnavailable(format!("read body: {e}")))?;

        if !status.is_success() {
            let txt = String::from_utf8_lossy(&raw).to_string();
            return Err(Error::CalldataUnavailable(format!("HTTP {status}: {txt}")));
        }

        let parsed: ResponseBody = serde_json::from_slice(&raw).map_err(|e| {
            Error::CalldataUnavailable(format!(
                "parse error: {e}; body: {}",
                String::from_utf8_lossy(&raw)
            ))
        })?;

        Self::parse_response(parsed, req, self.router_address)
    }
}

#[async_trait]
impl RouteProvider for NewRouterClient {
    async fn quote(&self, req: &QuoteRequest) -> Result<ExecutableQuote, Error> {
        // Quote probes: single call, no calldata needed, server-side
        // `min_out` (if any) is the only floor that exists.
        if req.is_quote {
            return self.post_quote(req).await;
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
        // If step 2 fails (route degraded between calls), retry with the
        // order floor as the bid — strictly worse but always safe.
        let probe = self.post_quote(req).await?;

        let mut bid_floor =
            QuoteRequest::min_out_with_slippage(probe.expected_output, self.slippage_bps);
        if let Some(order_floor) = req.min_out {
            if bid_floor < order_floor {
                bid_floor = order_floor;
            }
        }

        let mut bind_req = req.clone();
        bind_req.min_out = Some(bid_floor);
        match self.post_quote(&bind_req).await {
            Ok(quote) => Ok(quote),
            Err(_) if req.min_out.is_some() => {
                // Fall back to the order floor — calldata still matches the
                // bid (now equal to req.min_out), invariant preserved.
                self.post_quote(req).await
            }
            Err(e) => Err(e),
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
}
