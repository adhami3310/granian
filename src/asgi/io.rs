use anyhow::Result;
use futures::{sink::SinkExt, StreamExt, TryStreamExt};
use http_body_util::BodyExt;
use hyper::{
    body,
    header::{HeaderMap, HeaderName, HeaderValue, SERVER as HK_SERVER},
    Response, StatusCode,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use std::{
    borrow::Cow,
    sync::{atomic, Arc, Mutex, RwLock},
};
use tokio::{
    fs::File,
    sync::{mpsc, oneshot, Mutex as AsyncMutex},
};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::io::ReaderStream;

use super::{
    errors::{error_flow, error_message, error_transport, UnsupportedASGIMessage},
    types::ASGIMessageType,
};
use crate::{
    conversion::BytesToPy,
    http::{response_404, HTTPRequest, HTTPResponse, HTTPResponseBody, HV_SERVER},
    runtime::{empty_future_into_py, future_into_py_futlike, future_into_py_iter, RuntimeRef},
    ws::{HyperWebsocket, UpgradeData, WSRxStream, WSTxStream},
};

const EMPTY_BYTES: Cow<[u8]> = Cow::Borrowed(b"");
const EMPTY_STRING: String = String::new();

#[pyclass(frozen, module = "granian._granian")]
pub(crate) struct ASGIHTTPProtocol {
    rt: RuntimeRef,
    tx: Mutex<Option<oneshot::Sender<HTTPResponse>>>,
    request_body: Arc<AsyncMutex<http_body_util::BodyStream<body::Incoming>>>,
    response_started: atomic::AtomicBool,
    response_chunked: atomic::AtomicBool,
    response_intent: Mutex<Option<(i16, HeaderMap)>>,
    body_tx: Mutex<Option<mpsc::Sender<Result<body::Bytes, anyhow::Error>>>>,
    flow_rx_exhausted: Arc<RwLock<bool>>,
    flow_tx_waiter: Arc<tokio::sync::Notify>,
}

impl ASGIHTTPProtocol {
    pub fn new(rt: RuntimeRef, request: HTTPRequest, tx: oneshot::Sender<HTTPResponse>) -> Self {
        Self {
            rt,
            tx: Mutex::new(Some(tx)),
            request_body: Arc::new(AsyncMutex::new(http_body_util::BodyStream::new(request.into_body()))),
            response_started: false.into(),
            response_chunked: false.into(),
            response_intent: Mutex::new(None),
            body_tx: Mutex::new(None),
            flow_rx_exhausted: Arc::new(RwLock::new(false)),
            flow_tx_waiter: Arc::new(tokio::sync::Notify::new()),
        }
    }

    #[inline(always)]
    fn send_response(&self, status: i16, headers: HeaderMap<HeaderValue>, body: HTTPResponseBody) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let mut res = Response::new(body);
            *res.status_mut() = hyper::StatusCode::from_u16(status as u16).unwrap();
            *res.headers_mut() = headers;
            let _ = tx.send(res);
        }
    }

    #[inline(always)]
    fn send_body<'p>(
        &self,
        py: Python<'p>,
        tx: mpsc::Sender<Result<body::Bytes, anyhow::Error>>,
        body: Box<[u8]>,
    ) -> PyResult<&'p PyAny> {
        future_into_py_futlike(self.rt.clone(), py, async move {
            match tx.send(Ok(body.into())).await {
                Ok(()) => Ok(()),
                Err(err) => {
                    log::warn!("ASGI transport tx error: {:?}", err);
                    error_transport!()
                }
            }
        })
    }

    pub fn tx(&self) -> Option<oneshot::Sender<HTTPResponse>> {
        self.tx.lock().unwrap().take()
    }
}

