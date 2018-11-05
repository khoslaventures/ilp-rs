use super::crypto::{
  fulfillment_to_condition, generate_condition, generate_fulfillment, random_condition,
};
use super::data_money_stream::DataMoneyStream;
use super::packet::*;
use super::StreamPacket;
use byteorder::{BigEndian, ByteOrder};
use bytes::{Bytes, BytesMut};
use chrono::{Duration, Utc};
use futures::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::task;
use futures::task::Task;
use futures::{Async, Future, Poll, Sink, Stream};
use hex;
use ilp::{IlpFulfill, IlpPacket, IlpPrepare, IlpReject, PacketType};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use plugin::IlpRequest;
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Connection {
  internal: Arc<Mutex<Internal>>,
}

impl Connection {
  pub fn new(
    outgoing: UnboundedSender<IlpRequest>,
    incoming: UnboundedReceiver<IlpRequest>,
    shared_secret: Bytes,
    source_account: String,
    destination_account: String,
    is_server: bool,
  ) -> Self {
    let next_stream_id = if is_server { 2 } else { 1 };

    let internal = Internal {
      state: ConnectionState::Open,
      outgoing,
      incoming,
      shared_secret,
      source_account,
      destination_account,
      next_stream_id,
      next_packet_sequence: 1,
      streams: HashMap::new(),
      closed_streams: HashSet::new(),
      pending_outgoing_packets: HashMap::new(),
      new_streams: VecDeque::new(),
      frames_to_resend: Vec::new(),
      recv_task: None,
      connection: None,
    };

    let conn = Connection {
      internal: Arc::new(Mutex::new(internal)),
    };

    {
      let mut internal = conn.internal.lock().unwrap();
      // TODO need a less janky way of allowing the internal to create connection references
      // Note this is used so that the Internal can pass a Connection to new incoming DataMoneyStreams
      internal.connection = Some(conn.clone());

      // TODO figure out a better way to send the initial packet - get the exchange rate and wait for response
      if !is_server {
        internal.send_handshake();
      }
    }

    conn
  }

  pub fn create_stream(&self) -> DataMoneyStream {
    let mut internal = self.internal.lock().unwrap();
    let id = internal.next_stream_id;
    internal.next_stream_id += 1;
    let stream = DataMoneyStream::new(id, self.clone());
    internal.streams.insert(id, stream.clone());
    debug!("Created stream {}", id);
    stream
  }

  pub fn close(&self) -> CloseFuture {
    debug!("Closing connection");
    let mut internal = self.internal.lock().unwrap();

    internal.set_closing();

    CloseFuture { conn: self.clone() }
  }

  pub(super) fn is_closed(&self) -> bool {
    let internal = self.internal.lock().unwrap();
    internal.state == ConnectionState::Closed
  }

  pub(super) fn try_send(&self) -> Result<(), ()> {
    let mut internal = self.internal.lock().unwrap();
    internal.try_send()
  }

  pub(super) fn try_handle_incoming(&self) -> Result<(), ()> {
    let mut internal = self.internal.lock().unwrap();
    internal.try_handle_incoming()
  }
}

impl Stream for Connection {
  type Item = DataMoneyStream;
  type Error = ();

  fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
    trace!("Polling for new incoming streams");
    self.try_handle_incoming()?;

    // Store the current task so that it can be woken up if the
    // MoneyStream or DataStream poll for incoming packets and the connection is closed
    let mut internal = self.internal.lock().unwrap();
    internal.recv_task = Some(task::current());

    if let Some(stream_id) = internal.new_streams.pop_front() {
      let stream = &internal.streams[&stream_id];
      Ok(Async::Ready(Some(stream.clone())))
    } else if self.is_closed() {
      trace!("Connection was closed, no more incoming streams");
      Ok(Async::Ready(None))
    } else {
      Ok(Async::NotReady)
    }
  }
}

