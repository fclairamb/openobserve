use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use actix_web::{get, web, Error, HttpRequest, HttpResponse};
use actix_ws::{Message, Session};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::ws_utils::{
    get_req_id_from_trace_id, get_ws_session_by_req_id, get_ws_trace_id_query_object,
    insert_in_ws_session_by_req_id, insert_in_ws_trace_id_query_object, insert_trace_id_to_req_id,
    remove_from_ws_session_by_req_id, remove_trace_id_from_cache, WSClientMessage,
    WEBSOCKET_MSG_CHAN,
};
use crate::handler::http::request::websocket::ws_utils::{
    print_req_id_to_trace_id, print_sessions, WebSocketMessageType,
};

/// Spawns a background task that periodically checks the aliveness of the WebSocket session.
///
/// The task will ping the session every 5 seconds. If the session does not respond with a pong
/// within 10 seconds, the connection is considered dead and the session is closed.
///
/// The `alive` parameter is a shared mutex that tracks the last time a pong was received from the
/// client. This is used to determine if the client is still responsive.
async fn aliveness_check(
    user_session_id: String,
    mut session: Session,
    alive: Arc<Mutex<Instant>>,
) {
    actix_web::rt::spawn(async move {
        let mut interval = actix_web::rt::time::interval(Duration::from_secs(10));

        loop {
            interval.tick().await;
            if session.ping(b"").await.is_err() {
                log::error!("Unable to send ping to {user_session_id}");
            }

            let client_timedout =
                Instant::now().duration_since(*alive.lock().await) > Duration::from_secs(30);
            if client_timedout {
                log::error!(
                    "{user_session_id} is not responding even after 30s, closing connection"
                );
                let _ = session.close(None).await;
                remove_from_ws_session_by_req_id(user_session_id).await;
                break;
            }
        }
    });
}

async fn websocket_handler(
    mut session: actix_ws::Session,
    alive: Arc<Mutex<Instant>>,
    request_id: String,
    mut msg_stream: actix_ws::MessageStream,
) {
    let mut receiver = WEBSOCKET_MSG_CHAN.1.resubscribe();

    loop {
        tokio::select! {
            Some(msg) = msg_stream.next() => {
                match msg {
                    Ok(Message::Ping(bytes)) => {
                        if session.pong(&bytes).await.is_err() {
                            log::info!("Failed to send pong, bailing");
                            return;
                        }
                    }
                    Ok(Message::Text(msg)) => {
                        match serde_json::from_str::<WSClientMessage>(&msg){
                            Ok(client_msg) => {
                                log::info!("Received trace_registration msg: {:?}", client_msg);
                                insert_trace_id_to_req_id(client_msg.trace_id().to_string(), request_id.clone()).await;
                                match client_msg{
                                    WSClientMessage::Search{trace_id, query, .. } => {
                                        insert_in_ws_trace_id_query_object(trace_id, query.clone()).await;
                                    }
                                    _ => {}
                                };
                                print_req_id_to_trace_id().await;
                            }
                            Err(e) => {
                                log::error!("Failed to parse message incoming message from ws client: {:?} {:?}", msg, e);
                            }
                        }
                    }
                    Ok(Message::Close(reason)) => {
                        let _ = session.close(reason).await;
                        log::info!("Got close, bailing");
                        return;
                    }
                    Ok(Message::Continuation(_)) => {
                        let _ = session.close(None).await;
                        log::info!("Got continuation, bailing");
                        return;
                    }
                    Ok(Message::Pong(_)) => {
                        *alive.lock().await = Instant::now();
                    }
                    _ => (),
                };
            }
            Ok(ws_msg) = receiver.recv() => {
                print_req_id_to_trace_id().await;
                print_sessions().await;

                let trace_id = ws_msg.trace_id();
                log::info!("Received ws message: {:?}", ws_msg);
                let request_id = get_req_id_from_trace_id(trace_id).await;
                log::info!("request_id: {:?} trace_id: {}", request_id, trace_id);
                if let Some(req_id) = request_id{
                    log::info!("Inside req_id: {}", req_id);
                    let ws_session = get_ws_session_by_req_id(&req_id).await;
                    log::info!("Inside get_ws_session_by_req_id");

                    if let Some(mut ws_session) = ws_session {
                        log::info!("Found websocket session for user_id: {} trace_id: {}", ws_msg.user_id, trace_id);
                        let data = match ws_msg.payload{
                            WebSocketMessageType::QueryEnqueued { .. } => {
                                if let Some(query_payload) = get_ws_trace_id_query_object(ws_msg.trace_id()).await{
                                    let mut new_payload = ws_msg.clone();
                                    new_payload.update_payload(query_payload);
                                    new_payload
                                }else{
                                    ws_msg.clone()
                                }
                            },
                            _ => ws_msg.clone(),
                        };
                        let payload = serde_json::to_string(&data).unwrap();
                        if let Err(e) = ws_session.text(payload).await {
                            log::error!("Error sending message: {}", e);
                            break;
                        }
                        log::info!("Sent message to the user, removing this trace_id from cache");
                        let _ = remove_trace_id_from_cache(trace_id).await;
                        break;
                    }
                }
                log::error!("No websocket session found for user_id: {} trace_id: {}", ws_msg.user_id, trace_id);
            }
            else =>{
                log::info!("Break the look because no message received");
                 break
                },
        }
    }

    log::info!("Breaking the loop, everything is done");
    let _ = session.close(None).await;
}

#[derive(Serialize, Deserialize, Clone, Debug, Hash)]
pub struct WSQueryParam {
    pub request_id: String,
}

#[get("/ws/{user_id}")]
pub async fn websocket(
    user_id: web::Path<String>,
    req: HttpRequest,
    stream: web::Payload,
    query: web::Query<WSQueryParam>,
) -> Result<HttpResponse, Error> {
    let (res, session, msg_stream) = actix_ws::handle(&req, stream)?;

    let user_id = user_id.into_inner();
    let request_id = query.request_id.clone();

    log::info!(
        "Got websocket request for user_id: {} request_id {}",
        user_id,
        request_id
    );

    insert_in_ws_session_by_req_id(request_id.clone(), session.clone()).await;

    let alive = Arc::new(Mutex::new(Instant::now()));
    let alive1 = alive.clone();
    let session1 = session.clone();
    let req_id = request_id.clone();
    actix_web::rt::spawn(async move { aliveness_check(req_id, session1, alive1).await });

    // Spawn the handler
    actix_web::rt::spawn(websocket_handler(
        session.clone(),
        alive.clone(),
        request_id,
        msg_stream,
    ));

    // Return the response
    Ok(res)
}