#[pymethods]
impl ASGIHTTPProtocol {
    fn receive<'p>(&self, py: Python<'p>) -> PyResult<&'p PyAny> {
        if *self.flow_rx_exhausted.read().unwrap() {
            let holder = self.flow_tx_waiter.clone();
            return future_into_py_futlike(self.rt.clone(), py, async move {
                let () = holder.notified().await;
                Python::with_gil(|py| {
                    let dict = PyDict::new(py);
                    dict.set_item(pyo3::intern!(py, "type"), pyo3::intern!(py, "http.disconnect"))?;
                    Ok(dict.to_object(py))
                })
            });
        }

        let body_ref = self.request_body.clone();
        let flow_ref = self.flow_rx_exhausted.clone();
        future_into_py_iter(self.rt.clone(), py, async move {
            let mut bodym = body_ref.lock().await;
            let body = &mut *bodym;
            let mut more_body = false;
            let chunk = match body.next().await {
                Some(chunk) => {
                    more_body = true;
                    chunk
                        .map(|buf| buf.into_data().unwrap_or_default())
                        .unwrap_or(body::Bytes::new())
                }
                _ => body::Bytes::new(),
            };
            if !more_body {
                let mut flow = flow_ref.write().unwrap();
                *flow = true;
            }

            Python::with_gil(|py| {
                let dict = PyDict::new(py);
                dict.set_item(pyo3::intern!(py, "type"), pyo3::intern!(py, "http.request"))?;
                dict.set_item(pyo3::intern!(py, "body"), BytesToPy(chunk))?;
                dict.set_item(pyo3::intern!(py, "more_body"), more_body)?;
                Ok(dict.to_object(py))
            })
        })
    }

    fn send<'p>(&self, py: Python<'p>, data: &'p PyDict) -> PyResult<&'p PyAny> {
        match adapt_message_type(data) {
            Ok(ASGIMessageType::HTTPStart) => match self.response_started.load(atomic::Ordering::Relaxed) {
                false => {
                    let mut response_intent = self.response_intent.lock().unwrap();
                    *response_intent = Some((adapt_status_code(py, data)?, adapt_headers(py, data)));
                    self.response_started.store(true, atomic::Ordering::Relaxed);
                    empty_future_into_py(py)
                }
                true => error_flow!(),
            },
            Ok(ASGIMessageType::HTTPBody) => {
                let (body, more) = adapt_body(py, data);
                match (
                    self.response_started.load(atomic::Ordering::Relaxed),
                    more,
                    self.response_chunked.load(atomic::Ordering::Relaxed),
                ) {
                    (true, false, false) => {
                        let (status, headers) = self.response_intent.lock().unwrap().take().unwrap();
                        self.send_response(
                            status,
                            headers,
                            http_body_util::Full::new(body::Bytes::from(body))
                                .map_err(|e| match e {})
                                .boxed(),
                        );
                        self.flow_tx_waiter.notify_one();
                        empty_future_into_py(py)
                    }
                    (true, true, false) => {
                        self.response_chunked.store(true, atomic::Ordering::Relaxed);
                        let (status, headers) = self.response_intent.lock().unwrap().take().unwrap();
                        let (body_tx, body_rx) = mpsc::channel::<Result<body::Bytes, anyhow::Error>>(1);
                        let body_stream = http_body_util::StreamBody::new(
                            tokio_stream::wrappers::ReceiverStream::new(body_rx).map_ok(hyper::body::Frame::data),
                        );
                        *self.body_tx.lock().unwrap() = Some(body_tx.clone());
                        self.send_response(status, headers, BodyExt::boxed(body_stream));
                        self.send_body(py, body_tx, body)
                    }
                    (true, true, true) => match &*self.body_tx.lock().unwrap() {
                        Some(tx) => {
                            let tx = tx.clone();
                            self.send_body(py, tx, body)
                        }
                        _ => error_flow!(),
                    },
                    (true, false, true) => match self.body_tx.lock().unwrap().take() {
                        Some(tx) => {
                            self.flow_tx_waiter.notify_one();
                            match body.is_empty() {
                                false => self.send_body(py, tx, body),
                                true => empty_future_into_py(py),
                            }
                        }
                        _ => error_flow!(),
                    },
                    _ => error_flow!(),
                }
            }
            Ok(ASGIMessageType::HTTPFile) => match (
                self.response_started.load(atomic::Ordering::Relaxed),
                adapt_file(py, data),
                self.tx.lock().unwrap().take(),
            ) {
                (true, Ok(file_path), Some(tx)) => {
                    let (status, headers) = self.response_intent.lock().unwrap().take().unwrap();
                    future_into_py_iter(self.rt.clone(), py, async move {
                        let res = match File::open(&file_path).await {
                            Ok(file) => {
                                let stream = ReaderStream::new(file);
                                let stream_body = http_body_util::StreamBody::new(stream.map_ok(body::Frame::data));
                                let mut res =
                                    Response::new(BodyExt::map_err(stream_body, std::convert::Into::into).boxed());
                                *res.status_mut() = StatusCode::from_u16(status as u16).unwrap();
                                *res.headers_mut() = headers;
                                res
                            }
                            Err(_) => {
                                log::info!("Cannot open file {}", &file_path);
                                response_404()
                            }
                        };
                        let _ = tx.send(res);
                        Ok(())
                    })
                }
                _ => error_flow!(),
            },
            Err(err) => Err(err.into()),
            _ => error_message!(),
        }
    }
}