pub struct CloseFuture {
  conn: Connection,
}

impl Future for CloseFuture {
  type Item = ();
  type Error = ();

  fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    trace!("Polling to see whether connection was closed");
    self.conn.try_handle_incoming()?;

    if self.conn.is_closed() {
      trace!("Connection was closed, resolving close future");
      Ok(Async::Ready(()))
    } else {
      self.conn.try_send()?;
      trace!("Connection wasn't closed yet, returning NotReady");
      Ok(Async::NotReady)
    }
  }
}

#[derive(PartialEq, Eq, Debug)]
enum ConnectionState {
  // Opening,
  Open,
  Closing,
  CloseSent,
  Closed,
}

struct Internal {
  state: ConnectionState,
  // TODO should this be a bounded sender? Don't want too many outgoing packets in the queue
  outgoing: UnboundedSender<IlpRequest>,
  incoming: UnboundedReceiver<IlpRequest>,
  shared_secret: Bytes,
  source_account: String,
  destination_account: String,
  next_stream_id: u64,
  next_packet_sequence: u64,
  streams: HashMap<u64, DataMoneyStream>,
  closed_streams: HashSet<u64>,
  pending_outgoing_packets: HashMap<u32, (u64, StreamPacket)>,
  new_streams: VecDeque<u64>,
  frames_to_resend: Vec<Frame>,
  // This is used to wake the task polling for incoming streams
  recv_task: Option<Task>,
  connection: Option<Connection>,
  // TODO add connection-level stats
}

impl Internal {
  fn set_closing(&mut self) {
    self.state = ConnectionState::Closing;

    // TODO make sure we don't send stream close frames for every stream
    for stream in self.streams.values() {
      stream.set_closing();
    }
  }

  fn try_send(&mut self) -> Result<(), ()> {
    if self.state == ConnectionState::Closed {
      trace!("Connection was closed, not sending any more packets");
      return Ok(());
    }

    trace!("Checking if we should send an outgoing packet");

    let mut outgoing_amount: u64 = 0;
    let mut frames: Vec<Frame> = Vec::new();
    let mut closed_streams: Vec<u64> = Vec::new();

    // TODO don't send more than max packet amount
    for stream in self.streams.values() {
      trace!("Checking if stream {} has money or data to send", stream.id);
      let amount_to_send =
        stream.money.send_max() - stream.money.pending() - stream.money.total_sent();
      if amount_to_send > 0 {
        trace!("Stream {} sending {}", stream.id, amount_to_send);
        stream.money.add_to_pending(amount_to_send);
        outgoing_amount += amount_to_send;
        frames.push(Frame::StreamMoney(StreamMoneyFrame {
          stream_id: BigUint::from(stream.id),
          shares: BigUint::from(amount_to_send),
        }));
      } else {
        trace!("Stream {} does not have any money to send", stream.id);
      }

      // Send data
      // TODO don't send too much data
      let max_data: usize = 1_000_000_000;
      if let Some((data, offset)) = stream.data.get_outgoing_data(max_data) {
        trace!(
          "Stream {} has {} bytes to send (offset: {})",
          stream.id,
          data.len(),
          offset
        );
        frames.push(Frame::StreamData(StreamDataFrame {
          stream_id: BigUint::from(stream.id),
          data,
          offset: BigUint::from(offset),
        }))
      } else {
        trace!("Stream {} does not have any data to send", stream.id);
      }

      // Inform other side about closing streams
      if stream.is_closing() {
        closed_streams.push(stream.id);
      }
    }

    if self.state == ConnectionState::Closing {
      trace!("Sending connection close frame");
      frames.push(Frame::ConnectionClose(ConnectionCloseFrame {
        code: ErrorCode::NoError,
        message: String::new(),
      }));
      self.state = ConnectionState::CloseSent;
    }

    // Note we need to remove them after we've given up the read lock on self.streams
    for stream_id in closed_streams.iter() {
      trace!("Sending stream close frame for stream {}", stream_id);
      {
        let stream = &self.streams[&stream_id];
        frames.push(Frame::StreamClose(StreamCloseFrame {
          stream_id: BigUint::from(stream.id),
          code: ErrorCode::NoError,
          message: String::new(),
        }));
        stream.set_closed();
      }

      self.streams.remove(&stream_id);
      self.closed_streams.insert(*stream_id);
      debug!("Removed stream {}", stream_id);
    }

    if frames.is_empty() {
      trace!("Not sending packet, no frames need to be sent");
      return Ok(());
    }

    let stream_packet = StreamPacket {
      sequence: self.next_packet_sequence,
      ilp_packet_type: PacketType::IlpPrepare,
      prepare_amount: 0, // TODO set min amount
      frames,
    };
    self.next_packet_sequence += 1;

    let encrypted = stream_packet.to_encrypted(&self.shared_secret).unwrap();
    let condition = generate_condition(&self.shared_secret, &encrypted);
    let prepare = IlpPrepare::new(
      self.destination_account.to_string(),
      outgoing_amount,
      condition,
      // TODO use less predictable timeout
      Utc::now() + Duration::seconds(30),
      encrypted,
    );

    let request_id = rand_u32();
    let request = (request_id, IlpPacket::Prepare(prepare));
    debug!(
      "Sending outgoing request {} with stream packet: {:?}",
      request_id, stream_packet
    );

    self
      .pending_outgoing_packets
      .insert(request_id, (outgoing_amount, stream_packet.clone()));

    self.outgoing.unbounded_send(request).map_err(|err| {
      error!("Error sending outgoing packet: {:?}", err);
    })?;

    Ok(())
  }

