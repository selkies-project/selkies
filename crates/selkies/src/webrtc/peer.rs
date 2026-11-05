/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One peer connection: its transport, its negotiation, its RTP senders and receivers, its pacer,
//! and its SCTP association, each stepped on tasks of this peer alone.
//!
//! `spawn` gathers candidates and writes the offer, then runs three tasks. The media task owns
//! everything that sends RTP: the `PacedTransport` with its pacer and transport-wide state, the
//! `RtpSender`s, the `RtpReceiver`s and the router in front of them; it takes frames from the
//! display's relay, RTCP and uplink RTP from the transport, and the commands the service sends,
//! and sleeps until the pacer or a report is next due. The demux task reads the transport's
//! incoming channel and hands SCTP packets straight to the association, so a key frame being
//! packed never holds input back; everything else goes on to the media task. SCTP runs on its
//! own task (`SctpTransport::spawn`), whose outgoing packets take the pacer's data class through
//! the media task, ahead of queued video and behind audio and RTCP.
//!
//! A still screen's path probe pads the video stream (`set_padding`): the media task writes a step
//! of the asked rate every `PAD_STEP_S` through `RtpSender::send_padding`, which the pacer admits
//! only from its credit, holding about five milliseconds of its pace, with nothing queued.
//!
//! No peer shares a lock or a task with another: a frame being packed for one peer, on one tokio
//! worker, stalls neither another peer's frames nor its input.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use log::{debug, error, info, warn};
use selkies_rtc::dtls::{self, Certificate, DtlsRole as TransportDtlsRole, DtlsState};
use selkies_rtc::ice::connection::IceParameters as TransportIceParameters;
use selkies_rtc::ice::lite::LiteTimings;
use selkies_rtc::ice::{Candidate, IceConfig, IceState};
use selkies_rtc::rtp::pacer::PacerEvent;
use selkies_rtc::rtp::receiver::ReceivedFrame;
use selkies_rtc::rtp::sender::SenderTransport;
use selkies_rtc::rtp::transport::SinkError;
use selkies_rtc::rtp::twcc::{TWCC_QUEUE_MS, Window};
use selkies_rtc::rtp::{
    CodecParameters, EncodedFrame, HeaderExtensionParameters, HeaderExtensionsMap, Kind,
    PacedTransport, PacketSink, RtcpPacket, RtpPacket, RtpReceiver, RtpRouter, RtpSender,
    SendParameters, SenderHooks, TwccReceiver, clock,
};
use selkies_rtc::sctp::{
    ChannelState, DataChannel, DataChannelParameters, DtlsDatagrams, Message, SctpEvent,
    SctpTransport, Side,
};
use selkies_rtc::sdp::codecs::MediaKind;
use selkies_rtc::sdp::offer::{Session, Transceiver};
use selkies_rtc::sdp::{
    Codec, Direction, DtlsFingerprint, DtlsRole, IceCandidate, IceParameters, LocalTransport,
    SdpError, SdpType,
};
use selkies_rtc::transport::{
    Incoming, Transport, TransportConfig, TransportError, TransportEvent, TransportIo,
};
use tokio::sync::{mpsc, oneshot};

use crate::webrtc::relay::RelayTrack;

/// The connection state a peer reports, as `RTCPeerConnection.connectionState` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerState {
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

/// Which of a peer's two server-opened channels a message rode: `input` is reliable and ordered,
/// `pointer` keeps no order, so a lost motion sample holds nothing behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    Input,
    Pointer,
}

impl ChannelKind {
    pub fn label(self) -> &'static str {
        match self {
            ChannelKind::Input => "input",
            ChannelKind::Pointer => "pointer",
        }
    }
}

/// An uplink's frames, undecoded, with the track they rode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Uplink {
    Microphone,
    Webcam,
}

/// What a peer tells the service. Every call is made on one of the peer's own tasks and must not
/// block; work with a wait in it moves to a task of its own.
pub trait PeerEvents: Send + Sync + 'static {
    /// The connection state changed.
    fn state(&self, peer: &str, state: PeerState);
    /// A channel opened; its handle sends on it.
    fn channel_open(&self, peer: &str, kind: ChannelKind, channel: DataChannel);
    /// A channel closed.
    fn channel_closed(&self, peer: &str, kind: ChannelKind);
    /// A message arrived, in the channel's arrival order.
    fn message(&self, peer: &str, kind: ChannelKind, message: Message);
    /// A frame of an uplink completed.
    fn uplink_frame(&self, peer: &str, uplink: Uplink, frame: ReceivedFrame);
}

/// The sender hooks the service gives a peer's video sender.
pub type VideoHooks = Box<dyn SenderHooks + Send>;

/// What a peer is built from.
pub struct PeerSpec {
    pub peer_id: String,
    pub ice: IceConfig,
    pub certificate: Arc<Certificate>,
    pub audio_channels: u8,
    /// The video sender's codec preferences (`codecs::codec_preferences`).
    pub video_preferences: Vec<Codec>,
    /// Whether an audio sender rides this peer: the primary display's alone.
    pub audio: bool,
    /// A receive-only audio section for the microphone, its preferences, none to use the table.
    pub microphone: Option<Vec<Codec>>,
    /// A receive-only video section for the camera, its preferences.
    pub webcam: Option<Vec<Codec>>,
    /// The pacer's encoder rate, `None` to send without pacing (`webrtc_pacer` off).
    pub pace_bps: Option<u64>,
    /// What the offer's text is rewritten with once written, as the page receives it.
    pub munge: Box<dyn FnOnce(String) -> String + Send>,
    pub video_enabled: bool,
    pub audio_enabled: bool,
    pub hooks: VideoHooks,
}

/// What the media task reports each control tick (`Peer::tick`).
#[derive(Clone, Debug, Default)]
pub struct PeerTick {
    /// Transport-wide feedback over the interval; `None` when none arrived.
    pub window: Option<Window>,
    pub room_bps: Option<f64>,
    pub pace_bps: Option<u64>,
    pub snapshot: Option<selkies_rtc::rtp::pacer::Snapshot>,
    pub rtt: Option<f64>,
    /// The payload octets the audio sender has sent, None without one.
    pub audio_octets: Option<u64>,
    /// The prior payloads the audio sender's RED packets carry and the configured depth, None
    /// where it sends no RED.
    pub red_depth: Option<(usize, usize)>,
}