pub(crate) struct WebsocketDetachedTransport {
    pub consumed: bool,
    rx: Option<WSRxStream>,
    tx: Option<WSTxStream>,
}

impl WebsocketDetachedTransport {
    pub fn new(consumed: bool, rx: Option<WSRxStream>, tx: Option<WSTxStream>) -> Self {
        Self { consumed, rx, tx }
    }

    pub async fn close(&mut self) {
        if let Some(mut tx) = self.tx.take() {
            if let Err(err) = tx.close().await {
                log::info!("Failed to close websocket with error {:?}", err);
            }
        }
        drop(self.rx.take());
    }
}

#[pyclass(frozen, module = "granian._granian")]
pub(crate) struct ASGIWebsocketProtocol {
    rt: RuntimeRef,
    tx: Mutex<Option<oneshot::Sender<WebsocketDetachedTransport>>>,
    websocket: Mutex<Option<HyperWebsocket>>,
    upgrade: Mutex<Option<UpgradeData>>,
    ws_rx: Arc<AsyncMutex<Option<WSRxStream>>>,
    ws_tx: Arc<AsyncMutex<Option<WSTxStream>>>,
    accepted: Arc<atomic::AtomicBool>,
    closed: Arc<atomic::AtomicBool>,
}

impl ASGIWebsocketProtocol {
    pub fn new(
        rt: RuntimeRef,
        tx: oneshot::Sender<WebsocketDetachedTransport>,
        websocket: HyperWebsocket,
        upgrade: UpgradeData,
    ) -> Self {
        Self {
            rt,
            tx: Mutex::new(Some(tx)),
            websocket: Mutex::new(Some(websocket)),
            upgrade: Mutex::new(Some(upgrade)),
            ws_rx: Arc::new(AsyncMutex::new(None)),
            ws_tx: Arc::new(AsyncMutex::new(None)),
            accepted: Arc::new(false.into()),
            closed: Arc::new(false.into()),
        }
    }

