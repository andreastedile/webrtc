use crate::association_stats::AssociationStats;
use crate::chunk::chunk_cookie_echo::ChunkCookieEcho;
use crate::chunk::chunk_init::ChunkInit;
use crate::chunk::chunk_payload_data::{ChunkPayloadData, PayloadProtocolIdentifier};
use crate::chunk::chunk_reconfig::ChunkReconfig;
use crate::chunk::chunk_selective_ack::ChunkSelectiveAck;
use crate::chunk::chunk_shutdown::ChunkShutdown;
use crate::chunk::chunk_shutdown_ack::ChunkShutdownAck;
use crate::chunk::chunk_shutdown_complete::ChunkShutdownComplete;
use crate::chunk::Chunk;
use crate::error::Error;
use crate::error_cause::*;
use crate::packet::Packet;
use crate::param::param_outgoing_reset_request::ParamOutgoingResetRequest;
use crate::param::param_reconfig_response::{ParamReconfigResponse, ReconfigResult};
use crate::param::param_state_cookie::ParamStateCookie;
use crate::queue::control_queue::ControlQueue;
use crate::queue::payload_queue::PayloadQueue;
use crate::queue::pending_queue::PendingQueue;
use crate::stream::{ReliabilityType, Stream};
use crate::timer::ack_timer::{AckTimer, ACK_INTERVAL};
use crate::timer::rtx_timer::{RtoManager, RtxTimer, MAX_INIT_RETRANS, NO_MAX_RETRANS};
use crate::util::*;

use util::Conn;
//use async_trait::async_trait;
use crate::chunk::chunk_error::ChunkError;
use crate::chunk::chunk_forward_tsn::{ChunkForwardTsn, ChunkForwardTsnStream};
use crate::param::Param;
use bytes::Bytes;
use rand::random;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::Notify;

pub(crate) const RECEIVE_MTU: u32 = 8192;
/// MTU for inbound packet (from DTLS)
pub(crate) const INITIAL_MTU: u32 = 1228;
/// initial MTU for outgoing packets (to DTLS)
pub(crate) const INITIAL_RECV_BUF_SIZE: u32 = 1024 * 1024;
pub(crate) const COMMON_HEADER_SIZE: u32 = 12;
pub(crate) const DATA_CHUNK_HEADER_SIZE: u32 = 16;
pub(crate) const DEFAULT_MAX_MESSAGE_SIZE: u32 = 65536;

/// other constants
pub(crate) const ACCEPT_CH_SIZE: usize = 16;

/// association state enums
#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) enum AssociationState {
    Closed = 0,
    CookieWait = 1,
    CookieEchoed = 2,
    Established = 3,
    ShutdownAckSent = 4,
    ShutdownPending = 5,
    ShutdownReceived = 6,
    ShutdownSent = 7,
}

impl From<u8> for AssociationState {
    fn from(v: u8) -> AssociationState {
        match v {
            1 => AssociationState::CookieWait,
            2 => AssociationState::CookieEchoed,
            3 => AssociationState::Established,
            4 => AssociationState::ShutdownAckSent,
            5 => AssociationState::ShutdownPending,
            6 => AssociationState::ShutdownReceived,
            7 => AssociationState::ShutdownSent,
            _ => AssociationState::Closed,
        }
    }
}

impl fmt::Display for AssociationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            AssociationState::Closed => "Closed",
            AssociationState::CookieWait => "CookieWait",
            AssociationState::CookieEchoed => "CookieEchoed",
            AssociationState::Established => "Established",
            AssociationState::ShutdownPending => "ShutdownPending",
            AssociationState::ShutdownSent => "ShutdownSent",
            AssociationState::ShutdownReceived => "ShutdownReceived",
            AssociationState::ShutdownAckSent => "ShutdownAckSent",
        };
        write!(f, "{}", s)
    }
}

/// retransmission timer IDs
#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) enum RtxTimerId {
    T1Init,
    T1Cookie,
    T2Shutdown,
    T3RTX,
    Reconfig,
}

impl Default for RtxTimerId {
    fn default() -> Self {
        RtxTimerId::T1Init
    }
}

impl fmt::Display for RtxTimerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            RtxTimerId::T1Init => "T1Init",
            RtxTimerId::T1Cookie => "T1Cookie",
            RtxTimerId::T2Shutdown => "T2Shutdown",
            RtxTimerId::T3RTX => "T3RTX",
            RtxTimerId::Reconfig => "Reconfig",
        };
        write!(f, "{}", s)
    }
}

/// ack mode (for testing)
#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) enum AckMode {
    Normal,
    NoDelay,
    AlwaysDelay,
}
impl Default for AckMode {
    fn default() -> Self {
        AckMode::Normal
    }
}

impl fmt::Display for AckMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            AckMode::Normal => "Normal",
            AckMode::NoDelay => "NoDelay",
            AckMode::AlwaysDelay => "AlwaysDelay",
        };
        write!(f, "{}", s)
    }
}

/// ack transmission state
#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) enum AckState {
    Idle,      // ack timer is off
    Immediate, // will send ack immediately
    Delay,     // ack timer is on (ack is being delayed)
}

impl Default for AckState {
    fn default() -> Self {
        AckState::Idle
    }
}

impl fmt::Display for AckState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            AckState::Idle => "Idle",
            AckState::Immediate => "Immediate",
            AckState::Delay => "Delay",
        };
        write!(f, "{}", s)
    }
}

/// Config collects the arguments to create_association construction into
/// a single structure
pub struct Config {
    pub net_conn: Arc<dyn Conn + Send + Sync>,
    pub max_receive_buffer_size: u32,
    pub max_message_size: u32,
}

///Association represents an SCTP association
///13.2.  Parameters Necessary per Association (i.e., the TCB)
///Peer        : Tag value to be sent in every packet and is received
///Verification: in the INIT or INIT ACK chunk.
///Tag         :
//
///My          : Tag expected in every inbound packet and sent in the
///Verification: INIT or INIT ACK chunk.
//
///Tag         :
///State       : A state variable indicating what state the association
///            : is in, i.e., COOKIE-WAIT, COOKIE-ECHOED, ESTABLISHED,
///            : SHUTDOWN-PENDING, SHUTDOWN-SENT, SHUTDOWN-RECEIVED,
///            : SHUTDOWN-ACK-SENT.
//
///              Note: No "CLOSED" state is illustrated since if a
///              association is "CLOSED" its TCB SHOULD be removed.
#[derive(Default)]
pub struct Association {
    bytes_received: u64,
    bytes_sent: u64,

    //lock sync.RWMutex
    net_conn: Option<Arc<dyn Conn + Send + Sync>>,

    peer_verification_tag: u32,
    my_verification_tag: u32,
    state: Arc<AtomicU8>,
    my_next_tsn: u32,         // nextTSN
    peer_last_tsn: u32,       // lastRcvdTSN
    min_tsn2measure_rtt: u32, // for RTT measurement
    will_send_forward_tsn: bool,
    will_retransmit_fast: bool,
    will_retransmit_reconfig: bool,

    will_send_shutdown: bool,
    will_send_shutdown_ack: bool,
    will_send_shutdown_complete: bool,

    // Reconfig
    my_next_rsn: u32,
    reconfigs: HashMap<u32, ChunkReconfig>,
    reconfig_requests: HashMap<u32, ParamOutgoingResetRequest>,

    // Non-RFC internal data
    source_port: u16,
    destination_port: u16,
    my_max_num_inbound_streams: u16,
    my_max_num_outbound_streams: u16,
    my_cookie: ParamStateCookie,
    payload_queue: PayloadQueue,
    inflight_queue: PayloadQueue,
    pending_queue: PendingQueue,
    control_queue: ControlQueue,
    mtu: u32,
    max_payload_size: u32, // max DATA chunk payload size
    cumulative_tsn_ack_point: u32,
    advanced_peer_tsn_ack_point: u32,
    use_forward_tsn: bool,

    // Congestion control parameters
    max_receive_buffer_size: u32,
    max_message_size: Arc<AtomicU32>,
    cwnd: u32,     // my congestion window size
    rwnd: u32,     // calculated peer's receiver windows size
    ssthresh: u32, // slow start threshold
    partial_bytes_acked: u32,
    in_fast_recovery: bool,
    fast_recover_exit_point: u32,

    // RTX & Ack timer
    rto_mgr: RtoManager,
    t1init: RtxTimer,
    t1cookie: RtxTimer,
    t2shutdown: RtxTimer,
    t3rtx: RtxTimer,
    treconfig: RtxTimer,
    ack_timer: AckTimer,

    // Chunks stored for retransmission
    stored_init: Option<ChunkInit>,
    stored_cookie_echo: Option<ChunkCookieEcho>,

    streams: HashMap<u16, Stream>,
    /*TODO:     acceptCh             chan *Stream
        readLoopCloseCh      chan struct{}

        closeWriteLoopCh     chan struct{}

    */
    awake_write_loop_ch: Notify,

    //TODO: handshakeCompletedCh : mpsc:: chan error
    //TODO: closeWriteLoopOnce sync.Once

    // local error
    silent_error: Option<Error>,

    ack_state: AckState,
    ack_mode: AckMode, // for testing

    // stats
    stats: AssociationStats,

    // per inbound packet context
    delayed_ack_triggered: bool,
    immediate_ack_triggered: bool,

    name: String,
    //log  logging.LeveledLogger
}

impl Association {
    /*/// Server accepts a SCTP stream over a conn
    pub fn Server(config: Config) ->Result<Self, Error> {
        a := create_association(config)
        self.init(false)

        select {
        case err := <-self.handshakeCompletedCh:
            if err != nil {
                return nil, err
            }
            return a, nil
        case <-self.readLoopCloseCh:
            return nil, errAssociationClosedBeforeConn
        }
    }

    /// Client opens a SCTP stream over a conn
    func Client(config Config) (*Association, error) {
        a := create_association(config)
        self.init(true)

        select {
        case err := <-self.handshakeCompletedCh:
            if err != nil {
                return nil, err
            }
            return a, nil
        case <-self.readLoopCloseCh:
            return nil, errAssociationClosedBeforeConn
        }
    }*/

    fn create_association(config: Config) -> Self {
        let max_receive_buffer_size = if config.max_receive_buffer_size == 0 {
            INITIAL_RECV_BUF_SIZE
        } else {
            config.max_receive_buffer_size
        };

        let max_message_size = if config.max_message_size == 0 {
            DEFAULT_MAX_MESSAGE_SIZE
        } else {
            config.max_message_size
        };

        let tsn = random::<u32>();
        let mut a = Association {
            net_conn: Some(config.net_conn),
            max_receive_buffer_size,
            max_message_size: Arc::new(AtomicU32::new(max_message_size)),
            my_max_num_outbound_streams: u16::MAX,
            my_max_num_inbound_streams: u16::MAX,
            payload_queue: PayloadQueue::new(),
            inflight_queue: PayloadQueue::new(),
            pending_queue: PendingQueue::new(),
            control_queue: ControlQueue::new(),
            mtu: INITIAL_MTU,
            max_payload_size: INITIAL_MTU - (COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE),
            my_verification_tag: random::<u32>(),
            my_next_tsn: tsn,
            my_next_rsn: tsn,
            min_tsn2measure_rtt: tsn,
            state: Arc::new(AtomicU8::new(AssociationState::Closed as u8)),
            rto_mgr: RtoManager::new(),
            streams: HashMap::new(),
            reconfigs: HashMap::new(),
            reconfig_requests: HashMap::new(),
            /*acceptCh:                make(chan *Stream, ACCEPT_CH_SIZE),
            readLoopCloseCh:         make(chan struct{}),
            awake_write_loop_ch:        make(chan struct{}, 1),
            closeWriteLoopCh:        make(chan struct{}),
            handshakeCompletedCh:    make(chan error),*/
            cumulative_tsn_ack_point: tsn - 1,
            advanced_peer_tsn_ack_point: tsn - 1,
            silent_error: Some(Error::ErrSilentlyDiscard),
            stats: AssociationStats::default(),
            //log:                     config.LoggerFactory.NewLogger("sctp"),
            ..Default::default()
        };

        a.name = format!("{:p}", &a);

        // RFC 4690 Sec 7.2.1
        //  o  The initial cwnd before DATA transmission or after a sufficiently
        //     long idle period MUST be set to min(4*MTU, max (2*MTU, 4380
        //     bytes)).
        a.cwnd = std::cmp::min(4 * a.mtu, std::cmp::max(2 * a.mtu, 4380));
        log::trace!(
            "[{}] updated cwnd={} ssthresh={} inflight={} (INI)",
            a.name,
            a.cwnd,
            a.ssthresh,
            a.inflight_queue.get_num_bytes()
        );

        a.t1init = RtxTimer::new(RtxTimerId::T1Init, MAX_INIT_RETRANS);
        a.t1cookie = RtxTimer::new(RtxTimerId::T1Cookie, MAX_INIT_RETRANS);
        a.t2shutdown = RtxTimer::new(RtxTimerId::T2Shutdown, NO_MAX_RETRANS); // retransmit forever
        a.t3rtx = RtxTimer::new(RtxTimerId::T3RTX, NO_MAX_RETRANS); // retransmit forever
        a.treconfig = RtxTimer::new(RtxTimerId::Reconfig, NO_MAX_RETRANS); // retransmit forever
        a.ack_timer = AckTimer::new(ACK_INTERVAL);

        a
    }

