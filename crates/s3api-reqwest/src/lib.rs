//! 基于 reqwest 的传输层。
//!
//! reqwest 的 future 只能在 tokio 上推进，而界面跑在 GPUI 自己的执行器上。
//! 这里把请求 spawn 到 tokio，再用通道把响应头和响应体转回来，
//! 返回的 future 与 stream 与运行时无关。

use std::io;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use s3api::{Body, HttpRequest, HttpResponse, HttpTransport, TransportError};
use tokio::runtime::Handle;

/// 响应体在通道里最多积压这么多块，反压交给 tokio 那侧。
const BODY_BUFFER: usize = 8;

#[derive(Debug, Clone)]
pub struct TransportOptions {
    pub connect_timeout: Duration,
    /// 两次读到数据之间的最长间隔，不是整个请求的上限，否则大文件会被误杀。
    pub read_timeout: Duration,
    /// 自签名证书的内网服务需要打开。
    pub insecure_tls: bool,
    pub user_agent: String,
}

impl Default for TransportOptions {
    fn default() -> Self {
        TransportOptions {
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(60),
            insecure_tls: false,
            user_agent: concat!("S3Browser/", env!("CARGO_PKG_VERSION")).to_owned(),
        }
    }
}

pub struct ReqwestTransport {
    client: reqwest::Client,
    handle: Handle,
}

impl ReqwestTransport {
    pub fn new(handle: Handle, options: TransportOptions) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            // S3 的分片上传在 HTTP/1.1 下行为最稳定，也避免 h2 的流控影响吞吐。
            .http1_only()
            .connect_timeout(options.connect_timeout)
            .read_timeout(options.read_timeout)
            .danger_accept_invalid_certs(options.insecure_tls)
            .user_agent(options.user_agent)
            .build()
            .map_err(|e| TransportError::Connect(e.to_string()))?;
        Ok(ReqwestTransport { client, handle })
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        let client = self.client.clone();
        let handle = self.handle.clone();
        Box::pin(async move {
            let (head_tx, head_rx) = async_channel::bounded(1);
            let (body_tx, body_rx) = async_channel::bounded::<io::Result<Bytes>>(BODY_BUFFER);

            handle.spawn(async move {
                let result = start(client, req).await;
                let response = match result {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = head_tx.send(Err(e)).await;
                        return;
                    }
                };
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                if head_tx.send(Ok((status, headers))).await.is_err() {
                    return;
                }

                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let item = chunk.map_err(|e| io::Error::other(describe(&e)));
                    let failed = item.is_err();
                    // 接收端不要了就停下，别继续把数据拉进内存。
                    if body_tx.send(item).await.is_err() || failed {
                        break;
                    }
                }
            });

            let (status, headers) = head_rx
                .recv()
                .await
                .map_err(|_| TransportError::Io("传输任务意外退出".into()))??;

            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(body_rx) as BoxStream<'static, io::Result<Bytes>>,
            })
        })
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        let handle = self.handle.clone();
        Box::pin(async move {
            let (tx, rx) = async_channel::bounded::<()>(1);
            handle.spawn(async move {
                tokio::time::sleep(duration).await;
                let _ = tx.send(()).await;
            });
            let _ = rx.recv().await;
        })
    }
}

async fn start(
    client: reqwest::Client,
    req: HttpRequest,
) -> Result<reqwest::Response, TransportError> {
    let mut builder = client.request(req.method, &req.url).headers(req.headers);

    // S3 不接受分块传输编码，长度必须显式给出。
    match req.body {
        Body::Empty => {
            builder = builder.header(http::header::CONTENT_LENGTH, 0);
        }
        Body::Bytes(bytes) => {
            builder = builder
                .header(http::header::CONTENT_LENGTH, bytes.len())
                .body(bytes);
        }
        Body::Stream { stream, len } => {
            builder = builder
                .header(http::header::CONTENT_LENGTH, len)
                .body(reqwest::Body::wrap_stream(stream));
        }
    }

    builder.send().await.map_err(map_error)
}

fn map_error(e: reqwest::Error) -> TransportError {
    if e.is_timeout() {
        TransportError::Timeout
    } else if e.is_connect() {
        TransportError::Connect(describe(&e))
    } else if e.is_builder() || e.is_request() {
        TransportError::InvalidRequest(describe(&e))
    } else {
        TransportError::Io(describe(&e))
    }
}

/// reqwest 的 Display 只给一层，真正的原因在 source 链末尾。
fn describe(e: &dyn std::error::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        if !message.contains(&text) {
            message.push_str("：");
            message.push_str(&text);
        }
        source = inner.source();
    }
    message
}
