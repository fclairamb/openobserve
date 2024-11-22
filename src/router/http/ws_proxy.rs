use actix::prelude::*;
use actix_web_actors::ws;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite};

#[derive(Message)]
#[rtype(result = "()")]
pub struct WebSocketMessageWrapper(ws::Message);

// Define your WebSocket actor
pub struct CustomWebSocketHandlers {
    pub url: String,
    pub tx: mpsc::UnboundedSender<ws::Message>,
}

fn from_actix_message(msg: ws::Message) -> tungstenite::Message {
    match msg {
        ws::Message::Text(text) => tungstenite::Message::Text(text.to_string()),
        ws::Message::Binary(bin) => tungstenite::Message::Binary(bin.to_vec()),
        ws::Message::Ping(msg) => tungstenite::Message::Ping(msg.to_vec()),
        ws::Message::Pong(msg) => tungstenite::Message::Pong(msg.to_vec()),
        ws::Message::Close(None) => {
            log::info!(
                "[WebSocketProxy] Received a Message::Close from internal client, closing connection to proxied server"
            );
            tungstenite::Message::Close(None)
        }
        _ => {
            log::info!("[WebSocketProxy] Unsupported message type");
            tungstenite::Message::Close(None)
        }
    }
}

fn from_tungstenite_msg_to_actix_msg(msg: tungstenite::Message) -> ws::Message {
    match msg {
        tungstenite::Message::Text(text) => ws::Message::Text(text.into()),
        tungstenite::Message::Binary(bin) => ws::Message::Binary(bin.into()),
        tungstenite::Message::Ping(msg) => ws::Message::Ping(msg.into()),
        tungstenite::Message::Pong(msg) => ws::Message::Pong(msg.into()),
        tungstenite::Message::Close(None) => ws::Message::Close(None),
        _ => {
            log::info!("[WebSocketProxy] Unsupported message type");
            ws::Message::Close(None)
        }
    }
}

impl Actor for CustomWebSocketHandlers {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        log::info!(
            "[WebSocketProxy] Started WebSocket connection to {}",
            self.url
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        self.tx = tx;
        let addr = ctx.address();
        let url_to_connect = self.url.clone();

        tokio::spawn(async move {
            let (ws_stream, _) = connect_async(&url_to_connect)
                .await
                .expect("Failed to connect");
            let (mut ws_sink, mut ws_stream) = ws_stream.split();

            tokio::spawn(async move {
                while let Some(msg) = rx.recv().await {
                    log::info!(
                        "[WebSocketProxy] Received message from the original websocket actor: {msg:?}"
                    );
                    ws_sink
                        .send(from_actix_message(msg))
                        .await
                        .expect("Failed to send message");
                }
            });

            while let Some(Ok(msg)) = ws_stream.next().await {
                log::info!(
                    "[WebSocketProxy] Should have sent to the original websocket actor to send back to client: {msg:?}"
                );
                addr.do_send(WebSocketMessageWrapper(from_tungstenite_msg_to_actix_msg(
                    msg,
                )));
            }
        });
    }
}

impl Handler<WebSocketMessageWrapper> for CustomWebSocketHandlers {
    type Result = ();

    fn handle(&mut self, msg: WebSocketMessageWrapper, ctx: &mut Self::Context) {
        log::info!(
            "[WebSocketProxy] Received message from the client: {:?}",
            &msg.0
        );
        match msg.0 {
            ws::Message::Text(text) => {
                log::info!("[WebSocketProxy] Text message received: {}", text);
                ctx.text(text);
            }
            ws::Message::Binary(bin) => {
                log::info!("[WebSocketProxy] Binary message received: {:?}", bin);
                ctx.binary(bin);
            }
            ws::Message::Ping(msg) => {
                log::info!("[WebSocketProxy] Ping message received: {:?}", msg);
                ctx.ping(&msg);
            }
            ws::Message::Pong(msg) => {
                log::info!("[WebSocketProxy] Pong message received: {:?}", msg);
                ctx.pong(&msg);
            }
            ws::Message::Close(reason) => {
                log::info!("[WebSocketProxy] Close message received: {:?}", reason);
                ctx.close(reason);
            }
            _ => {
                log::error!("Unsupported message type");
            }
        }
    }
}

impl actix::StreamHandler<Result<ws::Message, ws::ProtocolError>> for CustomWebSocketHandlers {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, _ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Text(text)) => {
                self.tx
                    .send(ws::Message::Text(text))
                    .expect("Failed to forward message");
            }
            Ok(ws::Message::Binary(bin)) => {
                self.tx
                    .send(ws::Message::Binary(bin))
                    .expect("Failed to forward message");
            }
            _ => (),
        }
    }
}
