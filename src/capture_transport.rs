//! Streaming forwarding and a private download route. Downloads do not traverse
//! the interception proxy and keep working after capture stops.
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::TryStreamExt;
use http_body_util::BodyExt;
use hudsucker::hyper::body::Body as _;
use hudsucker::{
    hyper::{self, header, Method, Request, Response, StatusCode},
    hyper_util::rt::TokioIo,
    Body,
};
use sha2::{Digest, Sha256};
use std::{convert::Infallible, sync::RwLock, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

static DOWNLOAD_ROUTE: RwLock<Option<String>> = RwLock::new(None);
pub(crate) fn download_proxy() -> Option<String> {
    DOWNLOAD_ROUTE.read().unwrap().clone()
}
pub(crate) fn set_download_proxy(url: Option<String>) {
    *DOWNLOAD_ROUTE.write().unwrap() = url;
}

pub(crate) fn client(upstream: Option<&str>) -> Result<reqwest::Client> {
    Ok(client_builder(upstream)?.build()?)
}
pub(crate) fn websocket_client(upstream: &str) -> Result<reqwest::Client> {
    Ok(client_builder(Some(upstream))?.http1_only().build()?)
}
fn client_builder(upstream: Option<&str>) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60));
    if let Some(url) = upstream {
        builder = builder.proxy(reqwest::Proxy::all(url)?);
    }
    Ok(builder)
}
pub(crate) async fn forward_websocket(
    client: &reqwest::Client,
    mut request: Request<Body>,
) -> Result<Response<Body>> {
    let incoming = hyper::upgrade::on(&mut request);
    let (mut parts, _) = request.into_parts();
    strip_hop_headers(&mut parts.headers);
    parts.headers.remove(header::HOST);
    parts.headers.insert(header::CONNECTION, "upgrade".parse()?);
    parts.headers.insert(header::UPGRADE, "websocket".parse()?);
    let response = client
        .request(parts.method, parts.uri.to_string())
        .headers(parts.headers)
        .send()
        .await?;
    let status = response.status();
    let mut headers = response.headers().clone();
    let mut result = if status == StatusCode::SWITCHING_PROTOCOLS {
        let mut remote = response.upgrade().await?;
        tokio::spawn(async move {
            if let Ok(stream) = incoming.await {
                let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(stream), &mut remote).await;
            }
        });
        Response::builder().status(status).body(Body::empty())?
    } else {
        strip_hop_headers(&mut headers);
        Response::builder().status(status).body(Body::from_stream(
            response.bytes_stream().map_err(std::io::Error::other),
        ))?
    };
    *result.headers_mut() = headers;
    Ok(result)
}
fn strip_hop_headers(headers: &mut hyper::HeaderMap) {
    let named: Vec<_> = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|v| v.trim().to_owned())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "proxy-connection",
        "proxy-authorization",
        "proxy-authenticate",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
    ] {
        headers.remove(name);
    }
}
pub(crate) async fn forward(
    client: &reqwest::Client,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (mut parts, body) = request.into_parts();
    strip_hop_headers(&mut parts.headers);
    parts.headers.remove(header::HOST);
    let mut outgoing = client
        .request(parts.method, parts.uri.to_string())
        .headers(parts.headers);
    if !body.is_end_stream() {
        outgoing = outgoing.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let response = outgoing.send().await?;
    let mut headers = response.headers().clone();
    strip_hop_headers(&mut headers);
    let mut result = Response::builder()
        .status(response.status())
        .body(Body::from_stream(
            response.bytes_stream().map_err(std::io::Error::other),
        ))?;
    *result.headers_mut() = headers;
    Ok(result)
}
fn error(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from("FFDownload transport unavailable"))
        .unwrap()
}
pub(crate) struct DownloadForwarder {
    pub url: String,
    pub upstream: Option<String>,
    task: JoinHandle<()>,
}
impl Drop for DownloadForwarder {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl DownloadForwarder {
    pub async fn start(upstream: Option<String>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let secret = format!(
            "{:x}",
            Sha256::digest(rcgen::KeyPair::generate()?.serialize_der())
        );
        let auth = format!("Basic {}", STANDARD.encode(format!("ffdm:{secret}")));
        let client = client(upstream.as_deref())?;
        let original_upstream = upstream.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (auth, upstream, client) = (auth.clone(), upstream.clone(), client.clone());
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(
                        move |mut req: Request<hyper::body::Incoming>| {
                            let (auth, upstream, client) =
                                (auth.clone(), upstream.clone(), client.clone());
                            async move {
                                let response = if req
                                    .headers()
                                    .get(header::PROXY_AUTHORIZATION)
                                    .and_then(|v| v.to_str().ok())
                                    != Some(&auth)
                                {
                                    error(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
                                } else if req.method() == Method::CONNECT {
                                    match tunnel(&req, upstream.as_deref()).await {
                                        Ok(mut remote) => {
                                            let upgrade = hyper::upgrade::on(&mut req);
                                            tokio::spawn(async move {
                                                if let Ok(upgraded) = upgrade.await {
                                                    let _ = tokio::io::copy_bidirectional(
                                                        &mut TokioIo::new(upgraded),
                                                        &mut remote,
                                                    )
                                                    .await;
                                                }
                                            });
                                            Response::new(Body::empty())
                                        }
                                        Err(_) => error(StatusCode::BAD_GATEWAY),
                                    }
                                } else {
                                    forward(&client, req.map(Body::from))
                                        .await
                                        .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY))
                                };
                                Ok::<_, Infallible>(response)
                            }
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        Ok(Self {
            url: format!("http://ffdm:{secret}@{address}"),
            upstream: original_upstream,
            task,
        })
    }
}
async fn tunnel<B>(request: &Request<B>, upstream: Option<&str>) -> Result<TcpStream> {
    let target = request
        .uri()
        .authority()
        .context("missing CONNECT authority")?
        .as_str();
    tokio::time::timeout(Duration::from_secs(15), async {
        let Some(upstream) = upstream else {
            return Ok(TcpStream::connect(target).await?);
        };
        let url = reqwest::Url::parse(upstream)?;
        let mut socket = TcpStream::connect((
            url.host_str().context("proxy host")?,
            url.port_or_known_default().context("proxy port")?,
        ))
        .await?;
        socket
            .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await?;
        let mut head = Vec::new();
        while head.len() < 8192 {
            head.push(socket.read_u8().await?);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        ensure!(
            head.ends_with(b"\r\n\r\n")
                && std::str::from_utf8(&head)?
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    == Some("200"),
            "upstream CONNECT rejected"
        );
        Ok(socket)
    })
    .await
    .context("proxy connection timed out")?
}

/// Relay the original TLS stream without generating a certificate or inspecting
/// account traffic, while preserving the user's existing upstream proxy.
pub(crate) async fn passthrough_connect(
    mut request: Request<Body>,
    upstream: Option<&str>,
) -> Result<Response<Body>> {
    let mut remote = tunnel(&request, upstream).await?;
    let incoming = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        if let Ok(stream) = incoming.await {
            let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(stream), &mut remote).await;
        }
    });
    Ok(Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn private_download_route_streams_without_interception() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = [0; 4];
            socket.read_exact(&mut data).await.unwrap();
            socket.write_all(&data).await.unwrap();
        });
        let forwarder = DownloadForwarder::start(None).await.unwrap();
        let url = reqwest::Url::parse(&forwarder.url).unwrap();
        let mut socket = TcpStream::connect((url.host_str().unwrap(), url.port().unwrap()))
            .await
            .unwrap();
        let token = STANDARD.encode(format!("{}:{}", url.username(), url.password().unwrap()));
        socket.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {token}\r\n\r\n").as_bytes()).await.unwrap();
        let mut head = vec![];
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
        assert!(std::str::from_utf8(&head)
            .unwrap()
            .starts_with("HTTP/1.1 200"));
        socket.write_all(b"ping").await.unwrap();
        let mut data = [0; 4];
        socket.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"ping");
        echo.await.unwrap();
        let unauthenticated = reqwest::Client::builder()
            .no_proxy()
            .proxy(
                reqwest::Proxy::http(format!(
                    "http://{}:{}",
                    url.host_str().unwrap(),
                    url.port().unwrap()
                ))
                .unwrap(),
            )
            .build()
            .unwrap();
        assert_eq!(
            unauthenticated
                .get("http://example.com/")
                .send()
                .await
                .unwrap()
                .status(),
            407
        );
    }
}
