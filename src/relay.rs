//! Data-route relay between Lightspeed and the envd in one machine.
//!
//! Guest-to-Lightspeed traffic is copied as is. Lightspeed-to-guest text
//! frames go through the credential rewriter first, so `secretEnv` values with
//! a rule are handed to the machine's substitution proxy instead of envd.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use axum::extract::ws::{Message as AxumMessage, WebSocket};
use environment_protocol::error::EnvironmentProtocolErrorCode;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::Message as GuestMessage;

use crate::{
    backend::VmBackend,
    channel::GuestSocket,
    config::{CredentialRule, UnknownSecretPolicy},
    credentials::{self, Rewrite},
};

pub struct RelayContext<B> {
    pub backend: B,
    pub target_id: String,
    pub rules: Arc<BTreeMap<String, CredentialRule>>,
    pub unknown: UnknownSecretPolicy,
    pub idle: Duration,
}

pub async fn proxy<B: VmBackend>(
    context: RelayContext<B>,
    mut lightspeed: WebSocket,
    mut guest: GuestSocket,
) {
    let reason = loop {
        let event = tokio::time::timeout(context.idle, async {
            tokio::select! {
                message = lightspeed.recv() => Event::Lightspeed(message),
                message = guest.next() => Event::Guest(message),
            }
        })
        .await;
        let Ok(event) = event else {
            break "idle timeout";
        };
        match event {
            Event::Lightspeed(Some(Ok(AxumMessage::Text(text)))) => {
                match forward_request(&context, text.as_str()).await {
                    Outbound::Guest(frame) => {
                        if guest.send(GuestMessage::Text(frame.into())).await.is_err() {
                            break "envd send failed";
                        }
                    }
                    Outbound::Lightspeed(response) => {
                        if lightspeed
                            .send(AxumMessage::Text(response.into()))
                            .await
                            .is_err()
                        {
                            break "lightspeed send failed";
                        }
                    }
                }
            }
            Event::Lightspeed(Some(Ok(message))) => {
                let close = matches!(message, AxumMessage::Close(_));
                if close {
                    let _ = guest.send(to_guest(message)).await;
                    break "lightspeed closed";
                }
                if guest.send(to_guest(message)).await.is_err() {
                    break "envd send failed";
                }
            }
            Event::Guest(Some(Ok(message))) => {
                let close = matches!(message, GuestMessage::Close(_));
                let Some(message) = to_lightspeed(message) else {
                    continue;
                };
                if close {
                    let _ = lightspeed.send(message).await;
                    break "envd closed";
                }
                if lightspeed.send(message).await.is_err() {
                    break "lightspeed send failed";
                }
            }
            Event::Lightspeed(None) => break "lightspeed disconnected",
            Event::Lightspeed(Some(Err(_))) => break "lightspeed read error",
            Event::Guest(None) => break "envd disconnected",
            Event::Guest(Some(Err(_))) => break "envd read error",
        }
    };
    tracing::info!(reason, "data route closed");
    // Close both legs so an upstream EOF never becomes a bare reset in envd.
    let _ = guest.close(None).await;
    let _ = lightspeed.send(AxumMessage::Close(None)).await;
}

enum Outbound {
    Guest(String),
    Lightspeed(String),
}

async fn forward_request<B: VmBackend>(context: &RelayContext<B>, text: &str) -> Outbound {
    match credentials::rewrite_frame(text, &context.rules, context.unknown) {
        Rewrite::Unchanged => Outbound::Guest(text.to_owned()),
        Rewrite::Rejected { response } => {
            tracing::warn!("rejected request carrying a secret with no credential rule");
            Outbound::Lightspeed(response)
        }
        Rewrite::Rewritten {
            id,
            frame,
            captured,
        } => match context
            .backend
            .supply_credentials(&context.target_id, &captured)
            .await
        {
            Ok(()) => Outbound::Guest(frame),
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "credential supply failed");
                Outbound::Lightspeed(credentials::error_response(
                    id,
                    EnvironmentProtocolErrorCode::CapabilityUnavailable,
                    format!("machine could not accept credentials: {error}"),
                ))
            }
        },
    }
}

enum Event {
    Lightspeed(Option<Result<AxumMessage, axum::Error>>),
    Guest(Option<Result<GuestMessage, tokio_tungstenite::tungstenite::Error>>),
}

fn to_guest(message: AxumMessage) -> GuestMessage {
    match message {
        AxumMessage::Text(value) => GuestMessage::Text(value.as_str().into()),
        AxumMessage::Binary(value) => GuestMessage::Binary(value),
        AxumMessage::Ping(value) => GuestMessage::Ping(value),
        AxumMessage::Pong(value) => GuestMessage::Pong(value),
        AxumMessage::Close(_) => GuestMessage::Close(None),
    }
}

fn to_lightspeed(message: GuestMessage) -> Option<AxumMessage> {
    match message {
        GuestMessage::Text(value) => Some(AxumMessage::Text(value.as_str().into())),
        GuestMessage::Binary(value) => Some(AxumMessage::Binary(value)),
        GuestMessage::Ping(value) => Some(AxumMessage::Ping(value)),
        GuestMessage::Pong(value) => Some(AxumMessage::Pong(value)),
        GuestMessage::Close(_) => Some(AxumMessage::Close(None)),
        GuestMessage::Frame(_) => None,
    }
}
