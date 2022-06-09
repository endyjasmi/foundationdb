use crate::flow::connection;
use crate::flow::file_identifier;
use crate::flow::frame;
use crate::flow::frame::Frame;
use crate::flow::uid;
use crate::flow::Result;

use std::future::Future; //
use std::sync::Arc;
use std::task::{Context, Poll}; // , Poll, Stream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower::Service;

pub mod network_test;
pub mod ping_request;

pub struct FlowRequest {
    frame: frame::Frame,
    parsed_file_identifier: file_identifier::ParsedFileIdentifier,
}

pub struct FlowResponse {
    frame: frame::Frame,
}

struct Listener {
    listener: TcpListener,
    limit_connections: Arc<Semaphore>,
    limit_requests: Arc<Semaphore>,
    // TODO: Shutdown?
}

const MAX_CONNECTIONS: usize = 250;
const MAX_REQUESTS: usize = MAX_CONNECTIONS * 2;

#[derive(Clone)]
struct Svc {}

async fn handle_req(request: FlowRequest) -> Result<Option<FlowResponse>> {
    request.frame.validate()?;
    Ok(match request.frame.token.get_well_known_endpoint() {
        Some(uid::WLTOKEN::PingPacket) => ping_request::handle(request).await?,
        Some(uid::WLTOKEN::ReservedForTesting) => network_test::handle(request).await?,
        Some(wltoken) => {
            println!(
                "Got unhandled request for well-known enpoint {:?}: {:x?} {:04x?}",
                wltoken, request.frame.token, request.parsed_file_identifier
            );
            None
        }
        None => {
            println!(
                "Message not destined for well-known endpoint: {:x?}",
                request.frame
            );
            println!(
                "{:x?} {:04x?}",
                request.frame.token, request.parsed_file_identifier
            );
            None
        }
    })
}

impl Service<FlowRequest> for Svc {
    // type Future: Future<Output = std::result::Result<Self::Response, Self::Error>>;
    type Response = Option<FlowResponse>;
    type Error = super::flow::Error;
    type Future = std::pin::Pin<
        Box<dyn Send + Future<Output = std::result::Result<Self::Response, Self::Error>>>,
    >; // XXX get rid of box!

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<super::flow::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: FlowRequest) -> Self::Future {
        Box::pin(handle_req(req))
    }
}

async fn handle_frame(
    frame: frame::Frame,
    parsed_file_identifier: file_identifier::ParsedFileIdentifier,
    response_tx: tokio::sync::mpsc::Sender<Frame>,
) -> Result<()> {
    let request = FlowRequest {
        frame,
        parsed_file_identifier,
    };

    match handle_req(request).await? {
        Some(response) => response_tx.send(response.frame).await?,
        None => (),
    };
    Ok(())
}

fn spawn_sender<C: 'static + AsyncWrite + Unpin + Send>(
    mut response_rx: tokio::sync::mpsc::Receiver<Frame>,
    mut writer: connection::ConnectionWriter<C>,
) {
    tokio::spawn(async move {
        while let Some(frame) = response_rx.recv().await {
            writer.write_frame(frame).await.unwrap(); //XXX unwrap!
            loop {
                match response_rx.try_recv() {
                    Ok(frame) => {
                        writer.write_frame(frame).await.unwrap();
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        writer.flush().await.unwrap();
                        break;
                    }
                    Err(e) => {
                        println!("Unexpected error! {:?}", e);
                        return;
                    }
                }
            }
        }
    });
}