    #[inline(always)]
    fn accept<'p>(&self, py: Python<'p>) -> PyResult<&'p PyAny> {
        let upgrade = self.upgrade.lock().unwrap().take();
        let websocket = self.websocket.lock().unwrap().take();
        let accepted = self.accepted.clone();
        let rx = self.ws_rx.clone();
        let tx = self.ws_tx.clone();

        future_into_py_iter(self.rt.clone(), py, async move {
            if let Some(mut upgrade) = upgrade {
                if (upgrade.send().await).is_ok() {
                    if let Some(websocket) = websocket {
                        if let Ok(stream) = websocket.await {
                            let mut wtx = tx.lock().await;
                            let mut wrx = rx.lock().await;
                            let (tx, rx) = stream.split();
                            *wtx = Some(tx);
                            *wrx = Some(rx);
                            accepted.store(true, atomic::Ordering::Relaxed);
                            return Ok(());
                        }
                    }
                }
            }
            error_flow!()
        })
    }

    #[inline(always)]
    fn send_message<'p>(&self, py: Python<'p>, data: &'p PyDict) -> PyResult<&'p PyAny> {
        let transport = self.ws_tx.clone();
        let message = ws_message_into_rs(py, data);
        let closed = self.closed.clone();

        future_into_py_futlike(self.rt.clone(), py, async move {
            match message {
                Ok(message) => {
                    if let Some(ws) = &mut *(transport.lock().await) {
                        match ws.send(message).await {
                            Ok(()) => return Ok(()),
                            _ => {
                                if closed.load(atomic::Ordering::Relaxed) {
                                    log::info!("Attempted to write to a closed websocket");
                                    return Ok(());
                                }
                            }
                        };
                    };
                    error_flow!()
                }
                Err(err) => Err(err),
            }
        })
    }

    #[inline(always)]
    fn close<'p>(&self, py: Python<'p>) -> PyResult<&'p PyAny> {
        let closed = self.closed.clone();
        let ws_rx = self.ws_rx.clone();
        let ws_tx = self.ws_tx.clone();

        future_into_py_iter(self.rt.clone(), py, async move {
            match ws_tx.lock().await.take() {
                Some(tx) => {
                    closed.store(true, atomic::Ordering::Relaxed);
                    WebsocketDetachedTransport::new(true, ws_rx.lock().await.take(), Some(tx))
                        .close()
                        .await;
                    Ok(())
                }
                _ => error_flow!(),
            }
        })
    }

    fn consumed(&self) -> bool {
        self.upgrade.lock().unwrap().is_none()
    }

    pub fn tx(
        &self,
    ) -> (
        Option<oneshot::Sender<WebsocketDetachedTransport>>,
        WebsocketDetachedTransport,
    ) {
        let mut ws_rx = self.ws_rx.blocking_lock();
        let mut ws_tx = self.ws_tx.blocking_lock();
        (
            self.tx.lock().unwrap().take(),
            WebsocketDetachedTransport::new(self.consumed(), ws_rx.take(), ws_tx.take()),
        )
    }
}

#[pymethods]
impl ASGIWebsocketProtocol {
    fn receive<'p>(&self, py: Python<'p>) -> PyResult<&'p PyAny> {
        let accepted = self.accepted.clone();
        let closed = self.closed.clone();
        let transport = self.ws_rx.clone();

        future_into_py_futlike(self.rt.clone(), py, async move {
            let accepted = accepted.load(atomic::Ordering::Relaxed);
            if !accepted {
                return Python::with_gil(|py| {
                    let dict = PyDict::new(py);
                    dict.set_item(pyo3::intern!(py, "type"), pyo3::intern!(py, "websocket.connect"))?;
                    Ok(dict.to_object(py))
                });
            }

            if let Some(ws) = &mut *(transport.lock().await) {
                while let Some(recv) = ws.next().await {
                    match recv {
                        Ok(Message::Ping(_)) => continue,
                        Ok(message @ Message::Close(_)) => {
                            closed.store(true, atomic::Ordering::Relaxed);
                            return ws_message_into_py(message);
                        }
                        Ok(message) => return ws_message_into_py(message),
                        _ => break,
                    }
                }
            }
            error_flow!()
        })
    }

    fn send<'p>(&self, py: Python<'p>, data: &'p PyDict) -> PyResult<&'p PyAny> {
        match adapt_message_type(data) {
            Ok(ASGIMessageType::WSAccept) => self.accept(py),
            Ok(ASGIMessageType::WSClose) => self.close(py),
            Ok(ASGIMessageType::WSMessage) => self.send_message(py, data),
            _ => future_into_py_iter::<_, _, PyErr>(self.rt.clone(), py, async { error_message!() }),
        }
    }
}

#[inline(never)]
fn adapt_message_type(message: &PyDict) -> Result<ASGIMessageType, UnsupportedASGIMessage> {
    match message.get_item("type") {
        Ok(Some(item)) => {
            let message_type: &str = item.extract()?;
            match message_type {
                "http.response.start" => Ok(ASGIMessageType::HTTPStart),
                "http.response.body" => Ok(ASGIMessageType::HTTPBody),
                "http.response.pathsend" => Ok(ASGIMessageType::HTTPFile),
                "websocket.accept" => Ok(ASGIMessageType::WSAccept),
                "websocket.close" => Ok(ASGIMessageType::WSClose),
                "websocket.send" => Ok(ASGIMessageType::WSMessage),
                _ => error_message!(),
            }
        }
        _ => error_message!(),
    }
}

