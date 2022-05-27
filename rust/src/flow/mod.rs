// Implementation of the flow network protocol.  See flow_transport.md for more information.
use bytes::{Buf, BytesMut};
use std::io::Cursor;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

// TODO
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

mod connection;
mod frame;
mod uid;

// #[allow(non_snake_case)]
#[path = "../../target/flatbuffers/PingRequest_generated.rs"]
mod ping_request;

struct Listener {
    listener: TcpListener,
    limit_connections: Arc<Semaphore>,
    // TODO: Shutdown?
}

const MAX_CONNECTIONS: usize = 250;

pub async fn hello() -> Result<()> {
    let listener = TcpListener::bind(&format!("127.0.0.1:{}", 6789)).await?;
    let server = Listener {
        listener,
        limit_connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
    };

    println!("listening");

    loop {
        server.limit_connections.acquire().await.unwrap().forget();
        let socket = server.listener.accept().await?;
        println!("got socket from {}", socket.1);
        let mut conn = connection::Connection::new(socket.0);
        conn.send_connect_packet().await?;
        println!("sent ConnectPacket");
        loop {
            let frame = conn.read_frame().await?;
            // println!("{:?}", frame);
            match frame {
                None => {
                    println!("clean shutdown!");
                    break;
                }
                Some(frame) => match frame.token.get_well_known_endpoint() {
                    Some(uid::WLTOKEN::PingPacket) => {
                        println!("Ping        payload: {:x?}", frame);
                        let fake_root = ping_request::root_as_fake_root(&frame.payload[..])?;
                        println!("FakeRoot: {:x?}", fake_root);
                        let reply = fake_root.ping_request().unwrap().reply_promise().unwrap();
                        let mut builder = flatbuffers::FlatBufferBuilder::with_capacity(1024);
                        let reply_buf = ping_request::ReplyPromise::create(
                            &mut builder,
                            &ping_request::ReplyPromiseArgs {
                                uid: Some(reply.uid().unwrap()),
                            },
                        );
                        builder.finish(reply_buf, None);
                        println!("reply: {:x?}", builder.finished_data());
                        let uid = reply.uid().unwrap();
                        let token = uid::UID {
                            uid: [uid.first(), uid.second()],
                        };
                        let frame = frame::Frame {
                            token,
                            payload: Vec::new(),
                        };
                        conn.write_frame(frame).await?;
                    }

                    Some(uid::WLTOKEN::NetworkTest) => {
                        println!("NetworkTest payload: {:x?}", frame);
                    }
                    Some(uid::WLTOKEN::EndpointNotFound) => {
                        println!("EndpointNotFound payload: {:x?}", frame);
                    }
                    Some(uid::WLTOKEN::AuthTenant) => {
                        println!("AuthTenant payload: {:x?}", frame);
                    }
                    Some(uid::WLTOKEN::UnauthorizedEndpoint) => {
                        println!("UnauthorizedEndpoint payload: {:x?}", frame);
                    }
                    None => {
                        println!("Message not destined for well-known endpoint: {:x?}", frame);
                    }
                },
            }
        }
    }
}

// #[test]
// fn test_uid() -> Result<()> {
//     let s = "0123456789abcdeffedcba9876543210";
//     let uid = uid::UID::from_string(s)?;
//     let uid_s = uid.to_string();
//     assert_eq!(uid_s, s);
//     let uid2 = uid::UID::from_string(&uid_s)?;
//     assert_eq!(uid, uid2);
//     assert_eq!(uid.to_u128(), 0x0123456789abcdeffedcba9876543210);
//     Ok(())
// }