pub async fn hello_tower() -> Result<()> {
    let bind = TcpListener::bind(&format!("127.0.0.1:{}", 6789)).await?;
    // let maker = ServiceBuilder::new().concurreny_limit(5).service(MakeSvc);
    // let server = Server::new(maker);
    let limit_connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        let permit = limit_connections.clone().acquire_owned().await?;
        let socket = bind.accept().await?;
        println!("tower got socket from {}", socket.1);
        let (mut reader, writer) = connection::new(socket.0);
        // writer.send_connect_packet().await?;
        // println!("sent ConnectPacket");

        // Bound the number of backlogged messages to a given remote endpoint.  This is
        // set to the process-wide MAX_REQUESTS / 10 so that a few backpressuring receivers
        // can't consume all the request slots for this process.
        let (response_tx, response_rx) = tokio::sync::mpsc::channel::<Frame>(MAX_REQUESTS / 10);
        spawn_sender(response_rx, writer);

        tokio::spawn(async move {
            let file_identifier_table = file_identifier::FileIdentifierNames::new()?;
            let mut svc = tower::limit::concurrency::ConcurrencyLimit::new(Svc {}, MAX_REQUESTS);
            loop {
                match reader.read_frame().await? {
                    None => {
                        println!("clean shutdown!");
                        break;
                    }
                    Some(frame) => {
                        if frame.payload.len() < 8 {
                            println!("Frame is too short! {:x?}", frame);
                            continue;
                        }
                        let file_identifier = frame.peek_file_identifier()?;
                        let parsed_file_identifier =
                            file_identifier_table.from_id(file_identifier)?;
                        let request = FlowRequest {
                            frame,
                            parsed_file_identifier,
                        };
                        let response_tx = response_tx.clone();
                        // poll_ready and call must be invoked atomically
                        // we could read this before reading the next frame to prevent the next, throttled request from consuming
                        // TCP buffers.  However, keeping one extra frame around (above the limit) is unlikely to matter in terms
                        // of memory usage, but it helps interleave network + processing time.
                        futures_util::future::poll_fn(|cx| svc.poll_ready(cx)).await?;
                        let fut = svc.call(request);
                        tokio::spawn(async move {
                            // the real work happens in await, anyway
                            let response = fut.await.unwrap();
                            match response {
                                Some(response) => response_tx.send(response.frame).await.unwrap(),
                                None => (),
                            };
                        });
                    }
                }
            }
            drop(permit);
            Ok::<(), crate::flow::Error>(())
        });
    }
}

async fn handle_connection<C: 'static + AsyncRead + AsyncWrite + Unpin + Send>(
    file_identifier_table: &file_identifier::FileIdentifierNames,
    limit_requests: Arc<Semaphore>,
    conn: C,
) -> Result<()> {
    let (mut reader, writer) = connection::new(conn);

    // Bound the number of backlogged messages to a given remote endpoint.  This is
    // set to the process-wide MAX_REQUESTS / 10 so that a few backpressuring receivers
    // can't consume all the request slots for this process.
    let (response_tx, response_rx) = tokio::sync::mpsc::channel::<Frame>(MAX_REQUESTS / 10);
    spawn_sender(response_rx, writer);

    loop {
        let response_tx = response_tx.clone();
        let limit_requests = limit_requests.clone();

        match reader.read_frame().await? {
            None => {
                println!("clean shutdown!");
                break;
            }
            Some(frame) => {
                if frame.payload.len() < 8 {
                    println!("Frame is too short! {:x?}", frame);
                    continue;
                }
                let file_identifier = frame.peek_file_identifier()?;
                let parsed_file_identifier = file_identifier_table.from_id(file_identifier)?;
                limit_requests.acquire().await.unwrap().forget();
                tokio::spawn(async move {
                    match handle_frame(frame, parsed_file_identifier, response_tx).await {
                        Ok(()) => (),
                        Err(e) => println!("Error: {:?}", e),
                    };
                    limit_requests.add_permits(1);
                });
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub async fn hello() -> Result<()> {
    let listener = TcpListener::bind(&format!("127.0.0.1:{}", 6789)).await?;
    let server = Listener {
        listener,
        limit_connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        limit_requests: Arc::new(Semaphore::new(MAX_REQUESTS)),
    };

    println!("listening");

    loop {
        let permit = server.limit_connections.clone().acquire_owned().await?;
        // .unwrap();
        let socket = server.listener.accept().await?;
        println!("got socket from {}", socket.1);
        let limit_requests = server.limit_requests.clone();
        tokio::spawn(async move {
            let file_identifier_table = file_identifier::FileIdentifierNames::new()?;
            match handle_connection(&file_identifier_table, limit_requests, socket.0).await {
                Ok(()) => {
                    println!("Clean connection shutdown!");
                }
                Err(e) => {
                    println!("Unclean connnection shutdown: {:?}", e);
                }
            }
            drop(permit);
            Ok::<(), crate::flow::Error>(())
        });
    }
}