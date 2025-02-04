/*
Copyright 2024 The Spice.ai OSS Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use crate::tls::TlsConfig;
use bytes::Bytes;
use http::{HeaderValue, Request, Response};
use http_body_util::Full;
use hyper::{
    body::{self, Incoming},
    header::CONTENT_TYPE,
    server::conn::http1::Builder,
};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::PrometheusHandle;
use snafu::prelude::*;
use std::net::ToSocketAddrs;
use std::{fmt::Debug, sync::Arc};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Unable to bind to address: {source}"))]
    UnableToBindServerToPort { source: std::io::Error },

    #[snafu(display("Unable to start HTTP server: {source}"))]
    UnableToStartHttpServer { source: std::io::Error },
}

type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) async fn start<A>(
    bind_address: Option<A>,
    handle: Option<PrometheusHandle>,
    tls_config: Option<Arc<TlsConfig>>,
) -> Result<()>
where
    A: ToSocketAddrs + Debug + Clone + Copy,
{
    let (Some(bind_address), Some(handle)) = (bind_address, handle) else {
        return Ok(());
    };

    let listener = std::net::TcpListener::bind(bind_address)
        .and_then(|listener| {
            listener.set_nonblocking(true)?;
            Ok(listener)
        })
        .context(UnableToBindServerToPortSnafu)?;
    let listener = TcpListener::from_std(listener).context(UnableToBindServerToPortSnafu)?;
    tracing::info!("Spice Runtime Metrics listening on {:?}", bind_address);

    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::debug!(
                    "Error accepting connection to serve Prometheus metrics request: {e}"
                );
                continue;
            }
        };

        match tls_config {
            Some(ref config) => {
                let acceptor = TlsAcceptor::from(Arc::clone(&config.server_config));
                process_tls_tcp_stream(stream, acceptor.clone(), handle.clone());
            }
            None => {
                process_tcp_stream(stream, handle.clone());
            }
        }
    }
}

fn process_tls_tcp_stream(stream: TcpStream, acceptor: TlsAcceptor, handle: PrometheusHandle) {
    tokio::spawn(async move {
        let stream = acceptor.accept(stream).await;
        match stream {
            Ok(stream) => {
                serve_connection(stream, handle).await;
            }
            Err(e) => {
                tracing::debug!("Error accepting TLS connection: {e}");
            }
        }
    });
}

fn process_tcp_stream(stream: TcpStream, handle: PrometheusHandle) {
    tokio::spawn(serve_connection(stream, handle));
}

async fn serve_connection<S>(stream: S, handle: PrometheusHandle)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: Request<body::Incoming>| {
        let handle = handle.clone();
        async move { Ok::<_, hyper::Error>(handle_http_request(&handle, &req)) }
    });

    if let Err(err) = Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        tracing::debug!(error = ?err, "Error serving Prometheus metrics connection.");
    }
}

fn handle_http_request(
    handle: &PrometheusHandle,
    req: &Request<Incoming>,
) -> Response<Full<Bytes>> {
    let mut response = Response::new(match req.uri().path() {
        "/health" => "OK".into(),
        _ => handle.render().into(),
    });
    response
        .headers_mut()
        .append(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}