  fn try_handle_incoming(&mut self) -> Result<(), ()> {
    // Handle incoming requests until there are no more
    // Note: looping until we get Async::NotReady tells Tokio to wake us up when there are more incoming requests
    loop {
      if self.state == ConnectionState::Closed {
        trace!("Connection was closed, not handling any more incoming packets");
        return Ok(());
      }

      trace!("Polling for incoming requests");
      match self.incoming.poll() {
        Ok(Async::Ready(Some((request_id, packet)))) => match packet {
          IlpPacket::Prepare(prepare) => self.handle_incoming_prepare(request_id, prepare)?,
          IlpPacket::Fulfill(fulfill) => self.handle_fulfill(request_id, fulfill)?,
          IlpPacket::Reject(reject) => self.handle_reject(request_id, reject)?,
          _ => {}
        },
        Ok(Async::Ready(None)) => {
          error!("Incoming stream closed");
          // TODO should this error?
          return Ok(());
        }
        Ok(Async::NotReady) => {
          trace!("No more incoming requests for now");
          return Ok(());
        }
        Err(err) => {
          error!("Error polling incoming request stream: {:?}", err);
          return Err(());
        }
      };
    }
  }

  fn handle_incoming_prepare(&mut self, request_id: u32, prepare: IlpPrepare) -> Result<(), ()> {
    debug!("Handling incoming prepare {}", request_id);

    let response_frames: Vec<Frame> = Vec::new();

    let fulfillment = generate_fulfillment(&self.shared_secret, &prepare.data);
    let condition = fulfillment_to_condition(&fulfillment);
    let is_fulfillable = condition == prepare.execution_condition;

    // TODO avoid copying data
    let stream_packet =
      StreamPacket::from_encrypted(&self.shared_secret, BytesMut::from(prepare.data));
    if stream_packet.is_err() {
      warn!(
        "Got Prepare with data that we cannot parse. Rejecting request {}",
        request_id
      );
      self
        .outgoing
        .unbounded_send((
          request_id,
          IlpPacket::Reject(IlpReject::new("F02", "", "", Bytes::new())),
        )).map_err(|err| {
          error!("Error sending Reject {} {:?}", request_id, err);
        })?;
      return Ok(());
    }
    let stream_packet = stream_packet.unwrap();

    debug!(
      "Prepare {} had stream packet: {:?}",
      request_id, stream_packet
    );

    // Handle new streams
    for frame in stream_packet.frames.iter() {
      match frame {
        Frame::StreamMoney(frame) => {
          self.handle_new_stream(frame.stream_id.to_u64().unwrap());
        }
        Frame::StreamData(frame) => {
          self.handle_new_stream(frame.stream_id.to_u64().unwrap());
        }
        // TODO handle other frames that open streams
        _ => {}
      }
    }

    // Count up the total number of money "shares" in the packet
    let total_money_shares: u64 = stream_packet.frames.iter().fold(0, |sum, frame| {
      if let Frame::StreamMoney(frame) = frame {
        sum + frame.shares.to_u64().unwrap()
      } else {
        sum
      }
    });

    // Handle incoming money
    if is_fulfillable {
      for frame in stream_packet.frames.iter() {
        if let Frame::StreamMoney(frame) = frame {
          // TODO only add money to incoming if sending the fulfill is successful
          // TODO make sure all other checks pass first
          let stream_id = frame.stream_id.to_u64().unwrap();
          let stream = &self.streams[&stream_id];
          let amount: u64 = frame.shares.to_u64().unwrap() * prepare.amount / total_money_shares;
          debug!("Stream {} received {}", stream_id, amount);
          stream.money.add_received(amount);
          stream.money.try_wake_polling();
        }
      }
    }

    self.handle_incoming_data(&stream_packet).unwrap();

    self.handle_stream_closes(&stream_packet);

    self.handle_connection_close(&stream_packet);

    // Fulfill or reject Preapre
    if is_fulfillable {
      let response_packet = StreamPacket {
        sequence: stream_packet.sequence,
        ilp_packet_type: PacketType::IlpFulfill,
        prepare_amount: prepare.amount,
        frames: response_frames,
      };
      let encrypted_response = response_packet.to_encrypted(&self.shared_secret).unwrap();
      let fulfill = IlpPacket::Fulfill(IlpFulfill::new(fulfillment.clone(), encrypted_response));
      debug!(
        "Fulfilling request {} with fulfillment: {} and encrypted stream packet: {:?}",
        request_id,
        hex::encode(&fulfillment[..]),
        response_packet
      );
      self.outgoing.unbounded_send((request_id, fulfill)).unwrap();
    } else {
      let response_packet = StreamPacket {
        sequence: stream_packet.sequence,
        ilp_packet_type: PacketType::IlpReject,
        prepare_amount: prepare.amount,
        frames: response_frames,
      };
      let encrypted_response = response_packet.to_encrypted(&self.shared_secret).unwrap();
      let reject = IlpPacket::Reject(IlpReject::new("F99", "", "", encrypted_response));
      debug!(
        "Rejecting request {} and including encrypted stream packet {:?}",
        request_id, response_packet
      );
      self.outgoing.unbounded_send((request_id, reject)).unwrap();
    }

    Ok(())
  }