    /*
            fn init(&self, isClient: bool) {

                 //TODO: go self.readLoop()
                 //TODO: go self.writeLoop()

                 if isClient {
                     self.set_state(CookieWait)
                     init := &chunkInit{}
                     init.initialTSN = self.my_next_tsn
                     init.numOutboundStreams = self.my_max_num_outbound_streams
                     init.numInboundStreams = self.my_max_num_inbound_streams
                     init.initiateTag = self.my_verification_tag
                     init.advertisedReceiverWindowCredit = self.max_receive_buffer_size
                     setSupportedExtensions(&init.chunkInitCommon)
                     self.stored_init = init

                     err := self.sendInit()
                     if err != nil {
                         self.log.Errorf("[%s] failed to send init: %s", self.name, err.Error())
                     }

                     self.t1init.start(self.rto_mgr.getRTO())
                 }
             }
    */

    /// caller must hold self.lock
    fn send_init(&mut self) -> Result<(), Error> {
        if let Some(stored_init) = &self.stored_init {
            log::debug!("[{}] sending INIT", self.name);

            self.source_port = 5000; // Spec??
            self.destination_port = 5000; // Spec??

            let outbound = Packet {
                source_port: self.source_port,
                destination_port: self.destination_port,
                verification_tag: self.peer_verification_tag,
                chunks: vec![Box::new(stored_init.clone())],
            };

            self.control_queue.push_back(outbound);
            self.awake_write_loop();

            Ok(())
        } else {
            Err(Error::ErrInitNotStoredToSend)
        }
    }

    /// caller must hold self.lock
    fn send_cookie_echo(&mut self) -> Result<(), Error> {
        if let Some(stored_cookie_echo) = &self.stored_cookie_echo {
            log::debug!("[{}] sending COOKIE-ECHO", self.name);

            let outbound = Packet {
                source_port: self.source_port,
                destination_port: self.destination_port,
                verification_tag: self.peer_verification_tag,
                chunks: vec![Box::new(stored_cookie_echo.clone())],
            };

            self.control_queue.push_back(outbound);
            self.awake_write_loop();
            Ok(())
        } else {
            Err(Error::ErrCookieEchoNotStoredToSend)
        }
    }
    /*
        // Shutdown initiates the shutdown sequence. The method blocks until the
        // shutdown sequence is completed and the connection is closed, or until the
        // passed context is done, in which case the context's error is returned.
        fn Shutdown(ctx context.Context) error {
            self.log.Debugf("[%s] closing association..", self.name)

            state := self.get_state()

            if state != Established {
                return fmt.Errorf("%w: shutdown %s", errShutdownNonEstablished, self.name)
            }

            // Attempt a graceful shutdown.
            self.set_state(ShutdownPending)

            self.lock.Lock()

            if self.inflight_queue.size() == 0 {
                // No more outstanding, send shutdown.
                self.will_send_shutdown = true
                self.awake_write_loop()
                self.set_state(ShutdownSent)
            }

            self.lock.Unlock()

            select {
            case <-self.closeWriteLoopCh:
                return nil
            case <-ctx.Done():
                return ctx.Err()
            }
        }

        // Close ends the SCTP Association and cleans up any state
        fn Close() error {
            self.log.Debugf("[%s] closing association..", self.name)

            err := self.close()

            // Wait for readLoop to end
            <-self.readLoopCloseCh

            self.log.Debugf("[%s] association closed", self.name)
            self.log.Debugf("[%s] stats nDATAs (in) : %d", self.name, self.stats.get_num_datas())
            self.log.Debugf("[%s] stats nSACKs (in) : %d", self.name, self.stats.get_num_sacks())
            self.log.Debugf("[%s] stats nT3Timeouts : %d", self.name, self.stats.get_num_t3timeouts())
            self.log.Debugf("[%s] stats nAckTimeouts: %d", self.name, self.stats.get_num_ack_timeouts())
            self.log.Debugf("[%s] stats nFastRetrans: %d", self.name, self.stats.get_num_fast_retrans())

            return err
        }
    */
    async fn close(&mut self) -> Result<(), Error> {
        log::debug!("[{}] closing association..", self.name);

        self.set_state(AssociationState::Closed);

        //self.net_conn.Close()

        self.close_all_timers().await;

        // awake writeLoop to exit
        //TODO: self.closeWriteLoopOnce.Do(func() { close(self.closeWriteLoopCh) })

        Ok(())
    }

    async fn close_all_timers(&mut self) {
        // Close all retransmission & ack timers
        self.t1init.stop().await;
        self.t1cookie.stop().await;
        self.t2shutdown.stop().await;
        self.t3rtx.stop().await;
        self.treconfig.stop().await;
        self.ack_timer.stop();
    }
    /*
    fn readLoop() {
        var closeErr error
        defer func() {
            // also stop writeLoop, otherwise writeLoop can be leaked
            // if connection is lost when there is no writing packet.
            self.closeWriteLoopOnce.Do(func() { close(self.closeWriteLoopCh) })

            self.lock.Lock()
            for _, s := range self.streams {
                self.unregister_stream(s, closeErr)
            }
            self.lock.Unlock()
            close(self.acceptCh)
            close(self.readLoopCloseCh)

            self.log.Debugf("[%s] association closed", self.name)
            self.log.Debugf("[%s] stats nDATAs (in) : %d", self.name, self.stats.get_num_datas())
            self.log.Debugf("[%s] stats nSACKs (in) : %d", self.name, self.stats.get_num_sacks())
            self.log.Debugf("[%s] stats nT3Timeouts : %d", self.name, self.stats.get_num_t3timeouts())
            self.log.Debugf("[%s] stats nAckTimeouts: %d", self.name, self.stats.get_num_ack_timeouts())
            self.log.Debugf("[%s] stats nFastRetrans: %d", self.name, self.stats.get_num_fast_retrans())
        }()

        self.log.Debugf("[%s] readLoop entered", self.name)
        buffer := make([]byte, RECEIVE_MTU)

        for {
            n, err := self.net_conn.read(buffer)
            if err != nil {
                closeErr = err
                break
            }
            // Make a buffer sized to what we read, then copy the data we
            // read from the underlying transport. We do this because the
            // user data is passed to the reassembly queue without
            // copying.
            inbound := make([]byte, n)
            copy(inbound, buffer[:n])
            atomic.AddUint64(&self.bytes_received, uint64(n))
            if err = self.handleInbound(inbound); err != nil {
                closeErr = err
                break
            }
        }

        self.log.Debugf("[%s] readLoop exited %s", self.name, closeErr)
    }

    fn writeLoop() {
        self.log.Debugf("[%s] writeLoop entered", self.name)
        defer self.log.Debugf("[%s] writeLoop exited", self.name)

    loop:
        for {
            rawPackets, ok := self.gather_outbound()

            for _, raw := range rawPackets {
                _, err := self.net_conn.write(raw)
                if err != nil {
                    if err != io.EOF {
                        self.log.Warnf("[%s] failed to write packets on net_conn: %v", self.name, err)
                    }
                    self.log.Debugf("[%s] writeLoop ended", self.name)
                    break loop
                }
                atomic.AddUint64(&self.bytes_sent, uint64(len(raw)))
            }

            if !ok {
                if err := self.close(); err != nil {
                    self.log.Warnf("[%s] failed to close association: %v", self.name, err)
                }

                return
            }

            select {
            case <-self.awake_write_loop_ch:
            case <-self.closeWriteLoopCh:
                break loop
            }
        }

        self.set_state(closed)
        self.close_all_timers()
    }*/

    fn awake_write_loop(&self) {
        self.awake_write_loop_ch.notify_one();
    }