/// What an answer settled, for the service to act on.
#[derive(Clone, Debug, Default)]
pub struct Settled {
    /// The video codecs the answer took, in its order, without RTX and FlexFEC.
    pub video_mimes: Vec<String>,
    /// The audio codec the answer put first, lower case.
    pub audio_mime: Option<String>,
}

enum Command {
    Answer(String, oneshot::Sender<Result<Settled, SdpError>>),
    RemoteCandidate(Option<IceCandidate>),
    VideoEnabled(bool),
    AudioEnabled(bool),
    UplinkEnabled(Uplink, bool),
    SwitchCodec(String, oneshot::Sender<bool>),
    RedDistance(Option<usize>),
    RedShed(Option<usize>),
    AudioFeed(RelayTrack<AudioSample>),
    EncoderBps(u64),
    SteeredBps(Option<u64>),
    FrameFloor(usize),
    RepairBudget(Option<f64>, Option<f64>),
    Padding(f64, f64),
    RequestUplinkKeyframe,
    Tick(oneshot::Sender<PeerTick>),
    Rtt(oneshot::Sender<Option<f64>>),
    Pace(oneshot::Sender<Option<u64>>),
    Close,
}

/// An audio frame on the display's 48 kHz clock, sharing the capture's buffer.
#[derive(Clone, Debug)]
pub struct AudioSample {
    pub data: Bytes,
    pub timestamp: u32,
}

/// The handle on a running peer.
#[derive(Clone)]
pub struct Peer {
    commands: mpsc::UnboundedSender<Command>,
    transport: Arc<Transport>,
}

/// Why a peer could not start.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Sdp(#[from] SdpError),
    #[error("{0}")]
    Candidate(String),
}

/// `PacketSink` over the transport handle.
struct TransportSink {
    transport: Arc<Transport>,
}

fn sink_error(e: TransportError) -> SinkError {
    SinkError(e.to_string())
}

impl PacketSink for TransportSink {
    fn send_rtp(&mut self, packet: &[u8]) -> Result<(), SinkError> {
        self.transport
            .send_rtp(packet)
            .map(|_| ())
            .map_err(sink_error)
    }

    fn send_rtcp(&mut self, compound: &[u8]) -> Result<(), SinkError> {
        self.transport
            .send_rtcp(compound)
            .map(|_| ())
            .map_err(sink_error)
    }

    fn send_data(&mut self, packet: &[u8]) -> Result<(), SinkError> {
        self.transport.send_data(packet).map_err(sink_error)
    }

    fn peer_heard_at(&self) -> f64 {
        clock::monotonic() - self.transport.peer_heard_at().elapsed().as_secs_f64()
    }
}

/// SCTP's outgoing packets, on their way to the media task's pacer.
struct SctpOut(mpsc::UnboundedSender<Bytes>);

impl DtlsDatagrams for SctpOut {
    fn send(&self, packet: &[u8]) {
        let _ = self.0.send(Bytes::copy_from_slice(packet));
    }
}

fn to_sdp_candidate(candidate: &Candidate) -> Result<IceCandidate, PeerError> {
    IceCandidate::from_sdp(&candidate.to_sdp()).map_err(PeerError::Sdp)
}

fn to_ice_candidate(candidate: &IceCandidate) -> Result<Candidate, String> {
    Candidate::from_sdp(&candidate.to_sdp())
}

fn local_transport(transport: &Transport) -> Result<LocalTransport, PeerError> {
    let ice = transport.ice_parameters();
    Ok(LocalTransport {
        ice: IceParameters {
            username_fragment: Some(ice.username_fragment),
            password: Some(ice.password),
            ice_lite: ice.ice_lite,
        },
        candidates: transport
            .local_candidates()
            .iter()
            .map(to_sdp_candidate)
            .collect::<Result<_, _>>()?,
        candidates_complete: true,
        fingerprints: transport
            .dtls_parameters()
            .fingerprints
            .into_iter()
            .map(|f| DtlsFingerprint {
                algorithm: f.algorithm,
                value: f.value,
            })
            .collect(),
    })
}

/// A transceiver's negotiated codec in the RTP layer's terms.
pub fn codec_parameters(codec: &Codec) -> CodecParameters {
    CodecParameters {
        mime_type: codec.mime_type.clone(),
        clock_rate: codec.clock_rate,
        channels: codec.channels.unwrap_or(0),
        payload_type: codec.payload_type,
        parameters: codec
            .parameters
            .iter()
            .map(|(k, v)| (k.clone(), v.clone().unwrap_or_default()))
            .collect::<BTreeMap<_, _>>(),
        rtcp_feedback: codec
            .rtcp_feedback
            .iter()
            .map(|f| (f.kind.clone(), f.parameter.clone().unwrap_or_default()))
            .collect(),
    }
}

fn header_extensions(transceiver: &Transceiver) -> Vec<HeaderExtensionParameters> {
    transceiver
        .header_extensions
        .iter()
        .map(|h| HeaderExtensionParameters::new(h.id as u8, &h.uri))
        .collect()
}

fn send_parameters(transceiver: &Transceiver, cname: &str) -> SendParameters {
    SendParameters {
        codecs: transceiver.codecs.iter().map(codec_parameters).collect(),
        header_extensions: header_extensions(transceiver),
        mux_id: transceiver.mid.clone(),
        cname: Some(cname.to_string()),
    }
}

fn receive_parameters(transceiver: &Transceiver) -> selkies_rtc::rtp::ReceiveParameters {
    selkies_rtc::rtp::ReceiveParameters {
        codecs: transceiver.codecs.iter().map(codec_parameters).collect(),
        header_extensions: header_extensions(transceiver),
        encodings: transceiver
            .remote_encodings
            .iter()
            .map(|e| selkies_rtc::rtp::params::ReceiveEncoding {
                ssrc: e.ssrc,
                rtx_ssrc: e.rtx_ssrc,
            })
            .collect(),
        mux_id: transceiver.mid.clone(),
    }
}