  fn handle_new_stream(&mut self, stream_id: u64) {
    // TODO make sure they don't open streams with our number (even or odd, depending on whether we're the client or server)
    let is_new = !self.streams.contains_key(&stream_id);
    let already_closed = self.closed_streams.contains(&stream_id);
    if is_new && !already_closed {
      debug!("Got new stream {}", stream_id);
      if let Some(ref conn) = self.connection {
        let stream = DataMoneyStream::new(stream_id, conn.clone());
        self.streams.insert(stream_id, stream);
        self.new_streams.push_back(stream_id);
      }
    }
  }

  fn handle_incoming_data(&mut self, stream_packet: &StreamPacket) -> Result<(), ()> {
    for frame in stream_packet.frames.iter() {
      if let Frame::StreamData(frame) = frame {
        let stream_id = frame.stream_id.to_u64().unwrap();
        let stream = &self.streams[&stream_id];
        // TODO make sure the offset number isn't too big
        let data = frame.data.clone();
        let offset = frame.offset.to_usize().unwrap();
        debug!(
          "Stream {} got {} bytes of incoming data",
          stream.id,
          data.len()
        );
        stream.data.push_incoming_data(data, offset)?;
        stream.data.try_wake_polling();
      }
    }
    Ok(())
  }

  fn handle_stream_closes(&mut self, stream_packet: &StreamPacket) {
    for frame in stream_packet.frames.iter() {
      if let Frame::StreamClose(frame) = frame {
        let stream_id = frame.stream_id.to_u64().unwrap();
        debug!("Remote closed stream {}", stream_id);
        let stream = &self.streams[&stream_id];
        // TODO finish sending the money and data first
        stream.set_closed();
      }
    }
  }