    /// unregister_stream un-registers a stream from the association
    /// The caller should hold the association write lock.
    fn unregister_stream(&mut self, stream_identifier: u16, _err: Error) {
        let s = self.streams.remove(&stream_identifier);
        if let Some(s) = s {
            //TODO: s.readErr = err
            s.read_notifier.notify_waiters();
        }
    }
    /*
                                  // handleInbound parses incoming raw packets
                                  fn handleInbound(raw []byte) error {
                                      p := &packet{}
                                      if err := p.unmarshal(raw); err != nil {
                                          self.log.Warnf("[%s] unable to parse SCTP packet %s", self.name, err)
                                          return nil
                                      }

                                      if err := check_packet(p); err != nil {
                                          self.log.Warnf("[%s] failed validating packet %s", self.name, err)
                                          return nil
                                      }

                                      self.handle_chunk_start()

                                      for _, c := range p.chunks {
                                          if err := self.handleChunk(p, c); err != nil {
                                              return err
                                          }
                                      }

                                      self.handleChunkEnd()

                                      return nil
                                  }

                                  // The caller should hold the lock
                                  fn gatherDataPacketsToRetransmit(rawPackets [][]byte) [][]byte {
                                      for _, p := range self.get_data_packets_to_retransmit() {
                                          raw, err := p.marshal()
                                          if err != nil {
                                              self.log.Warnf("[%s] failed to serialize a DATA packet to be retransmitted", self.name)
                                              continue
                                          }
                                          rawPackets = append(rawPackets, raw)
                                      }

                                      return rawPackets
                                  }

                                  // The caller should hold the lock
                                  fn gatherOutboundDataAndReconfigPackets(rawPackets [][]byte) [][]byte {
                                      // Pop unsent data chunks from the pending queue to send as much as
                                      // cwnd and rwnd allow.
                                      chunks, sisToReset := self.pop_pending_data_chunks_to_send()
                                      if len(chunks) > 0 {
                                          // Start timer. (noop if already started)
                                          self.log.Tracef("[%s] T3-rtx timer start (pt1)", self.name)
                                          self.t3rtx.start(self.rto_mgr.getRTO())
                                          for _, p := range self.bundle_data_chunks_into_packets(chunks) {
                                              raw, err := p.marshal()
                                              if err != nil {
                                                  self.log.Warnf("[%s] failed to serialize a DATA packet", self.name)
                                                  continue
                                              }
                                              rawPackets = append(rawPackets, raw)
                                          }
                                      }

                                      if len(sisToReset) > 0 || self.will_retransmit_reconfig {
                                          if self.will_retransmit_reconfig {
                                              self.will_retransmit_reconfig = false
                                              self.log.Debugf("[%s] retransmit %d RECONFIG chunk(s)", self.name, len(self.reconfigs))
                                              for _, c := range self.reconfigs {
                                                  p := self.create_packet([]chunk{c})
                                                  raw, err := p.marshal()
                                                  if err != nil {
                                                      self.log.Warnf("[%s] failed to serialize a RECONFIG packet to be retransmitted", self.name)
                                                  } else {
                                                      rawPackets = append(rawPackets, raw)
                                                  }
                                              }
                                          }

                                          if len(sisToReset) > 0 {
                                              rsn := self.generate_next_rsn()
                                              tsn := self.my_next_tsn - 1
                                              c := &chunkReconfig{
                                                  paramA: &paramOutgoingResetRequest{
                                                      reconfigRequestSequenceNumber: rsn,
                                                      senderLastTSN:                 tsn,
                                                      streamIdentifiers:             sisToReset,
                                                  },
                                              }
                                              self.reconfigs[rsn] = c // store in the map for retransmission
                                              self.log.Debugf("[%s] sending RECONFIG: rsn=%d tsn=%d streams=%v",
                                                  self.name, rsn, self.my_next_tsn-1, sisToReset)
                                              p := self.create_packet([]chunk{c})
                                              raw, err := p.marshal()
                                              if err != nil {
                                                  self.log.Warnf("[%s] failed to serialize a RECONFIG packet to be transmitted", self.name)
                                              } else {
                                                  rawPackets = append(rawPackets, raw)
                                              }
                                          }

                                          if len(self.reconfigs) > 0 {
                                              self.t_reconfig.start(self.rto_mgr.getRTO())
                                          }
                                      }

                                      return rawPackets
                                  }

                                  // The caller should hold the lock
                                  fn gatherOutboundFastRetransmissionPackets(rawPackets [][]byte) [][]byte {
                                      if self.will_retransmit_fast {
                                          self.will_retransmit_fast = false

                                          toFastRetrans := []chunk{}
                                          fastRetransSize := COMMON_HEADER_SIZE

                                          for i := 0; ; i++ {
                                              c, ok := self.inflight_queue.get(self.cumulative_tsn_ack_point + uint32(i) + 1)
                                              if !ok {
                                                  break // end of pending data
                                              }

                                              if c.acked || c.abandoned() {
                                                  continue
                                              }

                                              if c.nSent > 1 || c.missIndicator < 3 {
                                                  continue
                                              }

                                              // RFC 4960 Sec 7.2.4 Fast Retransmit on Gap Reports
                                              //  3)  Determine how many of the earliest (i.e., lowest TSN) DATA chunks
                                              //      marked for retransmission will fit into a single packet, subject
                                              //      to constraint of the path MTU of the destination transport
                                              //      address to which the packet is being sent.  Call this value K.
                                              //      Retransmit those K DATA chunks in a single packet.  When a Fast
                                              //      Retransmit is being performed, the sender SHOULD ignore the value
                                              //      of cwnd and SHOULD NOT delay retransmission for this single
                                              //		packet.

                                              dataChunkSize := DATA_CHUNK_HEADER_SIZE + uint32(len(c.userData))
                                              if self.mtu < fastRetransSize+dataChunkSize {
                                                  break
                                              }

                                              fastRetransSize += dataChunkSize
                                              self.stats.inc_fast_retrans()
                                              c.nSent++
                                              self.check_partial_reliability_status(c)
                                              toFastRetrans = append(toFastRetrans, c)
                                              self.log.Tracef("[%s] fast-retransmit: tsn=%d sent=%d htna=%d",
                                                  self.name, c.tsn, c.nSent, self.fast_recover_exit_point)
                                          }

                                          if len(toFastRetrans) > 0 {
                                              raw, err := self.create_packet(toFastRetrans).marshal()
                                              if err != nil {
                                                  self.log.Warnf("[%s] failed to serialize a DATA packet to be fast-retransmitted", self.name)
                                              } else {
                                                  rawPackets = append(rawPackets, raw)
                                              }
                                          }
                                      }

                                      return rawPackets
                                  }

                                  // The caller should hold the lock
                                  fn gatherOutboundSackPackets(rawPackets [][]byte) [][]byte {
                                      if self.ack_state == ackStateImmediate {
                                          self.ack_state = ackStateIdle
                                          sack := self.create_selective_ack_chunk()
                                          self.log.Debugf("[%s] sending SACK: %s", self.name, sack.String())
                                          raw, err := self.create_packet([]chunk{sack}).marshal()
                                          if err != nil {
                                              self.log.Warnf("[%s] failed to serialize a SACK packet", self.name)
                                          } else {
                                              rawPackets = append(rawPackets, raw)
                                          }
                                      }

                                      return rawPackets
                                  }
    */
    /// The caller should hold the lock
    fn gather_outbound_forward_tsn_packets(&mut self, mut raw_packets: Vec<Bytes>) -> Vec<Bytes> {
        if self.will_send_forward_tsn {
            self.will_send_forward_tsn = false;
            if sna32gt(
                self.advanced_peer_tsn_ack_point,
                self.cumulative_tsn_ack_point,
            ) {
                let fwd_tsn = self.create_forward_tsn();
                if let Ok(raw) = self.create_packet(vec![Box::new(fwd_tsn)]).marshal() {
                    raw_packets.push(raw);
                } else {
                    log::warn!("[{}] failed to serialize a Forward TSN packet", self.name);
                }
            }
        }

        raw_packets
    }

