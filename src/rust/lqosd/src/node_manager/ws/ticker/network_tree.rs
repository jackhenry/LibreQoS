use crate::node_manager::ws::messages::WsResponse;
use crate::node_manager::ws::publish_subscribe::PubSub;
use crate::node_manager::ws::published_channels::PublishedChannels;
use lqos_bus::{BusReply, BusRequest, BusResponse, Circuit};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::time::timeout;

const INTERNAL_BUS_TIMEOUT: Duration = Duration::from_secs(5);

async fn request_internal_bus(
    context: &'static str,
    bus_tx: Sender<(tokio::sync::oneshot::Sender<lqos_bus::BusReply>, BusRequest)>,
    request: BusRequest,
) -> Option<BusReply> {
    let (tx, rx) = tokio::sync::oneshot::channel::<BusReply>();
    match timeout(INTERNAL_BUS_TIMEOUT, bus_tx.send((tx, request))).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!("{context}: failed to send request to bus: {err:?}");
            return None;
        }
        Err(_) => {
            tracing::warn!("{context}: timed out queueing request to bus");
            return None;
        }
    }

    match timeout(INTERNAL_BUS_TIMEOUT, rx).await {
        Ok(Ok(replies)) => Some(replies),
        Ok(Err(err)) => {
            tracing::warn!("{context}: failed to receive response from bus: {err:?}");
            None
        }
        Err(_) => {
            tracing::warn!("{context}: timed out waiting for bus response");
            None
        }
    }
}

pub async fn network_tree(
    channels: Arc<PubSub>,
    bus_tx: Sender<(tokio::sync::oneshot::Sender<lqos_bus::BusReply>, BusRequest)>,
) {
    if !channels
        .is_channel_alive(PublishedChannels::NetworkTree)
        .await
    {
        return;
    }

    let Some(replies) =
        request_internal_bus("NetworkTree", bus_tx, BusRequest::GetFullNetworkMap).await
    else {
        return;
    };
    for reply in replies.responses.into_iter() {
        if let BusResponse::NetworkMap(nodes) = reply {
            let message = WsResponse::NetworkTree { data: nodes };
            channels.send(PublishedChannels::NetworkTree, message).await;
        }
    }
}

pub async fn all_circuits(
    bus_tx: Sender<(tokio::sync::oneshot::Sender<lqos_bus::BusReply>, BusRequest)>,
) -> Vec<Circuit> {
    let Some(replies) =
        request_internal_bus("AllCircuits", bus_tx, BusRequest::GetAllCircuits).await
    else {
        return Vec::new();
    };
    for reply in replies.responses.into_iter() {
        if let BusResponse::CircuitData(circuits) = reply {
            return circuits;
        }
    }
    Vec::new()
}

pub async fn all_subscribers(
    channels: Arc<PubSub>,
    bus_tx: Sender<(tokio::sync::oneshot::Sender<lqos_bus::BusReply>, BusRequest)>,
) {
    if !channels
        .is_channel_alive(PublishedChannels::NetworkTreeClients)
        .await
    {
        return;
    }

    let devices = all_circuits(bus_tx).await;
    let message = WsResponse::NetworkTreeClients { data: devices };
    channels
        .send(PublishedChannels::NetworkTreeClients, message)
        .await;
}