  fn handle_connection_close(&mut self, stream_packet: &StreamPacket) {
    for frame in stream_packet.frames.iter() {
      if let Frame::ConnectionClose(frame) = frame {
        debug!(
          "Remote closed connection with code: {:?}: {}",
          frame.code, frame.message
        );
        self.close_now();
      }
    }
  }

  fn close_now(&mut self) {
    debug!("Closing connection now");
    self.state = ConnectionState::Closed;

    for stream in self.streams.values() {
      stream.set_closed();
    }

    self.outgoing.close().unwrap();
    self.incoming.close();

    // Wake up the task polling for incoming streams so it ends
    self.try_wake_polling();
  }

  fn handle_fulfill(&mut self, request_id: u32, fulfill: IlpFulfill) -> Result<(), ()> {
    debug!(
      "Request {} was fulfilled with fulfillment: {}",
      request_id,
      hex::encode(&fulfill.fulfillment[..])
    );

    let (original_amount, original_packet) =
      self.pending_outgoing_packets.remove(&request_id).unwrap();

    let response = {
      let decrypted =
        StreamPacket::from_encrypted(&self.shared_secret, BytesMut::from(fulfill.data)).ok();
      if let Some(packet) = decrypted {
        if packet.sequence != original_packet.sequence {
          warn!("Got Fulfill with stream packet whose sequence does not match the original request. Request ID: {}, sequence: {}, fulfill packet: {:?}", request_id, original_packet.sequence, packet);
          None
        } else if packet.ilp_packet_type != PacketType::IlpFulfill {
          warn!("Got Fulfill with stream packet that should have been on a differen type of ILP packet. Request ID: {}, fulfill packet: {:?}", request_id, packet);
          None
        } else {
          trace!("Got Fulfill with stream packet: {:?}", packet);
          Some(packet)
        }
      } else {
        None
      }
    };

    let total_delivered = {
      match response.as_ref() {
        Some(packet) => packet.prepare_amount,
        None => 0,
      }
    };

    for frame in original_packet.frames.iter() {
      if let Frame::StreamMoney(frame) = frame {
        let stream_id = frame.stream_id.to_u64().unwrap();
        let stream = &self.streams[&stream_id];

        let shares = frame.shares.to_u64().unwrap();
        stream.money.pending_to_sent(shares);

        let amount_delivered: u64 = total_delivered * shares / original_amount;
        stream.money.add_delivered(amount_delivered);
      }
    }

    if let Some(packet) = response.as_ref() {
      self.handle_incoming_data(&packet)?;
    }

    // TODO handle response frames

    // Close the connection if they sent a close frame or we sent one and they ACKed it
    if let Some(packet) = response {
      self.handle_connection_close(&packet);
    }
    let we_sent_close_frame = original_packet.frames.iter().any(|frame| {
      if let Frame::ConnectionClose(_) = frame {
        true
      } else {
        false
      }
    });
    if we_sent_close_frame {
      debug!("ConnectionClose frame was ACKed, closing connection now");
      self.close_now();
    }

    Ok(())
  }

