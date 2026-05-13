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
}

impl NewRouterClient {
    pub fn new(quote_url: Url, router_address: eth::Address) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            quote_url,
            router_address,
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

    fn parse_response(
        body: ResponseBody,
        req: &QuoteRequest,
        expected_router: eth::Address,
    ) -> Result<ExecutableQuote, Error> {
        if let Some(err) = body.error {
            return Err(Error::CalldataUnavailable(err));
        }

        let returned_router: eth::Address = body
            .router_address
            .parse()
            .map_err(|_| Error::CalldataUnavailable(format!(
                "invalid router_address: {}", body.router_address
            )))?;
        if returned_router != expected_router {
            return Err(Error::RouterAddressMismatch {
                expected: expected_router,
                got: returned_router,
            });
        }

        let final_token: eth::Address = body
            .final_token
            .parse()
            .map_err(|_| Error::CalldataUnavailable(format!(
                "invalid final_token: {}", body.final_token
            )))?;
        if final_token != req.buy_token {
            return Err(Error::FinalTokenMismatch {
                expected: req.buy_token,
                got: final_token,
            });
        }

        let expected_output: eth::U256 = body
            .expected_out
            .parse()
            .map_err(|_| Error::CalldataUnavailable(format!(
                "invalid expected_out: {}", body.expected_out
            )))?;

        let calldata = if req.is_quote {
            Vec::new()
        } else {
            let hex = body
                .calldata
                .ok_or_else(|| Error::CalldataUnavailable("missing calldata on real solve".into()))?;
            let stripped = hex.strip_prefix("0x").unwrap_or(&hex);
            alloy::hex::decode(stripped).map_err(|e| {
                Error::CalldataUnavailable(format!("invalid hex calldata: {e}"))
            })?
        };

        // The server already enforces `min_out` (sends 422 if unachievable) and
        // bakes it into the returned calldata. We carry it back as the artifact
        // floor; quote-only paths (no min_out) fall back to expected_output.
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
}

#[async_trait]
impl RouteProvider for NewRouterClient {
    async fn quote(&self, req: &QuoteRequest) -> Result<ExecutableQuote, Error> {
        let body = Self::build_request_body(req);
        let resp = self
            .http
            .post(self.quote_url.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::CalldataUnavailable(format!("network: {e}")))?;

        let status = resp.status();
        let raw = resp.bytes().await.map_err(|e| {
            Error::CalldataUnavailable(format!("read body: {e}"))
        })?;

        if !status.is_success() {
            let txt = String::from_utf8_lossy(&raw).to_string();
            return Err(Error::CalldataUnavailable(format!(
                "HTTP {status}: {txt}"
            )));
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
        assert_eq!(q.router_address, Address::repeat_byte(0xCA));
        assert_eq!(q.calldata, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(q.gas_estimate, Some(350_000));
        assert_eq!(q.quality, Some(QuoteQuality::RouterExecution));
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
        let q = NewRouterClient::parse_response(body, &req, Address::repeat_byte(0xCA))
            .expect("ok");
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
