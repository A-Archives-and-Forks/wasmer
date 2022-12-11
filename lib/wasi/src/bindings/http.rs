use std::string::FromUtf8Error;

use wasmer_wasi_types::wasix::wasix_http_client_v1::{
    BodyParam, BodyResult, HeaderResult, Request, Response, WasixHttpClientV1,
};

use crate::{
    http::{DynHttpClient, HttpClientCapabilityV1},
    WasiEnv,
};

pub struct WasixHttpClientImpl {
    env: WasiEnv,
}

impl WasixHttpClientImpl {
    pub fn new(env: WasiEnv) -> Self {
        Self { env }
    }
}

#[derive(Debug)]
pub struct ClientImpl {
    client: DynHttpClient,
    capabilities: HttpClientCapabilityV1,
}

impl WasixHttpClientV1 for WasixHttpClientImpl {
    type Client = ClientImpl;

    fn client_new(&mut self) -> Result<Self::Client, String> {
        let capabilities = if self.env.capabilities.insecure_allow_all {
            HttpClientCapabilityV1::new_allow_all()
        } else if !self.env.capabilities.http.is_deny_all() {
            self.env.capabilities.http.clone()
        } else {
            return Err("Permission denied - http client not enabled".to_string());
        };

        let client = self
            .env
            .runtime
            .http_client()
            .ok_or_else(|| "No http client available".to_string())?
            .clone();
        Ok(ClientImpl {
            client,
            capabilities,
        })
    }

    fn client_send(
        &mut self,
        self_: &Self::Client,
        request: Request<'_>,
    ) -> Result<Response, String> {
        let uri: http::Uri = request
            .url
            .parse()
            .map_err(|err| format!("Invalid request url: {err}"))?;
        let host = uri.host().unwrap_or_default();
        if !self_.capabilities.can_access_domain(host) {
            return Err(format!(
                "Permission denied: http capability not enabled for host '{host}'"
            ));
        }

        let headers = request
            .headers
            .into_iter()
            .map(|h| {
                let value = String::from_utf8(h.value.to_vec())?;
                Ok((h.key.to_string(), value))
            })
            .collect::<Result<Vec<_>, FromUtf8Error>>()
            .map_err(|_| "non-utf8 request header")?;

        // FIXME: stream body...

        let body = match request.body {
            Some(BodyParam::Fd(_)) => {
                return Err("File descriptor bodies not supported yet".to_string());
            }
            Some(BodyParam::Data(data)) => Some(data.to_vec()),
            None => None,
        };

        let req = crate::http::HttpRequest {
            url: request.url.to_string(),
            method: request.method.to_string(),
            headers,
            body,
            options: crate::http::HttpRequestOptions {
                gzip: false,
                cors_proxy: None,
            },
        };
        let f = self_.client.request(req);

        let res = self.env.tasks.block_on(f).map_err(|e| e.to_string())?;

        let res_headers = res
            .headers
            .into_iter()
            .map(|(key, value)| HeaderResult {
                key,
                value: value.into_bytes(),
            })
            .collect();

        let res_body = if let Some(b) = res.body {
            BodyResult::Data(b)
        } else {
            BodyResult::Data(Vec::new())
        };

        Ok({
            Response {
                status: res.status,
                headers: res_headers,
                body: res_body,
                // TODO: provide redirect urls?
                redirect_urls: None,
            }
        })
    }
}