  fn handle_reject(&mut self, request_id: u32, reject: IlpReject) -> Result<(), ()> {
    debug!(
      "Request {} was rejected with code: {}",
      request_id, reject.code
    );

    let entry = self.pending_outgoing_packets.remove(&request_id);
    if entry.is_none() {
      return Ok(());
    }
    let (_original_amount, mut original_packet) = entry.unwrap();

    let response = {
      let decrypted =
        StreamPacket::from_encrypted(&self.shared_secret, BytesMut::from(reject.data)).ok();
      if let Some(packet) = decrypted {
        if packet.sequence != original_packet.sequence {
          warn!("Got Reject with stream packet whose sequence does not match the original request. Request ID: {}, sequence: {}, packet: {:?}", request_id, original_packet.sequence, packet);
          None
        } else if packet.ilp_packet_type != PacketType::IlpReject {
          warn!("Got Reject with stream packet that should have been on a differen type of ILP packet. Request ID: {}, packet: {:?}", request_id, packet);
          None
        } else {
          trace!("Got Reject with stream packet: {:?}", packet);
          Some(packet)
        }
      } else {
        None
      }
    };

    // Release pending money
    for frame in original_packet.frames.iter() {
      if let Frame::StreamMoney(frame) = frame {
        let stream_id = frame.stream_id.to_u64().unwrap();
        let stream = &self.streams[&stream_id];

        let shares = frame.shares.to_u64().unwrap();
        stream.money.subtract_from_pending(shares);
      }
    }
    // TODO handle response frames

    if let Some(packet) = response.as_ref() {
      self.handle_incoming_data(&packet)?;

      self.handle_connection_close(&packet);
    }

    // Only resend frames if they didn't get to the receiver
    if response.is_none() {
      while !original_packet.frames.is_empty() {
        match original_packet.frames.pop().unwrap() {
          Frame::StreamData(frame) => self.frames_to_resend.push(Frame::StreamData(frame)),
          Frame::StreamClose(frame) => self.frames_to_resend.push(Frame::StreamClose(frame)),
          Frame::ConnectionClose(frame) => {
            self.frames_to_resend.push(Frame::ConnectionClose(frame))
          }
          _ => {}
        }
      }
    }

    Ok(())
  }

  fn send_handshake(&mut self) {
    let sequence = self.next_packet_sequence;
    self.next_packet_sequence += 1;

    let packet = StreamPacket {
      sequence,
      ilp_packet_type: PacketType::IlpPrepare,
      prepare_amount: 0,
      frames: vec![Frame::ConnectionNewAddress(ConnectionNewAddressFrame {
        source_account: self.source_account.to_string(),
      })],
    };
    self.send_unfulfillable_prepare(&packet);
  }

  // TODO wait for response
  fn send_unfulfillable_prepare(&self, stream_packet: &StreamPacket) -> () {
    let request_id = rand_u32();
    let prepare = IlpPacket::Prepare(IlpPrepare::new(
      // TODO do we need to clone this?
      self.destination_account.to_string(),
      0,
      random_condition(),
      Utc::now() + Duration::seconds(30),
      stream_packet.to_encrypted(&self.shared_secret).unwrap(),
    ));
    self.outgoing.unbounded_send((request_id, prepare)).unwrap();
  }

  fn try_wake_polling(&mut self) {
    if let Some(task) = self.recv_task.take() {
      debug!("Notifying incoming stream poller that it should wake up");
      task.notify();
    }
  }
}

// TODO make sure this isn't a slow operation, we don't need cryptographically secure randomness
fn rand_u32() -> u32 {
  let mut int: [u8; 4] = [0; 4];
  SystemRandom::new()
    .fill(&mut int[..])
    .expect("Unable to get random u32");
  BigEndian::read_u32(&int[..])
}