/// The keys the router sorts a bundled peer's streams under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Track {
    Video,
    Audio,
    Microphone,
    Webcam,
}

/// An uplink packet routed to its track and parsed with the header extensions that track's
/// media section negotiated. Routing reads the SSRC and payload type alone, so the packet is
/// checked against no extensions first, which keeps a malformed one from teaching the router an
/// SSRC, then read with its own track's: the microphone's section carries no abs-send-time, which
/// the webcam's receive estimate needs.
fn parse_uplink(
    router: &mut RtpRouter<Track>,
    data: &Bytes,
    microphone: Option<&HeaderExtensionsMap>,
    webcam: Option<&HeaderExtensionsMap>,
) -> Option<(Track, RtpPacket)> {
    let header = RtpPacket::parse(data, &HeaderExtensionsMap::default()).ok()?;
    let track = router.route_rtp(&header)?;
    let map = match track {
        Track::Microphone => microphone?,
        Track::Webcam => webcam?,
        _ => return None,
    };
    Some((track, RtpPacket::parse(data, map).ok()?))
}

/// The step a peer's padding is written in.
const PAD_STEP_S: f64 = 0.005;

/// The queues the media task reads, made before it starts.
struct MediaChannels {
    commands: mpsc::UnboundedReceiver<Command>,
    control: mpsc::Receiver<Incoming>,
    sctp_out: mpsc::UnboundedReceiver<Bytes>,
    sctp_in: mpsc::Receiver<Bytes>,
    sctp_forward: mpsc::Sender<Bytes>,
    control_tx: mpsc::Sender<Incoming>,
}

struct Media {
    peer_id: String,
    transport: Arc<Transport>,
    paced: PacedTransport<TransportSink>,
    session: Session,
    events: Arc<dyn PeerEvents>,
    video_index: usize,
    audio_index: Option<usize>,
    mic_index: Option<usize>,
    webcam_index: Option<usize>,
    video: RtpSender,
    audio: Option<RtpSender>,
    microphone: Option<RtpReceiver>,
    webcam: Option<RtpReceiver>,
    router: RtpRouter<Track>,
    twcc_rx: TwccReceiver,
    sctp_out: mpsc::UnboundedSender<Bytes>,
    sctp: Option<SctpTransport>,
    /// The inbound SCTP packets of a negotiated association DTLS has yet to carry.
    sctp_negotiated: Option<mpsc::Receiver<Bytes>>,
    video_feed: RelayTrack<Arc<EncodedFrame>>,
    audio_feed: Option<RelayTrack<AudioSample>>,
    dtls_connected: bool,
    ice_connected: bool,
    state: PeerState,
    drain_at: Option<Instant>,
    report_at: Instant,
    remote_set: bool,
    /// The padding asked of the video stream, in bits per second, until `pad_until`
    /// (`clock::monotonic`), and when its next step is due.
    pad_bps: f64,
    pad_until: f64,
    pad_at: Option<Instant>,
}

impl Peer {
    /// Gather, write the offer, and start the peer's tasks; returns the handle and the offer text
    /// as the page receives it.
    ///
    /// # Errors
    /// No socket could be bound, or the offer could not be written.
    pub async fn spawn(
        spec: PeerSpec,
        events: Arc<dyn PeerEvents>,
        video_feed: RelayTrack<Arc<EncodedFrame>>,
        audio_feed: Option<RelayTrack<AudioSample>>,
    ) -> Result<(Peer, String), PeerError> {
        let lite = spec.ice.lite;
        let (transport, io) = Transport::new(TransportConfig {
            ice: spec.ice,
            certificate: spec.certificate,
            lite_timings: LiteTimings::default(),
        })
        .await?;
        let transport = Arc::new(transport);

        let mut session = Session::new(lite, spec.audio_channels);
        let video_index = session.add_transceiver(MediaKind::Video, Direction::SendOnly);
        session.set_codec_preferences(video_index, spec.video_preferences);
        let audio_index = spec
            .audio
            .then(|| session.add_transceiver(MediaKind::Audio, Direction::SendOnly));
        let mic_index = spec.microphone.map(|preferences| {
            let index = session.add_transceiver(MediaKind::Audio, Direction::RecvOnly);
            session.set_codec_preferences(index, preferences);
            index
        });
        let webcam_index = spec.webcam.map(|preferences| {
            let index = session.add_transceiver(MediaKind::Video, Direction::RecvOnly);
            session.set_codec_preferences(index, preferences);
            index
        });
        session.add_data_section();
        let offer = session.create_offer(&local_transport(&transport)?)?;
        session.set_local_description(SdpType::Offer, &offer)?;
        let offer = (spec.munge)(offer);

        let now = clock::monotonic();
        let paced = PacedTransport::new(
            TransportSink {
                transport: transport.clone(),
            },
            spec.pace_bps,
            now,
        );
        let mut video = RtpSender::new(Kind::Video, spec.hooks);
        video.set_enabled(spec.video_enabled);
        let audio = spec.audio.then(|| {
            let mut audio = RtpSender::new(Kind::Audio, Box::new(selkies_rtc::rtp::NoHooks));
            audio.set_enabled(spec.audio_enabled);
            audio
        });
        {
            let v = &session.transceivers[video_index];
            video.set_ssrcs(v.ssrc, v.rtx_ssrc, v.fec_ssrc);
        }
        let mut audio = audio;
        if let (Some(audio), Some(index)) = (audio.as_mut(), audio_index) {
            let a = &session.transceivers[index];
            audio.set_ssrcs(a.ssrc, a.rtx_ssrc, a.fec_ssrc);
        }
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (sctp_out, sctp_out_rx) = mpsc::unbounded_channel();
        let (sctp_in, sctp_in_rx) = mpsc::channel(4096);
        let (control_tx, control_rx) = mpsc::channel(4096);

        let reaper_events = events.clone();
        let reaper_id = spec.peer_id.clone();
        let media = Media {
            peer_id: spec.peer_id,
            transport: transport.clone(),
            paced,
            session,
            events,
            video_index,
            audio_index,
            mic_index,
            webcam_index,
            video,
            audio,
            microphone: mic_index.map(|_| RtpReceiver::new("audio")),
            webcam: webcam_index.map(|_| RtpReceiver::new("video")),
            router: RtpRouter::new(),
            twcc_rx: TwccReceiver::new(),
            sctp_out,
            sctp: None,
            sctp_negotiated: None,
            video_feed,
            audio_feed,
            dtls_connected: false,
            ice_connected: false,
            state: PeerState::Connecting,
            drain_at: None,
            report_at: Instant::now() + Duration::from_millis(500),
            pad_bps: 0.0,
            pad_until: 0.0,
            pad_at: None,
            remote_set: false,
        };
        let running = tokio::spawn(media.run(
            MediaChannels {
                commands: command_rx,
                control: control_rx,
                sctp_out: sctp_out_rx,
                sctp_in: sctp_in_rx,
                sctp_forward: sctp_in,
                control_tx,
            },
            io,
        ));
        // A panic on this peer's media task reads as this peer closing, never as a stream that
        // stopped with its connection still up.
        {
            let (transport, events, peer_id) = (transport.clone(), reaper_events, reaper_id);
            tokio::spawn(async move {
                if let Err(e) = running.await
                    && e.is_panic()
                {
                    error!(target: "webrtc", "Peer {peer_id}: its media task panicked; closing it");
                    transport.close().await;
                    events.state(&peer_id, PeerState::Closed);
                }
            });
        }
        Ok((
            Peer {
                commands,
                transport,
            },
            offer,
        ))
    }

