mod client;
mod congestion;
mod connection;
mod crypto;
mod data_money_stream;
mod listener;
mod packet;

pub use self::client::connect_async;
pub use self::connection::Connection;
pub use self::data_money_stream::{DataMoneyStream, DataStream, MoneyStream};
pub use self::listener::{ConnectionGenerator, PrepareToSharedSecretGenerator, StreamListener};
use self::packet::*;

use futures::sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::Future;
use futures::{Sink, Stream};
use ilp::IlpPacket;
use plugin::{IlpRequest, Plugin};
use stream_cancel::Valved;
use tokio;

pub type StreamRequest = (u32, IlpPacket, Option<StreamPacket>);

pub fn plugin_to_channels<S>(plugin: S) -> (UnboundedSender<S::Item>, UnboundedReceiver<S::Item>)
where
    S: Plugin<Item = IlpRequest, Error = (), SinkItem = IlpRequest, SinkError = ()> + 'static,
{
    let (sink, stream) = plugin.split();
    let (outgoing_sender, outgoing_receiver) = unbounded::<IlpRequest>();
    let (incoming_sender, incoming_receiver) = unbounded::<IlpRequest>();

    // Stop reading from the plugin when the connection closes
    let (exit, stream) = Valved::new(stream);

    // Forward packets from Connection to plugin
    let receiver = outgoing_receiver.map_err(|err| {
        error!("Broken connection worker chan {:?}", err);
    });
    let forward_to_plugin = sink
        .send_all(receiver)
        .map(|_| ())
        .map_err(|err| {
            error!("Error forwarding request to plugin: {:?}", err);
        }).then(move |_| {
            trace!("Finished forwarding packets from Connection to plugin");
            drop(exit);
            Ok(())
        });
    tokio::spawn(forward_to_plugin);

    // Forward packets from plugin to Connection
    let handle_packets = incoming_sender
        .sink_map_err(|err| {
            error!(
                "Error forwarding packet from plugin to Connection: {:?}",
                err.into_inner()
            );
        }).send_all(stream)
        .then(|_| {
            trace!("Finished forwarding packets from plugin to Connection");
            Ok(())
        });
    tokio::spawn(handle_packets);

    (outgoing_sender, incoming_receiver)
}

#[derive(Fail, Debug)]
pub enum Error {
    #[fail(display = "Error connecting: {}", _0)]
    ConnectionError(String),
}
