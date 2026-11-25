//! Handler machinery for server-initiated requests.
//!
//! Everything here is generated from the shared server-request table in
//! [`crate::events`] (`server_request_table!`): the handler storage, the
//! `set_*`/`clear_*` registration methods, the typed `respond_*` wrappers,
//! and the auto-dispatch used by the inbound loop.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;
use serde_json::json;
use tokio::sync::RwLock;

use super::{CodexClient, Inner};
use crate::error::{ClientError, RPC_ERROR_CODE_HANDLER_FAILED};
use crate::events::{ServerEvent, ServerRequestEvent, server_request_table};
use crate::protocol::server_requests as sr;
use crate::protocol::shared::RequestId;

type HandlerFuture<R> = Pin<Box<dyn Future<Output = Result<R, ClientError>> + Send>>;
type Handler<P, R> = Arc<dyn Fn(P) -> HandlerFuture<R> + Send + Sync>;

/// Expands the server-request table into handler storage, registration
/// methods, respond wrappers, and the auto-dispatch function.
macro_rules! define_server_request_handlers {
    ( $( $variant:ident {
        method: $method:literal,
        params: $params:ident,
        response: $response:ident,
        handler: $handler:ident,
        set: $set:ident,
        clear: $clear:ident,
        respond: $respond:ident,
        context: $context:literal,
    } ),+ $(,)? ) => {
        /// One optional async handler per server-initiated request type.
        #[derive(Default)]
        pub(super) struct ServerRequestHandlers {
            $( $handler: RwLock<Option<Handler<sr::$params, sr::$response>>>, )+
        }

        impl CodexClient {
            $(
                pub async fn $set<F, Fut>(&self, handler: F)
                where
                    F: Fn(sr::$params) -> Fut + Send + Sync + 'static,
                    Fut: Future<Output = Result<sr::$response, ClientError>> + Send + 'static,
                {
                    let wrapped: Handler<sr::$params, sr::$response> =
                        Arc::new(move |params| Box::pin(handler(params)));
                    *self.inner.server_request_handlers.$handler.write().await = Some(wrapped);
                }

                pub async fn $clear(&self) {
                    *self.inner.server_request_handlers.$handler.write().await = None;
                }

                pub async fn $respond(
                    &self,
                    id: RequestId,
                    response: sr::$response,
                ) -> Result<(), ClientError> {
                    self.respond_server_request(id, response).await
                }
            )+
        }

        /// Starts the registered handler for `request`, if any, as its own
        /// task. The task sends the handler's result (or error) back over the
        /// transport. A handler may wait for a person, so the inbound loop
        /// never waits for it: it goes on reading notifications, responses
        /// and later requests. Returns `false` when no handler is registered
        /// so the request is published as an event.
        pub(super) async fn try_auto_handle_server_request(
            inner: &Arc<Inner>,
            request: &ServerRequestEvent,
        ) -> bool {
            match request {
                $(
                    ServerRequestEvent::$variant { id, params } => {
                        let handler = inner
                            .server_request_handlers
                            .$handler
                            .read()
                            .await
                            .clone();
                        let Some(handler) = handler else {
                            return false;
                        };

                        let inner = Arc::clone(inner);
                        let id = id.clone();
                        let params = params.clone();
                        tokio::spawn(async move {
                            let response = handler(params).await;
                            send_server_request_handler_result(&inner, &id, response, $context)
                                .await;
                        });
                        true
                    }
                )+
                _ => false,
            }
        }
    };
}

server_request_table!(define_server_request_handlers);

async fn send_server_request_handler_result<R: Serialize>(
    inner: &Arc<Inner>,
    id: &RequestId,
    response: Result<R, ClientError>,
    context: &str,
) -> bool {
    let payload = match response {
        Ok(result) => json!({ "id": id, "result": result }),
        Err(err) => json!({
            "id": id,
            "error": {
                "code": RPC_ERROR_CODE_HANDLER_FAILED,
                "message": format!("{context} handler failed: {err}")
            }
        }),
    };

    if inner.outbound.send(payload).await.is_err() {
        let _ = inner.event_tx.send(ServerEvent::TransportClosed);
    }

    true
}