    /// Apply the page's answer: start ICE and DTLS and the senders it negotiated.
    ///
    /// # Errors
    /// The description is refused.
    pub async fn set_answer(&self, sdp: String) -> Result<Settled, SdpError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Answer(sdp, reply))
            .map_err(|_| SdpError::State("the peer is closed".into()))?;
        answer
            .await
            .map_err(|_| SdpError::State("the peer is closed".into()))?
    }

    /// Add a candidate the page trickled, `None` for its end-of-candidates.
    pub fn add_remote_candidate(&self, candidate: Option<IceCandidate>) {
        let _ = self.commands.send(Command::RemoteCandidate(candidate));
    }

    /// Whether the video sender puts frames on the wire; a paused one drains its feed.
    pub fn set_video_enabled(&self, enabled: bool) {
        let _ = self.commands.send(Command::VideoEnabled(enabled));
    }

    /// Whether the audio sender puts frames on the wire.
    pub fn set_audio_enabled(&self, enabled: bool) {
        let _ = self.commands.send(Command::AudioEnabled(enabled));
    }

    /// Whether an uplink's frames are taken.
    pub fn set_uplink_enabled(&self, uplink: Uplink, enabled: bool) {
        let _ = self.commands.send(Command::UplinkEnabled(uplink, enabled));
    }

    /// Move the video sender to `mime` from its next frame; false where the answer did not take it.
    pub async fn switch_codec(&self, mime: &str) -> bool {
        let (reply, answer) = oneshot::channel();
        if self
            .commands
            .send(Command::SwitchCodec(mime.to_string(), reply))
            .is_err()
        {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    /// RED's redundancy for the audio sender.
    pub fn set_red_distance(&self, distance: Option<usize>) {
        let _ = self.commands.send(Command::RedDistance(distance));
    }

    /// Carry `distance` prior payloads in the running RED stream, or its configured depth
    /// (None), from the next packet on (`RtpSender::shed_red_depth`).
    pub fn shed_red(&self, distance: Option<usize>) {
        let _ = self.commands.send(Command::RedShed(distance));
    }

    /// Feed the audio sender from `feed`: the stereo companion for a page that took Opus in a
    /// surround session.
    pub fn set_audio_feed(&self, feed: RelayTrack<AudioSample>) {
        let _ = self.commands.send(Command::AudioFeed(feed));
    }

    /// The display's configured ceiling, which keeps its queue capacity.
    pub fn set_encoder_bps(&self, bps: u64) {
        let _ = self.commands.send(Command::EncoderBps(bps));
    }

    /// The rate congestion control steers the display's encoder and pacing to.
    pub fn set_steered_bps(&self, bps: Option<u64>) {
        let _ = self.commands.send(Command::SteeredBps(bps));
    }

    /// The largest cleanup frame the pacer takes whole.
    pub fn set_frame_floor(&self, bytes: usize) {
        let _ = self.commands.send(Command::FrameFloor(bytes));
    }

    /// Bound the video sender's repairs to `bps` (None: unbounded) and time its key frames by
    /// what the path delivers (`RtpSender::set_repair_budget`).
    pub fn set_repair_budget(&self, bps: Option<f64>, delivery_bps: Option<f64>) {
        let _ = self.commands.send(Command::RepairBudget(bps, delivery_bps));
    }

    /// Pad the video stream at `bps` until `until` (`clock::monotonic`), or stop where `bps` is 0.
    pub fn set_padding(&self, bps: f64, until: f64) {
        let _ = self.commands.send(Command::Padding(bps, until));
    }

    /// Ask the page's camera for a key frame.
    pub fn request_uplink_keyframe(&self) {
        let _ = self.commands.send(Command::RequestUplinkKeyframe);
    }

    /// The feedback window, and the pacer's reading, since the last tick.
    pub async fn tick(&self) -> Option<PeerTick> {
        let (reply, answer) = oneshot::channel();
        self.commands.send(Command::Tick(reply)).ok()?;
        answer.await.ok()
    }

    /// The round trip the peer's receiver reports carry for its video.
    pub async fn rtt(&self) -> Option<f64> {
        let (reply, answer) = oneshot::channel();
        self.commands.send(Command::Rtt(reply)).ok()?;
        answer.await.ok().flatten()
    }

    /// The rate the pacer paces at.
    pub async fn pace(&self) -> Option<u64> {
        let (reply, answer) = oneshot::channel();
        self.commands.send(Command::Pace(reply)).ok()?;
        answer.await.ok().flatten()
    }

    /// End the connection; `PeerEvents::state` reports `Closed` once it is.
    pub fn close(&self) {
        let _ = self.commands.send(Command::Close);
    }

    /// The pair media rides, once nominated.
    pub fn selected_pair(&self) -> Option<(Candidate, Candidate)> {
        self.transport.selected_pair()
    }

    /// The transport's packet and byte counters.
    pub fn transport_stats(&self) -> selkies_rtc::transport::TransportStats {
        self.transport.stats()
    }

    /// The transport's states, for a suite to read.
    pub fn states(&self) -> (IceState, DtlsState) {
        (self.transport.ice_state(), self.transport.dtls_state())
    }
}

/// Read the transport's incoming channel: SCTP packets go straight to the association, so a frame
/// being packed never holds input back, and the rest goes to the media task.
async fn run_demux(
    mut incoming: mpsc::Receiver<Incoming>,
    sctp: mpsc::Sender<Bytes>,
    control: mpsc::Sender<Incoming>,
) {
    while let Some(packet) = incoming.recv().await {
        match packet {
            Incoming::Data(data) => {
                if sctp.try_send(data.freeze()).is_err() {
                    debug!(target: "webrtc", "SCTP inbound queue full, dropping a packet");
                }
            }
            other => {
                if control.try_send(other).is_err() {
                    debug!(target: "webrtc", "RTP inbound queue full, dropping a packet");
                }
            }
        }
    }
}

/// Forward an association's events to the service, and open the two channels on it.
async fn run_sctp_events(
    peer_id: String,
    events: Arc<dyn PeerEvents>,
    sctp: SctpTransport,
    mut rx: mpsc::UnboundedReceiver<SctpEvent>,
) {
    let mut input = sctp
        .open_channel(DataChannelParameters::new(ChannelKind::Input.label(), true))
        .await
        .ok();
    let mut pointer = sctp
        .open_channel(DataChannelParameters::new(
            ChannelKind::Pointer.label(),
            false,
        ))
        .await
        .ok();
    let input_id = input.as_ref().map(DataChannel::id);
    let pointer_id = pointer.as_ref().map(DataChannel::id);
    let kind_of = |id: u16| {
        if Some(id) == input_id {
            Some(ChannelKind::Input)
        } else if Some(id) == pointer_id {
            Some(ChannelKind::Pointer)
        } else {
            None
        }
    };
    while let Some(event) = rx.recv().await {
        match event {
            SctpEvent::Connected => debug!(target: "webrtc", "SCTP established for peer {peer_id}"),
            SctpEvent::ChannelState(id, state) => {
                let Some(kind) = kind_of(id) else { continue };
                match state {
                    ChannelState::Open => {
                        let channel = match kind {
                            ChannelKind::Input => input.take(),
                            ChannelKind::Pointer => pointer.take(),
                        };
                        if let Some(channel) = channel {
                            events.channel_open(&peer_id, kind, channel);
                        }
                    }
                    ChannelState::Closed => events.channel_closed(&peer_id, kind),
                    _ => {}
                }
            }
            SctpEvent::Message(id, message) => {
                if let Some(kind) = kind_of(id) {
                    events.message(&peer_id, kind, message);
                }
            }
            SctpEvent::ChannelOpened(id, params) => {
                debug!(target: "webrtc", "Peer {peer_id} opened channel {id} ({}), which nothing reads", params.label);
            }
            SctpEvent::BufferedAmountLow(_) => {}
            SctpEvent::Closed => {
                for kind in [ChannelKind::Input, ChannelKind::Pointer] {
                    events.channel_closed(&peer_id, kind);
                }
                return;
            }
        }
    }
}

impl Media {
    async fn run(mut self, channels: MediaChannels, io: TransportIo) {
        let MediaChannels {
            mut commands,
            mut control,
            mut sctp_out,
            sctp_in,
            sctp_forward,
            control_tx,
        } = channels;
        let TransportIo {
            incoming,
            mut events,
        } = io;
        tokio::spawn(run_demux(incoming, sctp_forward, control_tx));
        let mut sctp_in = Some(sctp_in);
        let far = Duration::from_secs(3600);
        loop {
            let wake = self
                .drain_at
                .unwrap_or_else(|| Instant::now() + far)
                .min(self.report_at)
                .min(self.pad_at.unwrap_or_else(|| Instant::now() + far));
            tokio::select! {
                biased;
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    if !self.on_command(command, &mut sctp_in).await {
                        break;
                    }
                }
                packet = sctp_out.recv() => {
                    if let Some(packet) = packet {
                        let _ = self.paced.send_data(packet, clock::monotonic());
                    }
                }
                event = events.recv() => {
                    match event {
                        Some(event) => self.on_transport_event(event),
                        None => break,
                    }
                }
                packet = control.recv() => {
                    if let Some(packet) = packet {
                        self.on_incoming(packet);
                    }
                }
                _ = tokio::time::sleep_until(wake.into()) => {
                    let now = Instant::now();
                    if self.report_at <= now {
                        self.send_reports();
                    }
                    if self.pad_at.is_some_and(|at| at <= now) {
                        self.pad();
                    }
                }
                frame = self.video_feed.recv(), if self.dtls_connected => {
                    self.on_video(&frame);
                }
                sample = recv_audio(&mut self.audio_feed), if self.dtls_connected => {
                    self.on_audio(sample);
                }
            }
            self.drain();
        }
        self.finish().await;
    }

    async fn on_command(
        &mut self,
        command: Command,
        sctp_in: &mut Option<mpsc::Receiver<Bytes>>,
    ) -> bool {
        match command {
            Command::Answer(sdp, reply) => {
                let settled = self.apply_answer(&sdp, sctp_in).await;
                let _ = reply.send(settled);
            }
            Command::RemoteCandidate(candidate) => match candidate {
                None => {
                    let _ = self.transport.add_remote_candidate(None).await;
                }
                Some(candidate) => match to_ice_candidate(&candidate) {
                    Ok(candidate) => {
                        let _ = self.transport.add_remote_candidate(Some(candidate)).await;
                    }
                    Err(e) => {
                        warn!(target: "webrtc", "Peer {}: a candidate that does not parse: {e}", self.peer_id);
                    }
                },
            },
            Command::VideoEnabled(enabled) => self.video.set_enabled(enabled),
            Command::AudioEnabled(enabled) => {
                if let Some(audio) = self.audio.as_mut() {
                    audio.set_enabled(enabled);
                }
            }
            Command::UplinkEnabled(uplink, enabled) => {
                let receiver = match uplink {
                    Uplink::Microphone => self.microphone.as_mut(),
                    Uplink::Webcam => self.webcam.as_mut(),
                };
                if let Some(receiver) = receiver {
                    receiver.set_enabled(enabled);
                }
            }
            Command::SwitchCodec(mime, reply) => {
                let _ = reply.send(self.video.switch_codec(&mime));
            }
            Command::RedDistance(distance) => {
                if let Some(audio) = self.audio.as_mut() {
                    audio.set_red_distance(distance);
                }
            }
            Command::RedShed(distance) => {
                if let Some(audio) = self.audio.as_mut() {
                    audio.shed_red_depth(distance);
                }
            }
            Command::AudioFeed(feed) => self.audio_feed = Some(feed),
            Command::EncoderBps(bps) => {
                let now = clock::monotonic();
                if let Some(pacer) = self.paced.pacer() {
                    pacer.set_encoder_bps(bps, now);
                }
            }
            Command::SteeredBps(bps) => {
                let now = clock::monotonic();
                if let Some(pacer) = self.paced.pacer() {
                    pacer.set_steered_bps(bps, now);
                }
            }
            Command::FrameFloor(bytes) => {
                if let Some(pacer) = self.paced.pacer() {
                    pacer.set_frame_floor(bytes);
                }
            }
            Command::RepairBudget(bps, delivery_bps) => {
                self.video.set_repair_budget(bps, delivery_bps);
            }
            Command::Padding(bps, until) => {
                self.pad_bps = bps;
                self.pad_until = until;
                if bps <= 0.0 {
                    self.pad_at = None;
                } else if self.pad_at.is_none() {
                    self.pad_at = Some(Instant::now());
                }
            }
            Command::RequestUplinkKeyframe => {
                if let Some(webcam) = self.webcam.as_mut() {
                    webcam.request_keyframe();
                }
                self.flush_receiver_rtcp();
            }
            Command::Tick(reply) => {
                let now = clock::monotonic();
                let window = self.paced.twcc_state().take_window(now);
                if let Some(window) = &window {
                    let standing = window
                        .queue_ms
                        .map(|q| q.max(window.queue_rising_ms.unwrap_or(0.0)));
                    self.video.steer_fec(
                        window.loss_fraction,
                        standing.is_some_and(|q| q > TWCC_QUEUE_MS),
                    );
                }
                let (room_bps, pace_bps, snapshot) = match self.paced.pacer() {
                    Some(pacer) => (
                        pacer.room_bps(now),
                        Some(pacer.pace_bps()),
                        Some(pacer.snapshot()),
                    ),
                    None => (None, None, None),
                };
                let (audio_octets, red_depth) = match self.audio.as_mut() {
                    Some(audio) => (Some(audio.octet_count()), audio.red_depth()),
                    None => (None, None),
                };
                let _ = reply.send(PeerTick {
                    window,
                    room_bps,
                    pace_bps,
                    snapshot,
                    rtt: self.video.rtt(),
                    audio_octets,
                    red_depth,
                });
            }
            Command::Rtt(reply) => {
                let _ = reply.send(self.video.rtt());
            }
            Command::Pace(reply) => {
                let _ = reply.send(self.paced.pacer().map(|p| p.pace_bps()));
            }
            Command::Close => return false,
        }
        true
    }

    /// Read the answer, start the transports on it, and start what it negotiated.
    async fn apply_answer(
        &mut self,
        sdp: &str,
        sctp_in: &mut Option<mpsc::Receiver<Bytes>>,
    ) -> Result<Settled, SdpError> {
        let negotiated = self.session.set_remote_description(SdpType::Answer, sdp)?;
        self.remote_set = true;

        let cname = self.session.cname().to_string();
        let video = &self.session.transceivers[self.video_index];
        let mut settled = Settled {
            video_mimes: video.answered_mimes(),
            audio_mime: None,
        };
        let video_params = send_parameters(video, &cname);
        if !video.codecs.is_empty() && self.video.start(&video_params).is_ok() {
            self.router.register_sender(Track::Video, video.ssrc, true);
        }
        if let (Some(index), Some(audio)) = (self.audio_index, self.audio.as_mut()) {
            let transceiver = &self.session.transceivers[index];
            settled.audio_mime = transceiver
                .codecs
                .first()
                .map(|c| c.mime_type.to_ascii_lowercase());
            if audio.start(&send_parameters(transceiver, &cname)).is_ok() {
                self.router
                    .register_sender(Track::Audio, transceiver.ssrc, false);
            }
        }
        let uplinks = [
            (self.mic_index, Track::Microphone),
            (self.webcam_index, Track::Webcam),
        ];
        for (index, track) in uplinks {
            let Some(index) = index else { continue };
            let transceiver = &self.session.transceivers[index];
            if transceiver.codecs.is_empty() {
                continue;
            }
            let parameters = receive_parameters(transceiver);
            let receiver = match track {
                Track::Microphone => self.microphone.as_mut(),
                _ => self.webcam.as_mut(),
            };
            let Some(receiver) = receiver else { continue };
            receiver.start(&parameters);
            let ssrcs: Vec<u32> = parameters
                .encodings
                .iter()
                .flat_map(|e| std::iter::once(e.ssrc).chain(e.rtx_ssrc))
                .collect();
            let payload_types: Vec<u8> = parameters.codecs.iter().map(|c| c.payload_type).collect();
            self.router.register_receiver(track, &ssrcs, &payload_types);
        }

        let Some(ice) = negotiated.ice.clone() else {
            return Err(SdpError::Invalid(
                "an answer without ICE credentials".into(),
            ));
        };
        let ice = TransportIceParameters {
            username_fragment: ice.username_fragment.unwrap_or_default(),
            password: ice.password.unwrap_or_default(),
            ice_lite: ice.ice_lite,
        };
        let dtls_params = dtls::DtlsParameters {
            fingerprints: negotiated
                .fingerprints
                .iter()
                .map(|f| dtls::Fingerprint {
                    algorithm: f.algorithm.clone(),
                    value: f.value.clone(),
                })
                .collect(),
            role: TransportDtlsRole::Auto,
        };
        let role = match negotiated.dtls_role.unwrap_or(self.session.dtls_role()) {
            DtlsRole::Client => TransportDtlsRole::Client,
            DtlsRole::Server => TransportDtlsRole::Server,
            DtlsRole::Auto => TransportDtlsRole::Auto,
        };
        if let Err(e) = self.transport.start(ice, dtls_params, role).await {
            return Err(SdpError::State(e.to_string()));
        }
        for candidate in &negotiated.candidates {
            match to_ice_candidate(candidate) {
                Ok(candidate) => {
                    let _ = self.transport.add_remote_candidate(Some(candidate)).await;
                }
                Err(e) => {
                    warn!(target: "webrtc", "Peer {}: a candidate that does not parse: {e}", self.peer_id);
                }
            }
        }
        if negotiated.candidates_complete {
            let _ = self.transport.add_remote_candidate(None).await;
        }

        if negotiated.sctp && self.sctp_negotiated.is_none() {
            self.sctp_negotiated = sctp_in.take();
            self.start_sctp();
        }
        Ok(settled)
    }

    /// Start the association once DTLS can carry it, with what the answer negotiated.
    ///
    /// An INIT sent any earlier has no keys to leave under and is lost, so the channels would
    /// open only on its retransmission a full RTO.Initial later, with the video already up and
    /// input going nowhere. A peer's INIT that arrives first waits in the inbound channel.
    fn start_sctp(&mut self) {
        if !self.dtls_connected {
            return;
        }
        let Some(rx) = self.sctp_negotiated.take() else {
            return;
        };
        let side = if self.transport.ice_role() == "controlling" {
            Side::Client
        } else {
            Side::Server
        };
        let caps = self.session.sctp.as_ref().and_then(|s| s.remote_caps);
        let (sctp, sctp_events) =
            SctpTransport::spawn(side, caps, Arc::new(SctpOut(self.sctp_out.clone())), rx);
        tokio::spawn(run_sctp_events(
            self.peer_id.clone(),
            self.events.clone(),
            sctp.clone(),
            sctp_events,
        ));
        self.sctp = Some(sctp);
    }

    fn on_transport_event(&mut self, event: TransportEvent) {
        match event {
            TransportEvent::IceState(state) => {
                debug!(target: "webrtc", "Peer {}: ICE {}", self.peer_id, state.as_str());
                match state {
                    IceState::Connected | IceState::Completed => self.ice_connected = true,
                    IceState::Disconnected => {
                        self.ice_connected = false;
                        self.set_state(PeerState::Disconnected);
                    }
                    IceState::Failed => self.set_state(PeerState::Failed),
                    IceState::Closed => self.set_state(PeerState::Closed),
                    _ => {}
                }
            }
            TransportEvent::DtlsState(state) => {
                debug!(target: "webrtc", "Peer {}: DTLS {}", self.peer_id, state.as_str());
                match state {
                    DtlsState::Connected => {
                        self.dtls_connected = true;
                        self.start_sctp();
                    }
                    DtlsState::Failed => {
                        self.stop_sending();
                        self.set_state(PeerState::Failed);
                    }
                    DtlsState::Closed => {
                        self.stop_sending();
                        self.set_state(PeerState::Closed);
                    }
                    _ => {}
                }
            }
            TransportEvent::SelectedPair { local, remote } => {
                info!(target: "webrtc", "Peer {} rides {} {}:{} to {}:{}", self.peer_id, local.transport.as_str(), local.host, local.port, remote.host, remote.port);
            }
            TransportEvent::Closed => self.set_state(PeerState::Closed),
        }
        if self.dtls_connected && self.ice_connected {
            self.set_state(PeerState::Connected);
        }
    }

    /// The keys are gone: what the pacer still holds goes nowhere, and nothing more enters it.
    fn stop_sending(&mut self) {
        self.dtls_connected = false;
        if let Some(pacer) = self.paced.pacer() {
            pacer.close();
        }
    }

    fn set_state(&mut self, state: PeerState) {
        if self.state != state && self.state != PeerState::Closed {
            self.state = state;
            self.events.state(&self.peer_id, state);
        }
    }

    fn on_incoming(&mut self, packet: Incoming) {
        let now = clock::monotonic();
        match packet {
            Incoming::Rtcp { data, .. } => {
                let Ok(packets) = RtcpPacket::parse(&data) else {
                    debug!(target: "webrtc", "Peer {}: RTCP that does not parse", self.peer_id);
                    return;
                };
                for packet in &packets {
                    let to = self.router.route_rtcp(packet);
                    for track in to.senders {
                        match track {
                            Track::Video => {
                                self.video.handle_rtcp(packet, &mut self.paced, now);
                            }
                            Track::Audio => {
                                if let Some(audio) = self.audio.as_mut() {
                                    audio.handle_rtcp(packet, &mut self.paced, now);
                                }
                            }
                            _ => {}
                        }
                    }
                    for track in to.receivers {
                        let receiver = match track {
                            Track::Microphone => self.microphone.as_mut(),
                            Track::Webcam => self.webcam.as_mut(),
                            _ => None,
                        };
                        if let Some(receiver) = receiver {
                            receiver.handle_rtcp(packet, now);
                        }
                    }
                }
            }
            Incoming::Rtp { data, arrived } => {
                self.on_uplink_rtp(data.freeze(), arrived, now);
            }
            Incoming::Data(_) => {}
        }
    }

    fn on_uplink_rtp(&mut self, data: Bytes, arrived: Instant, now: f64) {
        let maps = (
            self.microphone
                .as_ref()
                .map(|r| r.header_extensions().clone()),
            self.webcam.as_ref().map(|r| r.header_extensions().clone()),
        );
        let Some((track, packet)) =
            parse_uplink(&mut self.router, &data, maps.0.as_ref(), maps.1.as_ref())
        else {
            return;
        };
        let (receiver, uplink) = match track {
            Track::Microphone => (self.microphone.as_mut(), Uplink::Microphone),
            Track::Webcam => (self.webcam.as_mut(), Uplink::Webcam),
            _ => return,
        };
        let Some(receiver) = receiver else { return };
        let waited = arrived.elapsed().as_secs_f64();
        let arrival_ms = ((now - waited) * 1000.0) as i64;
        if let Some(frame) = receiver.handle_rtp(packet, arrival_ms, now, &mut self.twcc_rx) {
            self.events.uplink_frame(&self.peer_id, uplink, frame);
        }
        self.flush_receiver_rtcp();
    }

    fn flush_receiver_rtcp(&mut self) {
        let mut out: Vec<Vec<u8>> = Vec::new();
        if let Some(receiver) = self.microphone.as_mut() {
            out.extend(receiver.take_rtcp());
        }
        if let Some(receiver) = self.webcam.as_mut() {
            out.extend(receiver.take_rtcp());
        }
        let now = clock::monotonic();
        for compound in out {
            self.paced.send_rtcp(&compound, now);
        }
    }

    fn send_reports(&mut self) {
        let now = clock::monotonic();
        let mut next = 1.0;
        if self.dtls_connected && self.video.started() {
            next = self.video.send_report(&mut self.paced, now);
        }
        if self.dtls_connected
            && let Some(audio) = self.audio.as_mut()
            && audio.started()
        {
            audio.send_report(&mut self.paced, now);
        }
        if self.dtls_connected {
            let mut compounds = Vec::new();
            for receiver in [self.microphone.as_mut(), self.webcam.as_mut()]
                .into_iter()
                .flatten()
            {
                compounds.extend(receiver.rtcp_report(now));
            }
            for compound in compounds {
                self.paced.send_rtcp(&compound, now);
            }
        }
        self.report_at = Instant::now() + Duration::from_secs_f64(next);
    }

    /// Write one step of the padding asked, and schedule the next while it is asked.
    fn pad(&mut self) {
        let now = clock::monotonic();
        if self.pad_bps <= 0.0 || now >= self.pad_until {
            self.pad_at = None;
            return;
        }
        if self.dtls_connected {
            let step = (self.pad_bps * PAD_STEP_S / 8.0) as usize;
            self.video.send_padding(step, &mut self.paced, now);
        }
        self.pad_at = Some(Instant::now() + Duration::from_secs_f64(PAD_STEP_S));
    }

    fn on_video(&mut self, frame: &EncodedFrame) {
        self.video
            .push_frame(frame.clone(), &mut self.paced, clock::monotonic());
    }

    fn on_audio(&mut self, sample: AudioSample) {
        if let Some(audio) = self.audio.as_mut() {
            audio.push_frame(
                EncodedFrame {
                    data: sample.data,
                    timestamp: sample.timestamp,
                    keyframe: true,
                    ..EncodedFrame::default()
                },
                &mut self.paced,
                clock::monotonic(),
            );
        }
    }

    /// Release what the pacer's credit covers and hand its events to the video sender.
    fn drain(&mut self) {
        let now = clock::monotonic();
        let next = self.paced.drain(now);
        self.drain_at = next.map(|s| Instant::now() + Duration::from_secs_f64(s));
        if let Some(pacer) = self.paced.pacer() {
            let events: Vec<PacerEvent> = pacer.take_events();
            if !events.is_empty() {
                self.video
                    .apply_pacer_events(events, self.paced.twcc_state());
            }
        }
    }

    async fn finish(mut self) {
        let now = clock::monotonic();
        if self.dtls_connected {
            if self.video.started() {
                self.video.stop(&mut self.paced, now);
            }
            if let Some(audio) = self.audio.as_mut()
                && audio.started()
            {
                audio.stop(&mut self.paced, now);
            }
        }
        if let Some(sctp) = self.sctp.take() {
            sctp.stop();
        }
        self.session.close();
        self.transport.close().await;
        self.set_state(PeerState::Closed);
    }
}