#[inline(always)]
fn adapt_status_code(py: Python, message: &PyDict) -> Result<i16, UnsupportedASGIMessage> {
    match message.get_item(pyo3::intern!(py, "status"))? {
        Some(item) => Ok(item.extract()?),
        _ => error_message!(),
    }
}

#[inline(always)]
fn adapt_headers(py: Python, message: &PyDict) -> HeaderMap {
    let mut ret = HeaderMap::new();
    ret.insert(HK_SERVER, HV_SERVER);
    match message.get_item(pyo3::intern!(py, "headers")) {
        Ok(Some(item)) => {
            let accum: Vec<Vec<&[u8]>> = item.extract().unwrap_or(Vec::new());
            for tup in &accum {
                if let (Ok(key), Ok(val)) = (HeaderName::from_bytes(tup[0]), HeaderValue::from_bytes(tup[1])) {
                    ret.append(key, val);
                }
            }
            ret
        }
        _ => ret,
    }
}

#[inline(always)]
fn adapt_body(py: Python, message: &PyDict) -> (Box<[u8]>, bool) {
    let body = match message.get_item(pyo3::intern!(py, "body")) {
        Ok(Some(item)) => item.extract().unwrap_or(EMPTY_BYTES),
        _ => EMPTY_BYTES,
    };
    let more = match message.get_item(pyo3::intern!(py, "more_body")) {
        Ok(Some(item)) => item.extract().unwrap_or(false),
        _ => false,
    };
    (body.into(), more)
}

#[inline(always)]
fn adapt_file(py: Python, message: &PyDict) -> PyResult<String> {
    match message.get_item(pyo3::intern!(py, "path"))? {
        Some(item) => item.extract(),
        _ => error_flow!(),
    }
}

#[inline(always)]
fn ws_message_into_rs(py: Python, message: &PyDict) -> PyResult<Message> {
    match (
        message.get_item(pyo3::intern!(py, "bytes"))?,
        message.get_item(pyo3::intern!(py, "text"))?,
    ) {
        (Some(item), None) => {
            let data: Cow<[u8]> = item.extract().unwrap_or(EMPTY_BYTES);
            Ok(data[..].into())
        }
        (None, Some(item)) => Ok(Message::Text(item.extract().unwrap_or(EMPTY_STRING))),
        (Some(itemb), Some(itemt)) => match (itemb.extract().unwrap_or(None), itemt.extract().unwrap_or(None)) {
            (Some(msgb), None) => Ok(Message::Binary(msgb)),
            (None, Some(msgt)) => Ok(Message::Text(msgt)),
            _ => error_flow!(),
        },
        _ => {
            error_flow!()
        }
    }
}

#[inline(always)]
fn ws_message_into_py(message: Message) -> PyResult<PyObject> {
    match message {
        Message::Binary(message) => Python::with_gil(|py| {
            let dict = PyDict::new(py);
            dict.set_item(pyo3::intern!(py, "type"), pyo3::intern!(py, "websocket.receive"))?;
            dict.set_item(pyo3::intern!(py, "bytes"), PyBytes::new(py, &message[..]))?;
            Ok(dict.to_object(py))
        }),
        Message::Text(message) => Python::with_gil(|py| {
            let dict = PyDict::new(py);
            dict.set_item(pyo3::intern!(py, "type"), pyo3::intern!(py, "websocket.receive"))?;
            dict.set_item(pyo3::intern!(py, "text"), message)?;
            Ok(dict.to_object(py))
        }),
        Message::Close(frame) => Python::with_gil(|py| {
            let close_code: u16 = match frame {
                Some(frame) => frame.code.into(),
                _ => 1005,
            };
            let dict = PyDict::new(py);
            dict.set_item(pyo3::intern!(py, "type"), pyo3::intern!(py, "websocket.disconnect"))?;
            dict.set_item(pyo3::intern!(py, "code"), close_code)?;
            Ok(dict.to_object(py))
        }),
        v => {
            log::warn!("Unsupported websocket message received {:?}", v);
            error_flow!()
        }
    }
}