    fn gather_outbound_shutdown_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
    ) -> (Vec<Bytes>, bool) {
        let mut ok = true;

        if self.will_send_shutdown {
            self.will_send_shutdown = false;

            let shutdown = ChunkShutdown {
                cumulative_tsn_ack: self.cumulative_tsn_ack_point,
            };

            if let Ok(raw) = self.create_packet(vec![Box::new(shutdown)]).marshal() {
                //TODO: add observer: self.t2shutdown.start(self.rto_mgr.get_rto());
                raw_packets.push(raw);
            } else {
                log::warn!("[{}] failed to serialize a Shutdown packet", self.name);
            }
        } else if self.will_send_shutdown_ack {
            self.will_send_shutdown_ack = false;

            let shutdown_ack = ChunkShutdownAck {};

            if let Ok(raw) = self.create_packet(vec![Box::new(shutdown_ack)]).marshal() {
                //TODO: add observer: self.t2shutdown.start(self.rto_mgr.get_rto());
                raw_packets.push(raw);
            } else {
                log::warn!("[{}] failed to serialize a ShutdownAck packet", self.name);
            }
        } else if self.will_send_shutdown_complete {
            self.will_send_shutdown_complete = false;

            let shutdown_complete = ChunkShutdownComplete {};

            if let Ok(raw) = self
                .create_packet(vec![Box::new(shutdown_complete)])
                .marshal()
            {
                raw_packets.push(raw);
                ok = false;
            } else {
                log::warn!(
                    "[{}] failed to serialize a ShutdownComplete packet",
                    self.name
                );
            }
        }

        (raw_packets, ok)
    }

    /// gather_outbound gathers outgoing packets. The returned bool value set to
    /// false means the association should be closed down after the final send.
    fn gather_outbound(&mut self) -> (Vec<Bytes>, bool) {
        let mut raw_packets = vec![];

        if !self.control_queue.is_empty() {
            for p in self.control_queue.drain(..) {
                if let Ok(raw) = p.marshal() {
                    raw_packets.push(raw);
                } else {
                    log::warn!("[{}] failed to serialize a control packet", self.name);
                    continue;
                }
            }
        }

        let ok = true;

        /*TODO:
        let state = self.get_state();
           match state {
            AssociationState::Established=> {
                raw_packets = self.gatherDataPacketsToRetransmit(raw_packets)
                raw_packets = self.gatherOutboundDataAndReconfigPackets(raw_packets)
                raw_packets = self.gatherOutboundFastRetransmissionPackets(raw_packets)
                raw_packets = self.gatherOutboundSackPackets(raw_packets)
                raw_packets = self.gather_outbound_forward_tsnpackets(raw_packets)
            }
            AssociationState::ShutdownPending|
            AssociationState::ShutdownSent|
            AssociationState::ShutdownReceived => {
                raw_packets = self.gatherDataPacketsToRetransmit(raw_packets)
                raw_packets = self.gatherOutboundFastRetransmissionPackets(raw_packets)
                raw_packets = self.gatherOutboundSackPackets(raw_packets)
                raw_packets, ok = self.gather_outbound_shutdown_packets(raw_packets)
            }
            AssociationState::ShutdownAckSent => {
                raw_packets, ok = self.gather_outbound_shutdown_packets(raw_packets)
            }
            _=>{}
        };*/

        (raw_packets, ok)
    }

    fn check_packet(p: &Packet) -> Result<(), Error> {
        // All packets must adhere to these rules

        // This is the SCTP sender's port number.  It can be used by the
        // receiver in combination with the source IP address, the SCTP
        // destination port, and possibly the destination IP address to
        // identify the association to which this packet belongs.  The port
        // number 0 MUST NOT be used.
        if p.source_port == 0 {
            return Err(Error::ErrSctpPacketSourcePortZero);
        }

        // This is the SCTP port number to which this packet is destined.
        // The receiving host will use this port number to de-multiplex the
        // SCTP packet to the correct receiving endpoint/application.  The
        // port number 0 MUST NOT be used.
        if p.destination_port == 0 {
            return Err(Error::ErrSctpPacketDestinationPortZero);
        }

        // Check values on the packet that are specific to a particular chunk type
        for c in &p.chunks {
            if c.as_any().downcast_ref::<ChunkInit>().is_some() {
                // An INIT or INIT ACK chunk MUST NOT be bundled with any other chunk.
                // They MUST be the only chunks present in the SCTP packets that carry
                // them.
                if p.chunks.len() != 1 {
                    return Err(Error::ErrInitChunkBundled);
                }

                // A packet containing an INIT chunk MUST have a zero Verification
                // Tag.
                if p.verification_tag != 0 {
                    return Err(Error::ErrInitChunkVerifyTagNotZero);
                }
            }
        }

        Ok(())
    }

    /// set_state atomically sets the state of the Association.
    /// The caller should hold the lock.
    fn set_state(&self, new_state: AssociationState) {
        let old_state = AssociationState::from(self.state.swap(new_state as u8, Ordering::SeqCst));
        if new_state != old_state {
            log::debug!(
                "[{}] state change: '{}' => '{}'",
                self.name,
                old_state,
                new_state,
            );
        }
    }

    /// get_state atomically returns the state of the Association.
    fn get_state(&self) -> AssociationState {
        self.state.load(Ordering::SeqCst).into()
    }

    /// bytes_sent returns the number of bytes sent
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent
        //return atomic.LoadUint64(&self.bytes_sent)
    }

    /// bytes_received returns the number of bytes received
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
        //return atomic.LoadUint64(&self.bytes_received)
    }
    /*
                                 func setSupportedExtensions(init *chunkInitCommon) {
                                     // nolint:godox
                                     // TODO RFC5061 https://tools.ietf.org/html/rfc6525#section-5.2
                                     // An implementation supporting this (Supported Extensions Parameter)
                                     // extension MUST list the ASCONF, the ASCONF-ACK, and the AUTH chunks
                                     // in its INIT and INIT-ACK parameters.
                                     init.params = append(init.params, &paramSupportedExtensions{
                                         ChunkTypes: []chunkType{ctReconfig, ctForwardTSN},
                                     })
                                 }

                                 // The caller should hold the lock.
                                 fn handleInit(p *packet, i *chunkInit) ([]*packet, error) {
                                     state := self.get_state()
                                     self.log.Debugf("[%s] chunkInit received in state '%s'", self.name, getAssociationStateString(state))

                                     // https://tools.ietf.org/html/rfc4960#section-5.2.1
                                     // Upon receipt of an INIT in the COOKIE-WAIT state, an endpoint MUST
                                     // respond with an INIT ACK using the same parameters it sent in its
                                     // original INIT chunk (including its Initiate Tag, unchanged).  When
                                     // responding, the endpoint MUST send the INIT ACK back to the same
                                     // address that the original INIT (sent by this endpoint) was sent.

                                     if state != closed && state != CookieWait && state != CookieEchoed {
                                         // 5.2.2.  Unexpected INIT in States Other than CLOSED, COOKIE-ECHOED,
                                         //        COOKIE-WAIT, and SHUTDOWN-ACK-SENT
                                         return nil, fmt.Errorf("%w: %s", errHandleInitState, getAssociationStateString(state))
                                     }

                                     // Should we be setting any of these permanently until we've ACKed further?
                                     self.my_max_num_inbound_streams = min16(i.numInboundStreams, self.my_max_num_inbound_streams)
                                     self.my_max_num_outbound_streams = min16(i.numOutboundStreams, self.my_max_num_outbound_streams)
                                     self.peer_verification_tag = i.initiateTag
                                     self.source_port = p.destination_port
                                     self.destination_port = p.source_port

                                     // 13.2 This is the last TSN received in sequence.  This value
                                     // is set initially by taking the peer's initial TSN,
                                     // received in the INIT or INIT ACK chunk, and
                                     // subtracting one from it.
                                     self.peer_last_tsn = i.initialTSN - 1

                                     for _, param := range i.params {
                                         switch v := param.(type) { // nolint:gocritic
                                         case *paramSupportedExtensions:
                                             for _, t := range v.ChunkTypes {
                                                 if t == ctForwardTSN {
                                                     self.log.Debugf("[%s] use ForwardTSN (on init)\n", self.name)
                                                     self.use_forward_tsn = true
                                                 }
                                             }
                                         }
                                     }
                                     if !self.use_forward_tsn {
                                         self.log.Warnf("[%s] not using ForwardTSN (on init)\n", self.name)
                                     }

                                     outbound := &packet{}
                                     outbound.verificationTag = self.peer_verification_tag
                                     outbound.source_port = self.source_port
                                     outbound.destination_port = self.destination_port

                                     initAck := &chunkInitAck{}

                                     initAck.initialTSN = self.my_next_tsn
                                     initAck.numOutboundStreams = self.my_max_num_outbound_streams
                                     initAck.numInboundStreams = self.my_max_num_inbound_streams
                                     initAck.initiateTag = self.my_verification_tag
                                     initAck.advertisedReceiverWindowCredit = self.max_receive_buffer_size

                                     if self.my_cookie == nil {
                                         var err error
                                         if self.my_cookie, err = newRandomStateCookie(); err != nil {
                                             return nil, err
                                         }
                                     }

                                     initAck.params = []param{self.my_cookie}

                                     setSupportedExtensions(&initAck.chunkInitCommon)

                                     outbound.chunks = []chunk{initAck}

                                     return pack(outbound), nil
                                 }

                                 // The caller should hold the lock.
                                 fn handleInitAck(p *packet, i *chunkInitAck) error {
                                     state := self.get_state()
                                     self.log.Debugf("[%s] chunkInitAck received in state '%s'", self.name, getAssociationStateString(state))
                                     if state != CookieWait {
                                         // RFC 4960
                                         // 5.2.3.  Unexpected INIT ACK
                                         //   If an INIT ACK is received by an endpoint in any state other than the
                                         //   COOKIE-WAIT state, the endpoint should discard the INIT ACK chunk.
                                         //   An unexpected INIT ACK usually indicates the processing of an old or
                                         //   duplicated INIT chunk.
                                         return nil
                                     }

                                     self.my_max_num_inbound_streams = min16(i.numInboundStreams, self.my_max_num_inbound_streams)
                                     self.my_max_num_outbound_streams = min16(i.numOutboundStreams, self.my_max_num_outbound_streams)
                                     self.peer_verification_tag = i.initiateTag
                                     self.peer_last_tsn = i.initialTSN - 1
                                     if self.source_port != p.destination_port ||
                                         self.destination_port != p.source_port {
                                         self.log.Warnf("[%s] handleInitAck: port mismatch", self.name)
                                         return nil
                                     }

                                     self.rwnd = i.advertisedReceiverWindowCredit
                                     self.log.Debugf("[%s] initial rwnd=%d", self.name, self.rwnd)

                                     // RFC 4690 Sec 7.2.1
                                     //  o  The initial value of ssthresh MAY be arbitrarily high (for
                                     //     example, implementations MAY use the size of the receiver
                                     //     advertised window).
                                     self.ssthresh = self.rwnd
                                     self.log.Tracef("[%s] updated cwnd=%d ssthresh=%d inflight=%d (INI)",
                                         self.name, self.cwnd, self.ssthresh, self.inflight_queue.getNumBytes())

                                     self.t1init.stop()
                                     self.stored_init = nil

                                     var cookieParam *paramStateCookie
                                     for _, param := range i.params {
                                         switch v := param.(type) {
                                         case *paramStateCookie:
                                             cookieParam = v
                                         case *paramSupportedExtensions:
                                             for _, t := range v.ChunkTypes {
                                                 if t == ctForwardTSN {
                                                     self.log.Debugf("[%s] use ForwardTSN (on initAck)\n", self.name)
                                                     self.use_forward_tsn = true
                                                 }
                                             }
                                         }
                                     }
                                     if !self.use_forward_tsn {
                                         self.log.Warnf("[%s] not using ForwardTSN (on initAck)\n", self.name)
                                     }
                                     if cookieParam == nil {
                                         return errInitAckNoCookie
                                     }

                                     self.stored_cookie_echo = &chunkCookieEcho{}
                                     self.stored_cookie_echo.cookie = cookieParam.cookie

                                     err := self.send_cookie_echo()
                                     if err != nil {
                                         self.log.Errorf("[%s] failed to send init: %s", self.name, err.Error())
                                     }

                                     self.t1cookie.start(self.rto_mgr.getRTO())
                                     self.set_state(CookieEchoed)
                                     return nil
                                 }

                                 // The caller should hold the lock.
                                 fn handleHeartbeat(c *chunkHeartbeat) []*packet {
                                     self.log.Tracef("[%s] chunkHeartbeat", self.name)
                                     hbi, ok := c.params[0].(*paramHeartbeatInfo)
                                     if !ok {
                                         self.log.Warnf("[%s] failed to handle Heartbeat, no ParamHeartbeatInfo", self.name)
                                     }

                                     return pack(&packet{
                                         verificationTag: self.peer_verification_tag,
                                         source_port:      self.source_port,
                                         destination_port: self.destination_port,
                                         chunks: []chunk{&chunkHeartbeatAck{
                                             params: []param{
                                                 &paramHeartbeatInfo{
                                                     heartbeatInformation: hbi.heartbeatInformation,
                                                 },
                                             },
                                         }},
                                     })
                                 }

                                 // The caller should hold the lock.
                                 fn handleCookieEcho(c *chunkCookieEcho) []*packet {
                                     state := self.get_state()
                                     self.log.Debugf("[%s] COOKIE-ECHO received in state '%s'", self.name, getAssociationStateString(state))

                                     if self.my_cookie == nil {
                                         self.log.Debugf("[%s] COOKIE-ECHO received before initialization", self.name)
                                         return nil
                                     }
                                     switch state {
                                     default:
                                         return nil
                                     case Established:
                                         if !bytes.Equal(self.my_cookie.cookie, c.cookie) {
                                             return nil
                                         }
                                     case closed, CookieWait, CookieEchoed:
                                         if !bytes.Equal(self.my_cookie.cookie, c.cookie) {
                                             return nil
                                         }

                                         self.t1init.stop()
                                         self.stored_init = nil

                                         self.t1cookie.stop()
                                         self.stored_cookie_echo = nil

                                         self.set_state(Established)
                                         self.handshakeCompletedCh <- nil
                                     }

                                     p := &packet{
                                         verificationTag: self.peer_verification_tag,
                                         source_port:      self.source_port,
                                         destination_port: self.destination_port,
                                         chunks:          []chunk{&chunkCookieAck{}},
                                     }
                                     return pack(p)
                                 }

                                 // The caller should hold the lock.
                                 fn handleCookieAck() {
                                     state := self.get_state()
                                     self.log.Debugf("[%s] COOKIE-ACK received in state '%s'", self.name, getAssociationStateString(state))
                                     if state != CookieEchoed {
                                         // RFC 4960
                                         // 5.2.5.  Handle Duplicate COOKIE-ACK.
                                         //   At any state other than COOKIE-ECHOED, an endpoint should silently
                                         //   discard a received COOKIE ACK chunk.
                                         return
                                     }

                                     self.t1cookie.stop()
                                     self.stored_cookie_echo = nil

                                     self.set_state(Established)
                                     self.handshakeCompletedCh <- nil
                                 }
    */
    // The caller should hold the lock.
    fn handle_data(&mut self, d: ChunkPayloadData) -> Option<Vec<Packet>> {
        log::trace!(
            "[{}] DATA: tsn={} immediateSack={} len={}",
            self.name,
            d.tsn,
            d.immediate_sack,
            d.user_data.len()
        );
        self.stats.inc_datas();

        let can_push = self.payload_queue.can_push(&d, self.peer_last_tsn);
        let mut stream_handle_data = false;
        if can_push {
            if let Some(_s) = self.get_or_create_stream(d.stream_identifier) {
                if self.get_my_receiver_window_credit() > 0 {
                    // Pass the new chunk to stream level as soon as it arrives
                    self.payload_queue.push(d.clone(), self.peer_last_tsn);
                    stream_handle_data = true; //s.handle_data(d.clone());
                } else {
                    // Receive buffer is full
                    if let Some(last_tsn) = self.payload_queue.get_last_tsn_received() {
                        if sna32lt(d.tsn, *last_tsn) {
                            log::debug!("[{}] receive buffer full, but accepted as this is a missing chunk with tsn={} ssn={}", self.name, d.tsn, d.stream_sequence_number);
                            self.payload_queue.push(d.clone(), self.peer_last_tsn);
                            stream_handle_data = true; //s.handle_data(d.clone());
                        }
                    } else {
                        log::debug!(
                            "[{}] receive buffer full. dropping DATA with tsn={} ssn={}",
                            self.name,
                            d.tsn,
                            d.stream_sequence_number
                        );
                    }
                }
            } else {
                // silently discard the data. (sender will retry on T3-rtx timeout)
                // see pion/sctp#30
                log::debug!("discard {}", d.stream_sequence_number);
                return None;
            }
        }

        let immediate_sack = d.immediate_sack;

        if stream_handle_data {
            if let Some(s) = self.streams.get_mut(&d.stream_identifier) {
                s.handle_data(d);
            }
        }

        self.handle_peer_last_tsn_and_acknowledgement(immediate_sack)
    }

    /// A common routine for handle_data and handle_forward_tsn routines
    /// The caller should hold the lock.
    fn handle_peer_last_tsn_and_acknowledgement(
        &mut self,
        sack_immediately: bool,
    ) -> Option<Vec<Packet>> {
        let mut reply = vec![];

        // Try to advance peer_last_tsn

        // From RFC 3758 Sec 3.6:
        //   .. and then MUST further advance its cumulative TSN point locally
        //   if possible
        // Meaning, if peer_last_tsn+1 points to a chunk that is received,
        // advance peer_last_tsn until peer_last_tsn+1 points to unreceived chunk.
        while self.payload_queue.pop(self.peer_last_tsn + 1).is_none() {
            self.peer_last_tsn += 1;

            //TODO: optimize it without clone?
            let rst_reqs: Vec<ParamOutgoingResetRequest> =
                self.reconfig_requests.values().cloned().collect();
            for rst_req in rst_reqs {
                let resp = self.reset_streams_if_any(&rst_req);
                log::debug!("[{}] RESET RESPONSE: {}", self.name, resp);
                reply.push(resp);
            }
        }

        let has_packet_loss = self.payload_queue.len() > 0;
        if has_packet_loss {
            log::trace!(
                "[{}] packetloss: {}",
                self.name,
                self.payload_queue
                    .get_gap_ack_blocks_string(self.peer_last_tsn)
            );
        }

        if (self.ack_state != AckState::Immediate
            && !sack_immediately
            && !has_packet_loss
            && self.ack_mode == AckMode::Normal)
            || self.ack_mode == AckMode::AlwaysDelay
        {
            if self.ack_state == AckState::Idle {
                self.delayed_ack_triggered = true;
            } else {
                self.immediate_ack_triggered = true;
            }
        } else {
            self.immediate_ack_triggered = true;
        }

        Some(reply)
    }

    /// The caller should hold the lock.
    fn get_my_receiver_window_credit(&self) -> u32 {
        let mut bytes_queued = 0;
        for s in self.streams.values() {
            bytes_queued += s.get_num_bytes_in_reassembly_queue() as u32;
        }

        if bytes_queued >= self.max_receive_buffer_size {
            0
        } else {
            self.max_receive_buffer_size - bytes_queued
        }
    }
    /*
                                             // OpenStream opens a stream
                                             fn OpenStream(streamIdentifier uint16, defaultPayloadType PayloadProtocolIdentifier) (*Stream, error) {
                                                 self.lock.Lock()
                                                 defer self.lock.Unlock()

                                                 if _, ok := self.streams[streamIdentifier]; ok {
                                                     return nil, fmt.Errorf("%w: %d", errStreamAlreadyExist, streamIdentifier)
                                                 }

                                                 s := self.create_stream(streamIdentifier, false)
                                                 s.setDefaultPayloadType(defaultPayloadType)

                                                 return s, nil
                                             }

                                             // AcceptStream accepts a stream
                                             fn AcceptStream() (*Stream, error) {
                                                 s, ok := <-self.acceptCh
                                                 if !ok {
                                                     return nil, io.EOF // no more incoming streams
                                                 }
                                                 return s, nil
                                             }
    */
    /// create_stream creates a stream. The caller should hold the lock and check no stream exists for this id.
    fn create_stream(&mut self, stream_identifier: u16, _accept: bool) -> Option<&Stream> {
        /* TODO: let s = Stream{
            //TODO: association:      a,
            stream_identifier: stream_identifier,
            reassemblyQueue:  newReassemblyQueue(stream_identifier),
            log:              self.log,
            name:             fmt.Sprintf("%d:%s", stream_identifier, self.name),
        }

        //TODO: s.readNotifier = sync.NewCond(&s.lock)

        if accept {
            select {
            case self.acceptCh <- s:
                self.streams[stream_identifier] = s
                self.log.Debugf("[%s] accepted a new stream (stream_identifier: %d)",
                    self.name, stream_identifier)
            default:
                self.log.Debugf("[%s] dropped a new stream (acceptCh size: %d)",
                    self.name, len(self.acceptCh))
                return nil
            }
        } else {
            self.streams[stream_identifier] = s
        }

        return s
         */
        self.streams.get(&stream_identifier)
    }

    /// get_or_create_stream gets or creates a stream. The caller should hold the lock.
    fn get_or_create_stream(&mut self, stream_identifier: u16) -> Option<&Stream> {
        if self.streams.contains_key(&stream_identifier) {
            self.streams.get(&stream_identifier)
        } else {
            self.create_stream(stream_identifier, true)
        }
    }

    /// The caller should hold the lock.
    async fn process_selective_ack(
        &mut self,
        d: &ChunkSelectiveAck,
    ) -> Result<(HashMap<u16, i64>, u32), Error> {
        let mut bytes_acked_per_stream = HashMap::new();

        // New ack point, so pop all ACKed packets from inflight_queue
        // We add 1 because the "currentAckPoint" has already been popped from the inflight queue
        // For the first SACK we take care of this by setting the ackpoint to cumAck - 1
        let mut i = self.cumulative_tsn_ack_point + 1;
        while sna32lte(i, d.cumulative_tsn_ack) {
            if let Some(c) = self.inflight_queue.pop(i) {
                if !c.acked {
                    // RFC 4096 sec 6.3.2.  Retransmission Timer Rules
                    //   R3)  Whenever a SACK is received that acknowledges the DATA chunk
                    //        with the earliest outstanding TSN for that address, restart the
                    //        T3-rtx timer for that address with its current RTO (if there is
                    //        still outstanding data on that address).
                    if i == self.cumulative_tsn_ack_point + 1 {
                        // T3 timer needs to be reset. Stop it for now.
                        self.t3rtx.stop().await;
                    }

                    let n_bytes_acked = c.user_data.len() as i64;

                    // Sum the number of bytes acknowledged per stream
                    if let Some(amount) = bytes_acked_per_stream.get_mut(&c.stream_identifier) {
                        *amount += n_bytes_acked;
                    } else {
                        bytes_acked_per_stream.insert(c.stream_identifier, n_bytes_acked);
                    }

                    // RFC 4960 sec 6.3.1.  RTO Calculation
                    //   C4)  When data is in flight and when allowed by rule C5 below, a new
                    //        RTT measurement MUST be made each round trip.  Furthermore, new
                    //        RTT measurements SHOULD be made no more than once per round trip
                    //        for a given destination transport address.
                    //   C5)  Karn's algorithm: RTT measurements MUST NOT be made using
                    //        packets that were retransmitted (and thus for which it is
                    //        ambiguous whether the reply was for the first instance of the
                    //        chunk or for a later instance)
                    if c.nsent == 1 && sna32gte(c.tsn, self.min_tsn2measure_rtt) {
                        self.min_tsn2measure_rtt = self.my_next_tsn;
                        let rtt = match SystemTime::now().duration_since(c.since) {
                            Ok(rtt) => rtt,
                            Err(_) => return Err(Error::ErrInvalidSystemTime),
                        };
                        let srtt = self.rto_mgr.set_new_rtt(rtt.as_millis() as u64);
                        log::trace!(
                            "[{}] SACK: measured-rtt={} srtt={} new-rto={}",
                            self.name,
                            rtt.as_millis(),
                            srtt,
                            self.rto_mgr.get_rto()
                        );
                    }
                }

                if self.in_fast_recovery && c.tsn == self.fast_recover_exit_point {
                    log::debug!("[{}] exit fast-recovery", self.name);
                    self.in_fast_recovery = false;
                }
            } else {
                return Err(Error::ErrInflightQueueTsnPop);
            }

            i += 1;
        }

        let mut htna = d.cumulative_tsn_ack;

        // Mark selectively acknowledged chunks as "acked"
        for g in &d.gap_ack_blocks {
            for i in g.start..=g.end {
                let tsn = d.cumulative_tsn_ack + i as u32;

                let (is_existed, is_acked) = if let Some(c) = self.inflight_queue.get(tsn) {
                    (true, c.acked)
                } else {
                    (false, false)
                };
                let n_bytes_acked = if is_existed && !is_acked {
                    self.inflight_queue.mark_as_acked(tsn) as i64
                } else {
                    0
                };

                if let Some(c) = self.inflight_queue.get(tsn) {
                    if !is_acked {
                        // Sum the number of bytes acknowledged per stream
                        if let Some(amount) = bytes_acked_per_stream.get_mut(&c.stream_identifier) {
                            *amount += n_bytes_acked;
                        } else {
                            bytes_acked_per_stream.insert(c.stream_identifier, n_bytes_acked);
                        }

                        log::trace!("[{}] tsn={} has been sacked", self.name, c.tsn);

                        if c.nsent == 1 {
                            self.min_tsn2measure_rtt = self.my_next_tsn;
                            let rtt = match SystemTime::now().duration_since(c.since) {
                                Ok(rtt) => rtt,
                                Err(_) => return Err(Error::ErrInvalidSystemTime),
                            };
                            let srtt = self.rto_mgr.set_new_rtt(rtt.as_millis() as u64);
                            log::trace!(
                                "[{}] SACK: measured-rtt={} srtt={} new-rto={}",
                                self.name,
                                rtt.as_millis(),
                                srtt,
                                self.rto_mgr.get_rto()
                            );
                        }

                        if sna32lt(htna, tsn) {
                            htna = tsn;
                        }
                    }
                } else {
                    return Err(Error::ErrTsnRequestNotExist);
                }
            }
        }

        Ok((bytes_acked_per_stream, htna))
    }

    /// The caller should hold the lock.
    async fn on_cumulative_tsn_ack_point_advanced(&mut self, total_bytes_acked: i64) {
        // RFC 4096, sec 6.3.2.  Retransmission Timer Rules
        //   R2)  Whenever all outstanding data sent to an address have been
        //        acknowledged, turn off the T3-rtx timer of that address.
        if self.inflight_queue.len() == 0 {
            log::trace!(
                "[{}] SACK: no more packet in-flight (pending={})",
                self.name,
                self.pending_queue.len()
            );
            self.t3rtx.stop().await;
        } else {
            log::trace!("[{}] T3-rtx timer start (pt2)", self.name);
            //TODO: self.t3rtx.start(self.rto_mgr.getRTO());
        }

        // Update congestion control parameters
        if self.cwnd <= self.ssthresh {
            // RFC 4096, sec 7.2.1.  Slow-Start
            //   o  When cwnd is less than or equal to ssthresh, an SCTP endpoint MUST
            //		use the slow-start algorithm to increase cwnd only if the current
            //      congestion window is being fully utilized, an incoming SACK
            //      advances the Cumulative TSN Ack Point, and the data sender is not
            //      in Fast Recovery.  Only when these three conditions are met can
            //      the cwnd be increased; otherwise, the cwnd MUST not be increased.
            //		If these conditions are met, then cwnd MUST be increased by, at
            //      most, the lesser of 1) the total size of the previously
            //      outstanding DATA chunk(s) acknowledged, and 2) the destination's
            //      path MTU.
            if !self.in_fast_recovery && self.pending_queue.len() > 0 {
                self.cwnd += std::cmp::min(total_bytes_acked as u32, self.cwnd); // TCP way
                                                                                 // self.cwnd += min32(uint32(total_bytes_acked), self.mtu) // SCTP way (slow)
                log::trace!(
                    "[{}] updated cwnd={} ssthresh={} acked={} (SS)",
                    self.name,
                    self.cwnd,
                    self.ssthresh,
                    total_bytes_acked
                );
            } else {
                log::trace!(
                    "[{}] cwnd did not grow: cwnd={} ssthresh={} acked={} FR={} pending={}",
                    self.name,
                    self.cwnd,
                    self.ssthresh,
                    total_bytes_acked,
                    self.in_fast_recovery,
                    self.pending_queue.len()
                );
            }
        } else {
            // RFC 4096, sec 7.2.2.  Congestion Avoidance
            //   o  Whenever cwnd is greater than ssthresh, upon each SACK arrival
            //      that advances the Cumulative TSN Ack Point, increase
            //      partial_bytes_acked by the total number of bytes of all new chunks
            //      acknowledged in that SACK including chunks acknowledged by the new
            //      Cumulative TSN Ack and by Gap Ack Blocks.
            self.partial_bytes_acked += total_bytes_acked as u32;

            //   o  When partial_bytes_acked is equal to or greater than cwnd and
            //      before the arrival of the SACK the sender had cwnd or more bytes
            //      of data outstanding (i.e., before arrival of the SACK, flight size
            //      was greater than or equal to cwnd), increase cwnd by MTU, and
            //      reset partial_bytes_acked to (partial_bytes_acked - cwnd).
            if self.partial_bytes_acked >= self.cwnd && self.pending_queue.len() > 0 {
                self.partial_bytes_acked -= self.cwnd;
                self.cwnd += self.mtu;
                log::trace!(
                    "[{}] updated cwnd={} ssthresh={} acked={} (CA)",
                    self.name,
                    self.cwnd,
                    self.ssthresh,
                    total_bytes_acked
                );
            }
        }
    }

    /// The caller should hold the lock.
    fn process_fast_retransmission(
        &mut self,
        cum_tsn_ack_point: u32,
        htna: u32,
        cum_tsn_ack_point_advanced: bool,
    ) -> Result<(), Error> {
        // HTNA algorithm - RFC 4960 Sec 7.2.4
        // Increment missIndicator of each chunks that the SACK reported missing
        // when either of the following is met:
        // a)  Not in fast-recovery
        //     miss indications are incremented only for missing TSNs prior to the
        //     highest TSN newly acknowledged in the SACK.
        // b)  In fast-recovery AND the Cumulative TSN Ack Point advanced
        //     the miss indications are incremented for all TSNs reported missing
        //     in the SACK.
        if !self.in_fast_recovery || cum_tsn_ack_point_advanced {
            let max_tsn = if !self.in_fast_recovery {
                // a) increment only for missing TSNs prior to the HTNA
                htna
            } else {
                // b) increment for all TSNs reported missing
                cum_tsn_ack_point + (self.inflight_queue.len() as u32) + 1
            };

            let mut tsn = cum_tsn_ack_point + 1;
            while sna32lt(tsn, max_tsn) {
                if let Some(c) = self.inflight_queue.get_mut(tsn) {
                    if !c.acked && !c.abandoned() && c.miss_indicator < 3 {
                        c.miss_indicator += 1;
                        if c.miss_indicator == 3 && !self.in_fast_recovery {
                            // 2)  If not in Fast Recovery, adjust the ssthresh and cwnd of the
                            //     destination address(es) to which the missing DATA chunks were
                            //     last sent, according to the formula described in Section 7.2.3.
                            self.in_fast_recovery = true;
                            self.fast_recover_exit_point = htna;
                            self.ssthresh = std::cmp::max(self.cwnd / 2, 4 * self.mtu);
                            self.cwnd = self.ssthresh;
                            self.partial_bytes_acked = 0;
                            self.will_retransmit_fast = true;

                            log::trace!(
                                "[{}] updated cwnd={} ssthresh={} inflight={} (FR)",
                                self.name,
                                self.cwnd,
                                self.ssthresh,
                                self.inflight_queue.get_num_bytes()
                            );
                        }
                    }
                } else {
                    return Err(Error::ErrTsnRequestNotExist);
                }

                tsn += 1;
            }
        }

        if self.in_fast_recovery && cum_tsn_ack_point_advanced {
            self.will_retransmit_fast = true;
        }

        Ok(())
    }

    /// The caller should hold the lock.
    async fn handle_sack(&mut self, d: ChunkSelectiveAck) -> Result<(), Error> {
        log::trace!(
            "[{}] SACK: cumTSN={} a_rwnd={}",
            self.name,
            d.cumulative_tsn_ack,
            d.advertised_receiver_window_credit
        );
        let state = self.get_state();
        if state != AssociationState::Established
            && state != AssociationState::ShutdownPending
            && state != AssociationState::ShutdownReceived
        {
            return Ok(());
        }

        self.stats.inc_sacks();

        if sna32gt(self.cumulative_tsn_ack_point, d.cumulative_tsn_ack) {
            // RFC 4960 sec 6.2.1.  Processing a Received SACK
            // D)
            //   i) If Cumulative TSN Ack is less than the Cumulative TSN Ack
            //      Point, then drop the SACK.  Since Cumulative TSN Ack is
            //      monotonically increasing, a SACK whose Cumulative TSN Ack is
            //      less than the Cumulative TSN Ack Point indicates an out-of-
            //      order SACK.

            log::debug!(
                "[{}] SACK Cumulative ACK {} is older than ACK point {}",
                self.name,
                d.cumulative_tsn_ack,
                self.cumulative_tsn_ack_point
            );

            return Ok(());
        }

        // Process selective ack
        let (bytes_acked_per_stream, htna) = self.process_selective_ack(&d).await?;

        let mut total_bytes_acked = 0;
        for n_bytes_acked in bytes_acked_per_stream.values() {
            total_bytes_acked += *n_bytes_acked;
        }

        let mut cum_tsn_ack_point_advanced = false;
        if sna32lt(self.cumulative_tsn_ack_point, d.cumulative_tsn_ack) {
            log::trace!(
                "[{}] SACK: cumTSN advanced: {} -> {}",
                self.name,
                self.cumulative_tsn_ack_point,
                d.cumulative_tsn_ack
            );

            self.cumulative_tsn_ack_point = d.cumulative_tsn_ack;
            cum_tsn_ack_point_advanced = true;
            self.on_cumulative_tsn_ack_point_advanced(total_bytes_acked)
                .await;
        }

        for (si, n_bytes_acked) in &bytes_acked_per_stream {
            if let Some(s) = self.streams.get_mut(si) {
                s.on_buffer_released(*n_bytes_acked);
            }
        }

        // New rwnd value
        // RFC 4960 sec 6.2.1.  Processing a Received SACK
        // D)
        //   ii) Set rwnd equal to the newly received a_rwnd minus the number
        //       of bytes still outstanding after processing the Cumulative
        //       TSN Ack and the Gap Ack Blocks.

        // bytes acked were already subtracted by markAsAcked() method
        let bytes_outstanding = self.inflight_queue.get_num_bytes() as u32;
        if bytes_outstanding >= d.advertised_receiver_window_credit {
            self.rwnd = 0;
        } else {
            self.rwnd = d.advertised_receiver_window_credit - bytes_outstanding;
        }

        self.process_fast_retransmission(d.cumulative_tsn_ack, htna, cum_tsn_ack_point_advanced)?;

        if self.use_forward_tsn {
            // RFC 3758 Sec 3.5 C1
            if sna32lt(
                self.advanced_peer_tsn_ack_point,
                self.cumulative_tsn_ack_point,
            ) {
                self.advanced_peer_tsn_ack_point = self.cumulative_tsn_ack_point
            }

            // RFC 3758 Sec 3.5 C2
            let mut i = self.advanced_peer_tsn_ack_point + 1;
            while let Some(c) = self.inflight_queue.get(i) {
                if !c.abandoned() {
                    break;
                }
                self.advanced_peer_tsn_ack_point = i;
                i += 1;
            }

            // RFC 3758 Sec 3.5 C3
            if sna32gt(
                self.advanced_peer_tsn_ack_point,
                self.cumulative_tsn_ack_point,
            ) {
                self.will_send_forward_tsn = true;
            }
            self.awake_write_loop();
        }

        self.postprocess_sack(state, cum_tsn_ack_point_advanced);

        Ok(())
    }

    /// The caller must hold the lock. This method was only added because the
    /// linter was complaining about the "cognitive complexity" of handle_sack.
    fn postprocess_sack(&mut self, state: AssociationState, mut should_awake_write_loop: bool) {
        if self.inflight_queue.len() > 0 {
            // Start timer. (noop if already started)
            log::trace!("[{}] T3-rtx timer start (pt3)", self.name);
            //TODO: self.t3rtx.start(self.rto_mgr.get_rto());
        } else if state == AssociationState::ShutdownPending {
            // No more outstanding, send shutdown.
            should_awake_write_loop = true;
            self.will_send_shutdown = true;
            self.set_state(AssociationState::ShutdownSent);
        } else if state == AssociationState::ShutdownReceived {
            // No more outstanding, send shutdown ack.
            should_awake_write_loop = true;
            self.will_send_shutdown_ack = true;
            self.set_state(AssociationState::ShutdownAckSent);
        }

        if should_awake_write_loop {
            self.awake_write_loop();
        }
    }

    /// The caller should hold the lock.
    fn handle_shutdown(&mut self, _: ChunkShutdown) {
        let state = self.get_state();

        if state == AssociationState::Established {
            if self.inflight_queue.len() > 0 {
                self.set_state(AssociationState::ShutdownReceived);
            } else {
                // No more outstanding, send shutdown ack.
                self.will_send_shutdown_ack = true;
                self.set_state(AssociationState::ShutdownAckSent);

                self.awake_write_loop();
            }
        } else if state == AssociationState::ShutdownSent {
            // self.cumulative_tsn_ack_point = c.cumulative_tsn_ack

            self.will_send_shutdown_ack = true;
            self.set_state(AssociationState::ShutdownAckSent);

            self.awake_write_loop();
        }
    }

    /// The caller should hold the lock.
    async fn handle_shutdown_ack(&mut self, _: ChunkShutdownAck) {
        let state = self.get_state();
        if state == AssociationState::ShutdownSent || state == AssociationState::ShutdownAckSent {
            self.t2shutdown.stop().await;
            self.will_send_shutdown_complete = true;

            self.awake_write_loop();
        }
    }

    async fn handle_shutdown_complete(&mut self, _: ChunkShutdownComplete) -> Result<(), Error> {
        let state = self.get_state();
        if state == AssociationState::ShutdownAckSent {
            self.t2shutdown.stop().await;

            self.close().await
        } else {
            Ok(())
        }
    }

    /// create_forward_tsn generates ForwardTSN chunk.
    /// This method will be be called if use_forward_tsn is set to false.
    /// The caller should hold the lock.
    fn create_forward_tsn(&self) -> ChunkForwardTsn {
        // RFC 3758 Sec 3.5 C4
        let mut stream_map: HashMap<u16, u16> = HashMap::new(); // to report only once per SI
        let mut i = self.cumulative_tsn_ack_point + 1;
        while sna32lte(i, self.advanced_peer_tsn_ack_point) {
            if let Some(c) = self.inflight_queue.get(i) {
                if let Some(ssn) = stream_map.get(&c.stream_identifier) {
                    if sna16lt(*ssn, c.stream_sequence_number) {
                        // to report only once with greatest SSN
                        stream_map.insert(c.stream_identifier, c.stream_sequence_number);
                    }
                } else {
                    stream_map.insert(c.stream_identifier, c.stream_sequence_number);
                }
            } else {
                break;
            }

            i += 1;
        }

        let mut fwd_tsn = ChunkForwardTsn {
            new_cumulative_tsn: self.advanced_peer_tsn_ack_point,
            streams: vec![],
        };

        let mut stream_str = String::new();
        for (si, ssn) in &stream_map {
            stream_str += format!("(si={} ssn={})", si, ssn).as_str();
            fwd_tsn.streams.push(ChunkForwardTsnStream {
                identifier: *si,
                sequence: *ssn,
            });
        }
        log::trace!(
            "[{}] building fwd_tsn: newCumulativeTSN={} cumTSN={} - {}",
            self.name,
            fwd_tsn.new_cumulative_tsn,
            self.cumulative_tsn_ack_point,
            stream_str
        );

        fwd_tsn
    }

    /// create_packet wraps chunks in a packet.
    /// The caller should hold the read lock.
    fn create_packet(&self, chunks: Vec<Box<dyn Chunk>>) -> Packet {
        Packet {
            verification_tag: self.peer_verification_tag,
            source_port: self.source_port,
            destination_port: self.destination_port,
            chunks,
        }
    }

    /// The caller should hold the lock.
    async fn handle_reconfig(&mut self, c: ChunkReconfig) -> Result<Vec<Packet>, Error> {
        log::trace!("[{}] handle_reconfig", self.name);

        let mut pp = vec![];

        if let Some(param_a) = &c.param_a {
            if let Some(p) = self.handle_reconfig_param(param_a).await? {
                pp.push(p);
            }
        }

        if let Some(param_b) = &c.param_b {
            if let Some(p) = self.handle_reconfig_param(param_b).await? {
                pp.push(p);
            }
        }

        Ok(pp)
    }

    /// The caller should hold the lock.
    fn handle_forward_tsn(&mut self, c: ChunkForwardTsn) -> Option<Vec<Packet>> {
        log::trace!("[{}] FwdTSN: {}", self.name, c.to_string());

        if !self.use_forward_tsn {
            log::warn!("[{}] received FwdTSN but not enabled", self.name);
            // Return an error chunk
            let cerr = ChunkError {
                error_causes: vec![ErrorCauseUnrecognizedChunkType::default()],
            };

            let outbound = Packet {
                verification_tag: self.peer_verification_tag,
                source_port: self.source_port,
                destination_port: self.destination_port,
                chunks: vec![Box::new(cerr)],
            };
            return Some(vec![outbound]);
        }

        // From RFC 3758 Sec 3.6:
        //   Note, if the "New Cumulative TSN" value carried in the arrived
        //   FORWARD TSN chunk is found to be behind or at the current cumulative
        //   TSN point, the data receiver MUST treat this FORWARD TSN as out-of-
        //   date and MUST NOT update its Cumulative TSN.  The receiver SHOULD
        //   send a SACK to its peer (the sender of the FORWARD TSN) since such a
        //   duplicate may indicate the previous SACK was lost in the network.

        log::trace!(
            "[{}] should send ack? newCumTSN={} peer_last_tsn={}",
            self.name,
            c.new_cumulative_tsn,
            self.peer_last_tsn
        );
        if sna32lte(c.new_cumulative_tsn, self.peer_last_tsn) {
            log::trace!("[{}] sending ack on Forward TSN", self.name);
            self.ack_state = AckState::Immediate;
            self.ack_timer.stop();
            self.awake_write_loop();
            return None;
        }

        // From RFC 3758 Sec 3.6:
        //   the receiver MUST perform the same TSN handling, including duplicate
        //   detection, gap detection, SACK generation, cumulative TSN
        //   advancement, etc. as defined in RFC 2960 [2]---with the following
        //   exceptions and additions.

        //   When a FORWARD TSN chunk arrives, the data receiver MUST first update
        //   its cumulative TSN point to the value carried in the FORWARD TSN
        //   chunk,

        // Advance peer_last_tsn
        while sna32lt(self.peer_last_tsn, c.new_cumulative_tsn) {
            self.payload_queue.pop(self.peer_last_tsn + 1); // may not exist
            self.peer_last_tsn += 1;
        }

        // Report new peer_last_tsn value and abandoned largest SSN value to
        // corresponding streams so that the abandoned chunks can be removed
        // from the reassemblyQueue.
        for forwarded in &c.streams {
            if let Some(s) = self.streams.get_mut(&forwarded.identifier) {
                s.handle_forward_tsn_for_ordered(forwarded.sequence);
            }
        }

        // TSN may be forewared for unordered chunks. ForwardTSN chunk does not
        // report which stream identifier it skipped for unordered chunks.
        // Therefore, we need to broadcast this event to all existing streams for
        // unordered chunks.
        // See https://github.com/pion/sctp/issues/106
        for s in self.streams.values_mut() {
            s.handle_forward_tsn_for_unordered(c.new_cumulative_tsn);
        }

        self.handle_peer_last_tsn_and_acknowledgement(false)
    }

    fn send_reset_request(&mut self, stream_identifier: u16) -> Result<(), Error> {
        let state = self.get_state();
        if state != AssociationState::Established {
            return Err(Error::ErrResetPacketInStateNotExist);
        }

        // Create DATA chunk which only contains valid stream identifier with
        // nil userData and use it as a EOS from the stream.
        let c = ChunkPayloadData {
            stream_identifier,
            beginning_fragment: true,
            ending_fragment: true,
            user_data: Bytes::new(),
            ..Default::default()
        };

        self.pending_queue.push(c);
        self.awake_write_loop();

        Ok(())
    }

    /// The caller should hold the lock.
    #[allow(clippy::borrowed_box)]
    async fn handle_reconfig_param(
        &mut self,
        raw: &Box<dyn Param>,
    ) -> Result<Option<Packet>, Error> {
        if let Some(p) = raw.as_any().downcast_ref::<ParamOutgoingResetRequest>() {
            self.reconfig_requests
                .insert(p.reconfig_request_sequence_number, p.clone());
            Ok(Some(self.reset_streams_if_any(p)))
        } else if let Some(p) = raw.as_any().downcast_ref::<ParamReconfigResponse>() {
            self.reconfigs.remove(&p.reconfig_response_sequence_number);
            if self.reconfigs.is_empty() {
                self.treconfig.stop().await;
            }
            Ok(None)
        } else {
            Err(Error::ErrParamterType)
        }
    }

    /// The caller should hold the lock.
    fn reset_streams_if_any(&mut self, p: &ParamOutgoingResetRequest) -> Packet {
        let mut result = ReconfigResult::SuccessPerformed;
        if sna32lte(p.sender_last_tsn, self.peer_last_tsn) {
            log::debug!(
                "[{}] resetStream(): senderLastTSN={} <= peer_last_tsn={}",
                self.name,
                p.sender_last_tsn,
                self.peer_last_tsn
            );
            for id in &p.stream_identifiers {
                if let Some(s) = self.streams.get(id) {
                    let stream_identifier = s.stream_identifier;
                    self.unregister_stream(stream_identifier, Error::ErrEof);
                }
            }
            self.reconfig_requests
                .remove(&p.reconfig_request_sequence_number);
        } else {
            log::debug!(
                "[{}] resetStream(): senderLastTSN={} > peer_last_tsn={}",
                self.name,
                p.sender_last_tsn,
                self.peer_last_tsn
            );
            result = ReconfigResult::InProgress;
        }

        self.create_packet(vec![Box::new(ChunkReconfig {
            param_a: Some(Box::new(ParamReconfigResponse {
                reconfig_response_sequence_number: p.reconfig_request_sequence_number,
                result,
            })),
            param_b: None,
        })])
    }

    /// Move the chunk peeked with self.pending_queue.peek() to the inflight_queue.
    /// The caller should hold the lock.
    fn move_pending_data_chunk_to_inflight_queue(
        &mut self,
        beginning_fragment: bool,
        unordered: bool,
    ) -> Option<ChunkPayloadData> {
        if let Some(mut c) = self.pending_queue.pop(beginning_fragment, unordered) {
            // Mark all fragements are in-flight now
            if c.ending_fragment {
                c.set_all_inflight();
            }

            // Assign TSN
            c.tsn = self.generate_next_tsn();

            c.since = SystemTime::now(); // use to calculate RTT and also for maxPacketLifeTime
            c.nsent = 1; // being sent for the first time

            self.check_partial_reliability_status(&c);

            log::trace!(
                "[{}] sending ppi={} tsn={} ssn={} sent={} len={} ({},{})",
                self.name,
                c.payload_type,
                c.tsn,
                c.stream_sequence_number,
                c.nsent,
                c.user_data.len(),
                c.beginning_fragment,
                c.ending_fragment
            );

            self.inflight_queue.push_no_check(c.clone());

            Some(c)
        } else {
            log::error!("[{}] failed to pop from pending queue", self.name);
            None
        }
    }

    /// pop_pending_data_chunks_to_send pops chunks from the pending queues as many as
    /// the cwnd and rwnd allows to send.
    /// The caller should hold the lock.
    fn pop_pending_data_chunks_to_send(&mut self) -> (Vec<ChunkPayloadData>, Vec<u16>) {
        let mut chunks = vec![];
        let mut sis_to_reset = vec![]; // stream identifiers to reset
        let is_empty = self.pending_queue.len() == 0;
        if !is_empty {
            // RFC 4960 sec 6.1.  Transmission of DATA Chunks
            //   A) At any given time, the data sender MUST NOT transmit new data to
            //      any destination transport address if its peer's rwnd indicates
            //      that the peer has no buffer space (i.e., rwnd is 0; see Section
            //      6.2.1).  However, regardless of the value of rwnd (including if it
            //      is 0), the data sender can always have one DATA chunk in flight to
            //      the receiver if allowed by cwnd (see rule B, below).

            while let Some(c) = self.pending_queue.peek() {
                let (beginning_fragment, unordered, data_len, stream_identifier) = (
                    c.beginning_fragment,
                    c.unordered,
                    c.user_data.len(),
                    c.stream_identifier,
                );

                if data_len == 0 {
                    sis_to_reset.push(stream_identifier);
                    if self
                        .pending_queue
                        .pop(beginning_fragment, unordered)
                        .is_none()
                    {
                        log::error!("failed to pop from pending queue");
                    }
                    continue;
                }

                if self.inflight_queue.get_num_bytes() + data_len > self.cwnd as usize {
                    break; // would exceeds cwnd
                }

                if data_len > self.rwnd as usize {
                    break; // no more rwnd
                }

                self.rwnd -= data_len as u32;

                if let Some(chunk) =
                    self.move_pending_data_chunk_to_inflight_queue(beginning_fragment, unordered)
                {
                    chunks.push(chunk);
                }
            }

            // the data sender can always have one DATA chunk in flight to the receiver
            if chunks.is_empty() && self.inflight_queue.len() == 0 {
                // Send zero window probe
                if let Some(c) = self.pending_queue.peek() {
                    let (beginning_fragment, unordered) = (c.beginning_fragment, c.unordered);

                    if let Some(chunk) = self
                        .move_pending_data_chunk_to_inflight_queue(beginning_fragment, unordered)
                    {
                        chunks.push(chunk);
                    }
                }
            }
        }

        (chunks, sis_to_reset)
    }

    /// bundle_data_chunks_into_packets packs DATA chunks into packets. It tries to bundle
    /// DATA chunks into a packet so long as the resulting packet size does not exceed
    /// the path MTU.
    /// The caller should hold the lock.
    fn bundle_data_chunks_into_packets(&self, chunks: Vec<ChunkPayloadData>) -> Vec<Packet> {
        let mut packets = vec![];
        let mut chunks_to_send = vec![];
        let mut bytes_in_packet = COMMON_HEADER_SIZE;

        for c in chunks {
            // RFC 4960 sec 6.1.  Transmission of DATA Chunks
            //   Multiple DATA chunks committed for transmission MAY be bundled in a
            //   single packet.  Furthermore, DATA chunks being retransmitted MAY be
            //   bundled with new DATA chunks, as long as the resulting packet size
            //   does not exceed the path MTU.
            if bytes_in_packet + c.user_data.len() as u32 > self.mtu {
                packets.push(self.create_packet(chunks_to_send));
                chunks_to_send = vec![];
                bytes_in_packet = COMMON_HEADER_SIZE;
            }

            bytes_in_packet += DATA_CHUNK_HEADER_SIZE + c.user_data.len() as u32;
            chunks_to_send.push(Box::new(c));
        }

        if !chunks_to_send.is_empty() {
            packets.push(self.create_packet(chunks_to_send));
        }

        packets
    }

    /// send_payload_data sends the data chunks.
    fn send_payload_data(&mut self, chunks: Vec<ChunkPayloadData>) -> Result<(), Error> {
        let state = self.get_state();
        if state != AssociationState::Established {
            return Err(Error::ErrPayloadDataStateNotExist);
        }

        // Push the chunks into the pending queue first.
        for c in chunks {
            self.pending_queue.push(c);
        }

        self.awake_write_loop();
        Ok(())
    }

    /// The caller should hold the lock.
    fn check_partial_reliability_status(&self, c: &ChunkPayloadData) {
        if !self.use_forward_tsn {
            return;
        }

        // draft-ietf-rtcweb-data-protocol-09.txt section 6
        //	6.  Procedures
        //		All Data Channel Establishment Protocol messages MUST be sent using
        //		ordered delivery and reliable transmission.
        //
        if c.payload_type == PayloadProtocolIdentifier::Dcep {
            return;
        }

        // PR-SCTP
        if let Some(s) = self.streams.get(&c.stream_identifier) {
            if s.reliability_type == ReliabilityType::Rexmit {
                if c.nsent >= s.reliability_value {
                    c.set_abandoned(true);
                    log::trace!(
                        "[{}] marked as abandoned: tsn={} ppi={} (remix: {})",
                        self.name,
                        c.tsn,
                        c.payload_type,
                        c.nsent
                    );
                }
            } else if s.reliability_type == ReliabilityType::Timed {
                if let Ok(elapsed) = SystemTime::now().duration_since(c.since) {
                    if elapsed.as_millis() as u32 >= s.reliability_value {
                        c.set_abandoned(true);
                        log::trace!(
                            "[{}] marked as abandoned: tsn={} ppi={} (timed: {:?})",
                            self.name,
                            c.tsn,
                            c.payload_type,
                            elapsed
                        );
                    }
                }
            }
        } else {
            log::error!("[{}] stream {} not found)", self.name, c.stream_identifier);
        }
    }

    /// get_data_packets_to_retransmit is called when T3-rtx is timed out and retransmit outstanding data chunks
    /// that are not acked or abandoned yet.
    /// The caller should hold the lock.
    fn get_data_packets_to_retransmit(&mut self) -> Vec<Packet> {
        let awnd = std::cmp::min(self.cwnd, self.rwnd);
        let mut chunks = vec![];
        let mut bytes_to_send = 0;
        let mut done = false;
        let mut i = 0;
        while !done {
            let tsn = self.cumulative_tsn_ack_point + i + 1;
            if let Some(c) = self.inflight_queue.get_mut(tsn) {
                if !c.retransmit {
                    continue;
                }

                if i == 0 && self.rwnd < c.user_data.len() as u32 {
                    // Send it as a zero window probe
                    done = true;
                } else if bytes_to_send + c.user_data.len() > awnd as usize {
                    break;
                }

                // reset the retransmit flag not to retransmit again before the next
                // t3-rtx timer fires
                c.retransmit = false;
                bytes_to_send += c.user_data.len();

                c.nsent += 1;
            } else {
                break; // end of pending data
            }

            if let Some(c) = self.inflight_queue.get(tsn) {
                self.check_partial_reliability_status(c);

                log::trace!(
                    "[{}] retransmitting tsn={} ssn={} sent={}",
                    self.name,
                    c.tsn,
                    c.stream_sequence_number,
                    c.nsent
                );

                chunks.push(c.clone());
            }
            i += 1;
        }

        self.bundle_data_chunks_into_packets(chunks)
    }

    /// generate_next_tsn returns the my_next_tsn and increases it. The caller should hold the lock.
    /// The caller should hold the lock.
    fn generate_next_tsn(&mut self) -> u32 {
        let tsn = self.my_next_tsn;
        self.my_next_tsn += 1;
        tsn
    }

    /// generate_next_rsn returns the my_next_rsn and increases it. The caller should hold the lock.
    /// The caller should hold the lock.
    fn generate_next_rsn(&mut self) -> u32 {
        let rsn = self.my_next_rsn;
        self.my_next_rsn += 1;
        rsn
    }

    fn create_selective_ack_chunk(&mut self) -> ChunkSelectiveAck {
        ChunkSelectiveAck {
            cumulative_tsn_ack: self.peer_last_tsn,
            advertised_receiver_window_credit: self.get_my_receiver_window_credit(),
            gap_ack_blocks: self.payload_queue.get_gap_ack_blocks(self.peer_last_tsn),
            duplicate_tsn: self.payload_queue.pop_duplicates(),
        }
    }

    fn pack(p: Packet) -> Vec<Packet> {
        vec![p]
    }

    fn handle_chunk_start(&mut self) {
        self.delayed_ack_triggered = false;
        self.immediate_ack_triggered = false;
    }

    /*fn handleChunkEnd(&mut self) {
        if self.immediate_ack_triggered {
            self.ack_state = AckState::Immediate;
            self.ack_timer.stop();
            self.awake_write_loop();
        } else if self.delayed_ack_triggered {
            // Will send delayed ack in the next ack timeout
            self.ack_state = AckState::Delay;
            self.ack_timer.start(); //TODO:
        }
    }
                  fn handleChunk(p *packet, c chunk) error {
                      self.lock.Lock()
                      defer self.lock.Unlock()

                      var packets []*packet
                      var err error

                      if _, err = c.check(); err != nil {
                          self.log.Errorf("[ %s ] failed validating chunk: %s ", self.name, err)
                          return nil
                      }

                      switch c := c.(type) {
                      case *chunkInit:
                          packets, err = self.handleInit(p, c)

                      case *chunkInitAck:
                          err = self.handleInitAck(p, c)

                      case *chunkAbort:
                          var errStr string
                          for _, e := range c.errorCauses {
                              errStr += fmt.Sprintf("(%s)", e)
                          }
                          return fmt.Errorf("[%s] %w: %s", self.name, errChunk, errStr)

                      case *chunkError:
                          var errStr string
                          for _, e := range c.errorCauses {
                              errStr += fmt.Sprintf("(%s)", e)
                          }
                          self.log.Debugf("[%s] Error chunk, with following errors: %s", self.name, errStr)

                      case *chunkHeartbeat:
                          packets = self.handleHeartbeat(c)

                      case *chunkCookieEcho:
                          packets = self.handleCookieEcho(c)

                      case *chunkCookieAck:
                          self.handleCookieAck()

                      case *chunkPayloadData:
                          packets = self.handle_data(c)

                      case *chunkSelectiveAck:
                          err = self.handle_sack(c)

                      case *chunkReconfig:
                          packets, err = self.handle_reconfig(c)

                      case *chunkForwardTSN:
                          packets = self.handle_forward_tsn(c)

                      case *chunkShutdown:
                          self.handle_shutdown(c)
                      case *chunkShutdownAck:
                          self.handle_shutdown_ack(c)
                      case *chunkShutdownComplete:
                          err = self.handle_shutdown_complete(c)

                      default:
                          err = errChunkTypeUnhandled
                      }

                      // Log and return, the only condition that is fatal is a ABORT chunk
                      if err != nil {
                          self.log.Errorf("Failed to handle chunk: %v", err)
                          return nil
                      }

                      if len(packets) > 0 {
                          self.control_queue.pushAll(packets)
                          self.awake_write_loop()
                      }

                      return nil
                  }

    */
    fn on_retransmission_timeout(&mut self, id: RtxTimerId, n_rtos: usize) {
        match id {
            RtxTimerId::T1Init => {
                if let Err(err) = self.send_init() {
                    log::debug!(
                        "[{}] failed to retransmit init (n_rtos={}): {:?}",
                        self.name,
                        n_rtos,
                        err
                    );
                }
            }

            RtxTimerId::T1Cookie => {
                if let Err(err) = self.send_cookie_echo() {
                    log::debug!(
                        "[{}] failed to retransmit cookie-echo (n_rtos={}): {:?}",
                        self.name,
                        n_rtos,
                        err
                    );
                }
            }

            RtxTimerId::T2Shutdown => {
                log::debug!(
                    "[{}] retransmission of shutdown timeout (n_rtos={})",
                    self.name,
                    n_rtos
                );
                let state = self.get_state();
                match state {
                    AssociationState::ShutdownSent => {
                        self.will_send_shutdown = true;
                        self.awake_write_loop();
                    }
                    AssociationState::ShutdownAckSent => {
                        self.will_send_shutdown_ack = true;
                        self.awake_write_loop();
                    }
                    _ => {}
                }
            }

            RtxTimerId::T3RTX => {
                self.stats.inc_t3timeouts();

                // RFC 4960 sec 6.3.3
                //  E1)  For the destination address for which the timer expires, adjust
                //       its ssthresh with rules defined in Section 7.2.3 and set the
                //       cwnd <- MTU.
                // RFC 4960 sec 7.2.3
                //   When the T3-rtx timer expires on an address, SCTP should perform slow
                //   start by:
                //      ssthresh = max(cwnd/2, 4*MTU)
                //      cwnd = 1*MTU

                self.ssthresh = std::cmp::max(self.cwnd / 2, 4 * self.mtu);
                self.cwnd = self.mtu;
                log::trace!(
                    "[{}] updated cwnd={} ssthresh={} inflight={} (RTO)",
                    self.name,
                    self.cwnd,
                    self.ssthresh,
                    self.inflight_queue.get_num_bytes()
                );

                // RFC 3758 sec 3.5
                //  A5) Any time the T3-rtx timer expires, on any destination, the sender
                //  SHOULD try to advance the "Advanced.Peer.Ack.Point" by following
                //  the procedures outlined in C2 - C5.
                if self.use_forward_tsn {
                    // RFC 3758 Sec 3.5 C2
                    let mut i = self.advanced_peer_tsn_ack_point + 1;
                    while let Some(c) = self.inflight_queue.get(i) {
                        if !c.abandoned() {
                            break;
                        }
                        self.advanced_peer_tsn_ack_point = i;
                        i += 1;
                    }

                    // RFC 3758 Sec 3.5 C3
                    if sna32gt(
                        self.advanced_peer_tsn_ack_point,
                        self.cumulative_tsn_ack_point,
                    ) {
                        self.will_send_forward_tsn = true;
                    }
                }

                log::debug!(
                    "[{}] T3-rtx timed out: n_rtos={} cwnd={} ssthresh={}",
                    self.name,
                    n_rtos,
                    self.cwnd,
                    self.ssthresh
                );

                self.inflight_queue.mark_all_to_retrasmit();
                self.awake_write_loop();
            }

            RtxTimerId::Reconfig => {
                self.will_retransmit_reconfig = true;
                self.awake_write_loop();
            }
        }
    }

    fn on_retransmission_failure(&self, id: RtxTimerId) {
        match id {
            RtxTimerId::T1Init => {
                log::error!("[{}] retransmission failure: T1-init", self.name);
                //TODO: self.handshakeCompletedCh < -errHandshakeInitAck;
            }
            RtxTimerId::T1Cookie => {
                log::error!("[{}] retransmission failure: T1-cookie", self.name);
                //TODO: self.handshakeCompletedCh < -errHandshakeCookieEcho;
            }

            RtxTimerId::T2Shutdown => {
                log::error!("[{}] retransmission failure: T2-shutdown", self.name);
            }

            RtxTimerId::T3RTX => {
                // T3-rtx timer will not fail by design
                // Justifications:
                //  * ICE would fail if the connectivity is lost
                //  * WebRTC spec is not clear how this incident should be reported to ULP
                log::error!("[{}] retransmission failure: T3-rtx (DATA)", self.name);
            }
            _ => {}
        }
    }

    fn on_ack_timeout(&mut self) {
        log::trace!(
            "[{}] ack timed out (ack_state: {})",
            self.name,
            self.ack_state
        );
        self.stats.inc_ack_timeouts();
        self.ack_state = AckState::Immediate;
        self.awake_write_loop();
    }

    /// buffered_amount returns total amount (in bytes) of currently buffered user data.
    /// This is used only by testing.
    fn buffered_amount(&self) -> usize {
        self.pending_queue.get_num_bytes() + self.inflight_queue.get_num_bytes()
    }

    /// max_message_size returns the maximum message size you can send.
    pub fn max_message_size(&self) -> u32 {
        self.max_message_size.load(Ordering::SeqCst)
    }

    /// set_max_message_size sets the maximum message size you can send.
    pub fn set_max_message_size(&self, max_message_size: u32) {
        self.max_message_size
            .store(max_message_size, Ordering::SeqCst);
    }
}