async fn recv_audio(feed: &mut Option<RelayTrack<AudioSample>>) -> AudioSample {
    match feed {
        Some(feed) => feed.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selkies_rtc::rtp::packet::{ABS_SEND_TIME_URI, MID_URI};

    fn map(extensions: &[(u8, &str)]) -> HeaderExtensionsMap {
        let mut map = HeaderExtensionsMap::new();
        let params: Vec<HeaderExtensionParameters> = extensions
            .iter()
            .map(|(id, uri)| HeaderExtensionParameters {
                id: *id,
                uri: uri.to_string(),
            })
            .collect();
        map.configure(&params);
        map
    }

    #[test]
    fn an_uplink_packet_is_read_with_its_own_tracks_extensions() {
        let microphone = map(&[(1, MID_URI)]);
        let webcam = map(&[(1, MID_URI), (3, ABS_SEND_TIME_URI)]);
        let mut router = RtpRouter::new();
        router.register_receiver(Track::Microphone, &[0x1111], &[111]);
        router.register_receiver(Track::Webcam, &[0x2222], &[96]);
        let mut camera = RtpPacket::new(96, 7, 9000, 0x2222);
        camera.extensions.abs_send_time = Some(0x12_3456);
        let wire = Bytes::from(camera.serialize(&webcam));
        assert_eq!(
            RtpPacket::parse(&wire, &microphone)
                .unwrap()
                .extensions
                .abs_send_time,
            None,
            "the microphone's section negotiated no abs-send-time"
        );
        let (track, packet) =
            parse_uplink(&mut router, &wire, Some(&microphone), Some(&webcam)).unwrap();
        assert_eq!(track, Track::Webcam);
        assert_eq!(
            packet.extensions.abs_send_time,
            Some(0x12_3456),
            "a webcam packet keeps the abs-send-time its receive estimate reads"
        );
        let voice = Bytes::from(RtpPacket::new(111, 1, 960, 0x1111).serialize(&microphone));
        let (track, _) =
            parse_uplink(&mut router, &voice, Some(&microphone), Some(&webcam)).unwrap();
        assert_eq!(track, Track::Microphone);
        assert!(
            parse_uplink(
                &mut router,
                &wire.slice(..6),
                Some(&microphone),
                Some(&webcam)
            )
            .is_none(),
            "a truncated packet is routed nowhere"
        );
    }
}
