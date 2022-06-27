use crate::flow::{
    connection, file_identifier::FileIdentifierNames, Error, Flow, FlowFuture, FlowMessage, Peer,
    Result,
};
use crate::services::RequestRouter;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tower::Service;

use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::task::{Context, Poll};

const MAX_CONNECTIONS: usize = 250;
const MAX_REQUESTS: usize = MAX_CONNECTIONS * 2;

/// Takes FlowMessages from multiple threads and writes them to a ConnectionWriter in a single-threaded way
async fn sender<C: 'static + AsyncWrite + Unpin + Send>(
    mut response_rx: tokio::sync::mpsc::UnboundedReceiver<FlowMessage>,
    mut writer: connection::ConnectionWriter<C>,
) -> Result<()> {
    while let Some(message) = response_rx.recv().await {
        writer.write_frame(message.frame).await?;
        loop {
            match response_rx.try_recv() {
                Ok(message) => {
                    writer.write_frame(message.frame).await.unwrap();
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    writer.flush().await.unwrap();
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// Takes FlowMessages from a single-threaded connection reader, and runs them in parallel by spawning concurrent tasks.
async fn receiver<C>(
    peer: SocketAddr,
    svc: Arc<RequestRouter>,
    mut reader: connection::ConnectionReader<C>,
) -> Result<()>
where
    C: 'static + AsyncRead + Unpin + Send,
{
    // XXX conncurrency limit per process or per connection?  This is per connection.
    let svc_clone = svc.clone();
    let mut limit_svc =
        tower::limit::concurrency::ConcurrencyLimit::new(svc_clone.deref(), MAX_REQUESTS);
    while let Some(frame) = reader.read_frame().await? {
        let request = FlowMessage::new(
            Flow {
                dst: Peer::Local(None),
                src: Peer::Remote(peer),
            },
            frame,
        )?;
        // poll_ready and call must be invoked atomically
        // we could read this before reading the next frame to prevent the next, throttled request from consuming
        // TCP buffers.  However, keeping one extra frame around (above the limit) is unlikely to matter in terms
        // of memory usage, but it helps interleave network + processing time.
        futures_util::future::poll_fn(|cx| limit_svc.poll_ready(cx)).await?;
        let fut = limit_svc.call(request);
        let svc = svc.clone();
        tokio::spawn(async move {
            // the real work happens in await, anyway
            let response = fut.await.unwrap();
            match response {
                Some(response) => {
                    // println!("Response: {:?}", response);
                    svc.deref().call(response).await.unwrap();
                }
                None => (),
            };
        });
    }
    Ok(())
}

fn spawn_receiver<C>(
    peer: SocketAddr,
    request_router: Arc<RequestRouter>,
    reader: connection::ConnectionReader<C>,
    permit: OwnedSemaphorePermit,
) where
    C: 'static + AsyncRead + Unpin + Send,
{
    tokio::spawn(async move {
        match receiver(peer, request_router, reader).await {
            Ok(_) => {
                println!("clean shutdown!");
            }
            Err(e) => {
                println!("Unexpected error from receiver! {:?}", e)
            }
        }
        drop(permit);
    });
}

fn spawn_sender<C>(
    response_rx: mpsc::UnboundedReceiver<FlowMessage>,
    writer: connection::ConnectionWriter<C>,
) where
    C: 'static + AsyncWrite + Unpin + Send,
{
    tokio::spawn(async move {
        match sender(response_rx, writer).await {
            Ok(_) => {}
            Err(e) => {
                println!("Unexpected error from sender! {:?}", e);
            }
        }
        // TODO: Connection teardown logic?
    });
}

pub struct ConnectionHandler {
    pub peer: SocketAddr,
    pub fit: FileIdentifierNames,
    pub response_tx: mpsc::UnboundedSender<FlowMessage>,
    pub request_router: Arc<RequestRouter>,
}

impl std::fmt::Debug for ConnectionHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("ConnectionHandler")
            .field("peer", &self.peer)
            .finish()
    }
}

impl ConnectionHandler {
    async fn new(
        socket: (TcpStream, SocketAddr),
        permit: OwnedSemaphorePermit,
        request_router: Arc<RequestRouter>,
    ) -> Result<Arc<Self>> {
        let (stream, peer) = socket;
        // TODO: Backpressure?
        let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel::<FlowMessage>();
        let connection_handler = ConnectionHandler {
            peer,
            fit: FileIdentifierNames::new().unwrap(),
            response_tx,
            request_router,
        };
        let (reader, writer, connect_packet) = connection::new(stream).await?;
        // TODO: Check protocol compatibility, create object w/ enough info to allow request routing
        println!("{} {:x?}", peer, connect_packet);
        let connection_handler = Arc::new(connection_handler);
        spawn_sender(response_rx, writer);
        spawn_receiver(
            connection_handler.peer,
            connection_handler.request_router.clone(),
            reader,
            permit,
        );
        Ok(connection_handler)
    }

    pub async fn new_outgoing_connection(
        saddr: SocketAddr,
        request_router: Arc<RequestRouter>,
    ) -> Result<Arc<ConnectionHandler>> {
        let conn = TcpStream::connect(saddr).await?;
        let limit_connections = Arc::new(Semaphore::new(1));
        let permit = limit_connections.clone().acquire_owned().await?;
        ConnectionHandler::new((conn, saddr), permit, request_router).await
    }

    async fn listener(
        bind: TcpListener,
        limit_connections: Arc<Semaphore>,
        tx: mpsc::Sender<Arc<ConnectionHandler>>,
        request_router: Arc<RequestRouter>,
    ) -> Result<()> {
        loop {
            let permit = limit_connections.clone().acquire_owned().await?;
            let socket = bind.accept().await?;
            tx.send(ConnectionHandler::new(socket, permit, request_router.clone()).await?)
                .await?; // Send will return error if the Receiver has been close()'ed.
        }
    }

    pub async fn new_listener(
        addr: &str,
        request_router: Arc<RequestRouter>,
    ) -> Result<mpsc::Receiver<Arc<ConnectionHandler>>> {
        let bind = TcpListener::bind(addr).await?;
        let limit_connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let (tx, rx) = mpsc::channel(100);
        tokio::spawn(Self::listener(bind, limit_connections, tx, request_router));
        Ok(rx)
    }
    fn handle_req(&self, request: FlowMessage) -> Result<Option<FlowFuture>> {
        request.validate()?;
        self.response_tx.send(request)?;
        Ok(None)
    }
}

impl Service<FlowMessage> for &ConnectionHandler {
    type Response = Option<FlowMessage>;
    type Error = Error;
    type Future = FlowFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: FlowMessage) -> Self::Future {
        match self.handle_req(req) {
            Ok(Some(fut)) => fut,
            Ok(None) => Box::pin(async move { Ok(None) }),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
}
