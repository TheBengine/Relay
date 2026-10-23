// Copyright 2025 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;

use serde::Deserialize;

use crate::data_channel::DataChannelState;

/// Presence in the received candidate JSON, before typed String defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateAddressPresence {
    Missing,
    Empty,
    Literal,
    NonLiteral,
    Malformed,
}

impl CandidateAddressPresence {
    pub fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Empty => "empty",
            Self::Literal => "literal_ip",
            Self::NonLiteral => "non_literal_ip",
            Self::Malformed => "malformed_type",
        }
    }
}

#[derive(Default, Clone)]
enum CandidateAddressField {
    #[default]
    Missing,
    Empty,
    Literal(std::net::IpAddr),
    NonLiteral,
    Malformed,
}

impl CandidateAddressField {
    fn text(value: &str) -> Self {
        if value.is_empty() {
            Self::Empty
        } else if value.len() <= 64 {
            value.parse().map(Self::Literal).unwrap_or(Self::NonLiteral)
        } else {
            Self::NonLiteral
        }
    }

    fn presence(&self) -> CandidateAddressPresence {
        match self {
            Self::Missing => CandidateAddressPresence::Missing,
            Self::Empty => CandidateAddressPresence::Empty,
            Self::Literal(_) => CandidateAddressPresence::Literal,
            Self::NonLiteral => CandidateAddressPresence::NonLiteral,
            Self::Malformed => CandidateAddressPresence::Malformed,
        }
    }
}

impl<'de> Deserialize<'de> for CandidateAddressField {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AddressVisitor;
        impl<'de> serde::de::Visitor<'de> for AddressVisitor {
            type Value = CandidateAddressField;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a candidate address JSON value")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value,E> {
                Ok(CandidateAddressField::text(value))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value,E> {
                self.visit_str(&value)
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value,E> { Ok(CandidateAddressField::Malformed) }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value,E> { Ok(CandidateAddressField::Malformed) }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value,E> { Ok(CandidateAddressField::Malformed) }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value,E> { Ok(CandidateAddressField::Malformed) }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value,E> { Ok(CandidateAddressField::Malformed) }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut values: A) -> Result<Self::Value,A::Error> {
                while values.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(CandidateAddressField::Malformed)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut values: A) -> Result<Self::Value,A::Error> {
                while values.next_entry::<serde::de::IgnoredAny,serde::de::IgnoredAny>()?.is_some() {}
                Ok(CandidateAddressField::Malformed)
            }
        }
        deserializer.deserialize_any(AddressVisitor)
    }
}

/// Which observed field supplied the resolved literal IP. These fixed labels do
/// not disclose the IP, candidate ID, URL, username fragment or other metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateAddressOrigin {
    ModernAddress,
    LegacyIp,
    BothAgree,
}

impl CandidateAddressOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::ModernAddress => "modern_address",
            Self::LegacyIp => "legacy_ip",
            Self::BothAgree => "both_agree",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateAddressError {
    Unavailable,
    NonLiteral,
    Malformed,
    Conflicting,
}

impl CandidateAddressError {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unavailable => "candidate_address_unavailable",
            Self::NonLiteral => "candidate_address_not_literal_ip",
            Self::Malformed => "candidate_address_malformed_type",
            Self::Conflicting => "candidate_address_fields_conflict",
        }
    }
}

#[derive(Clone, Copy)]
pub struct ResolvedCandidateAddress {
    ip: std::net::IpAddr,
    origin: CandidateAddressOrigin,
}

impl std::fmt::Debug for ResolvedCandidateAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedCandidateAddress").field("origin", &self.origin).finish_non_exhaustive()
    }
}

impl ResolvedCandidateAddress {
    /// In-memory only: compare this value with the owned expected peer address;
    /// do not put it in diagnostic output.
    pub fn literal_ip(self) -> std::net::IpAddr { self.ip }
    pub fn origin(self) -> CandidateAddressOrigin { self.origin }
    pub fn used_modern(self) -> bool { self.origin != CandidateAddressOrigin::LegacyIp }
    pub fn used_legacy(self) -> bool { self.origin != CandidateAddressOrigin::ModernAddress }
}

/// Opt-in observation of modern `address` and legacy native `ip`. Both fields'
/// presence survives missing/empty defaults, without retaining arbitrary strings.
/// Debug and error labels are redacted. Deserializing this observation alone can
/// classify malformed fields; it does not weaken the existing typed address API.
#[derive(Default, Clone, Deserialize)]
#[serde(default)]
pub struct CandidateAddressObservation {
    #[serde(rename = "address")]
    modern: CandidateAddressField,
    #[serde(rename = "ip")]
    legacy: CandidateAddressField,
}

impl std::fmt::Debug for CandidateAddressObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandidateAddressObservation")
            .field("modern", &self.modern_presence()).field("legacy", &self.legacy_presence()).finish()
    }
}

impl CandidateAddressObservation {
    pub fn modern_presence(&self) -> CandidateAddressPresence { self.modern.presence() }
    pub fn legacy_presence(&self) -> CandidateAddressPresence { self.legacy.presence() }
    pub fn modern_present(&self) -> bool { self.modern_presence() != CandidateAddressPresence::Missing }
    pub fn legacy_present(&self) -> bool { self.legacy_presence() != CandidateAddressPresence::Missing }

    /// Resolve only literal IPs. A nonempty malformed/nonliteral field never
    /// silently falls back to the other field. Two literals must agree as IPs.
    /// Missing/empty companions are allowed, and remain visible through presence.
    pub fn resolve_literal(&self) -> Result<ResolvedCandidateAddress, CandidateAddressError> {
        use CandidateAddressField::*;
        let (ip, origin) = match (&self.modern, &self.legacy) {
            (Malformed, _) | (_, Malformed) => return Err(CandidateAddressError::Malformed),
            (NonLiteral, _) | (_, NonLiteral) => return Err(CandidateAddressError::NonLiteral),
            (Literal(modern), Literal(legacy)) if modern == legacy => (*modern, CandidateAddressOrigin::BothAgree),
            (Literal(_), Literal(_)) => return Err(CandidateAddressError::Conflicting),
            (Literal(ip), Missing | Empty) => (*ip, CandidateAddressOrigin::ModernAddress),
            (Missing | Empty, Literal(ip)) => (*ip, CandidateAddressOrigin::LegacyIp),
            (Missing | Empty, Missing | Empty) => return Err(CandidateAddressError::Unavailable),
        };
        Ok(ResolvedCandidateAddress { ip, origin })
    }
}

/// Values from https://www.w3.org/TR/webrtc-stats/ (NOTE: Some of the structs are not in the SPEC
/// but inside libwebrtc)
/// serde will handle the magic of correctly deserializing the json into our structs.
/// The enums values are inside encapsulated inside option because we're not sure about their
/// default values (So we default to None instead of an arbitrary value)

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "kebab-case")]
pub enum RtcStats {
    Codec(CodecStats),
    InboundRtp(InboundRtpStats),
    OutboundRtp(OutboundRtpStats),
    RemoteInboundRtp(RemoteInboundRtpStats),
    RemoteOutboundRtp(RemoteOutboundRtpStats),
    MediaSource(MediaSourceStats),
    MediaPlayout(MediaPlayoutStats),
    PeerConnection(PeerConnectionStats),
    DataChannel(DataChannelStats),
    Transport(TransportStats),
    CandidatePair(CandidatePairStats),
    LocalCandidate(LocalCandidateStats),
    RemoteCandidate(RemoteCandidateStats),
    Certificate(CertificateStats),
    Stream(StreamStats),
    Track, // Deprecated
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QualityLimitationReason {
    #[default]
    None,
    Cpu,
    Bandwidth,
    Other,
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IceRole {
    #[default]
    Unknown,
    Controlling,
    Controlled,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DtlsTransportState {
    New,
    Connecting,
    Connected,
    Closed,
    Failed,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IceTransportState {
    New,
    Checking,
    Connected,
    Completed,
    Disconnected,
    Failed,
    Closed,
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DtlsRole {
    Client,
    Server,
    #[default]
    Unknown,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IceCandidatePairState {
    Frozen,
    Waiting,
    InProgress, // in-progress
    Failed,
    Succeeded,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IceCandidateType {
    Host,
    Srflx,
    Prflx,
    Relay,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IceServerTransportProtocol {
    Udp,
    Tcp,
    Tls,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IceTcpCandidateType {
    Active,
    Passive,
    So,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct CodecStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub codec: dictionaries::CodecStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct InboundRtpStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub stream: dictionaries::RtpStreamStats,

    #[serde(flatten)]
    pub received: dictionaries::ReceivedRtpStreamStats,

    #[serde(flatten)]
    pub inbound: dictionaries::InboundRtpStreamStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct OutboundRtpStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub stream: dictionaries::RtpStreamStats,

    #[serde(flatten)]
    pub sent: dictionaries::SentRtpStreamStats,

    #[serde(flatten)]
    pub outbound: dictionaries::OutboundRtpStreamStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct RemoteInboundRtpStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub stream: dictionaries::RtpStreamStats,

    #[serde(flatten)]
    pub received: dictionaries::ReceivedRtpStreamStats,

    #[serde(flatten)]
    pub remote_inbound: dictionaries::RemoteInboundRtpStreamStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct RemoteOutboundRtpStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub stream: dictionaries::RtpStreamStats,

    #[serde(flatten)]
    pub sent: dictionaries::SentRtpStreamStats,

    #[serde(flatten)]
    pub remote_outbound: dictionaries::RemoteOutboundRtpStreamStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct MediaSourceStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub source: dictionaries::MediaSourceStats,

    #[serde(flatten)]
    pub audio: dictionaries::AudioSourceStats,

    #[serde(flatten)]
    pub video: dictionaries::VideoSourceStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct MediaPlayoutStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub audio_playout: dictionaries::AudioPlayoutStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct PeerConnectionStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub pc: dictionaries::PeerConnectionStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct DataChannelStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub dc: dictionaries::DataChannelStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct TransportStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub transport: dictionaries::TransportStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct CandidatePairStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub candidate_pair: dictionaries::CandidatePairStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct LocalCandidateStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub local_candidate: dictionaries::IceCandidateStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct RemoteCandidateStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub remote_candidate: dictionaries::IceCandidateStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct CertificateStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub certificate: dictionaries::CertificateStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct StreamStats {
    #[serde(flatten)]
    pub rtc: dictionaries::RtcStats,

    #[serde(flatten)]
    pub stream: dictionaries::StreamStats,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct TrackStats {}

pub mod dictionaries {
    use super::*;

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct RtcStats {
        pub id: String,
        pub timestamp: i64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct CodecStats {
        pub payload_type: u32,
        pub transport_id: String,
        pub mime_type: String,
        pub clock_rate: u32,
        pub channels: u32,
        pub sdp_fmtp_line: String,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct RtpStreamStats {
        pub ssrc: u32,
        pub kind: String,
        pub transport_id: String,
        pub codec_id: String,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct ReceivedRtpStreamStats {
        pub packets_received: u64,
        pub packets_lost: i64,
        pub jitter: f64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct InboundRtpStreamStats {
        pub track_identifier: String,
        pub mid: String,
        pub remote_id: String,
        pub frames_decoded: u32,
        pub key_frames_decoded: u32,
        pub frames_rendered: u32,
        pub frames_dropped: u32,
        pub frame_width: u32,
        pub frame_height: u32,
        pub frames_per_second: f64,
        pub qp_sum: u64,
        pub total_decode_time: f64,
        pub total_inter_frame_delay: f64,
        pub total_squared_inter_frame_delay: f64,
        pub pause_count: u32,
        pub total_pause_duration: f64,
        pub freeze_count: u32,
        pub total_freeze_duration: f64,
        pub last_packet_received_timestamp: f64,
        pub header_bytes_received: u64,
        pub packets_discarded: u64,
        pub fec_bytes_received: u64,
        pub fec_packets_received: u64,
        pub fec_packets_discarded: u64,
        pub bytes_received: u64,
        pub nack_count: u32,
        pub fir_count: u32,
        pub pli_count: u32,
        pub total_processing_delay: f64,
        pub estimated_playout_timestamp: f64,
        pub jitter_buffer_delay: f64,
        pub jitter_buffer_target_delay: f64,
        pub jitter_buffer_emitted_count: u64,
        pub jitter_buffer_minimum_delay: f64,
        pub total_samples_received: u64,
        pub concealed_samples: u64,
        pub silent_concealed_samples: u64,
        pub concealment_events: u64,
        pub inserted_samples_for_deceleration: u64,
        pub removed_samples_for_acceleration: u64,
        pub audio_level: f64,
        pub total_audio_energy: f64,
        pub total_samples_duration: f64,
        pub frames_received: u64,
        pub decoder_implementation: String,
        pub playout_id: String,
        pub power_efficient_decoder: bool,
        pub frames_assembled_from_multiple_packets: u64,
        pub total_assembly_time: f64,
        pub retransmitted_packets_received: u64,
        pub retransmitted_bytes_received: u64,
        pub rtx_ssrc: u32,
        pub fec_ssrc: u32,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct SentRtpStreamStats {
        pub packets_sent: u64,
        pub bytes_sent: u64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct OutboundRtpStreamStats {
        pub mid: String,
        pub media_source_id: String,
        pub remote_id: String,
        pub rid: String,
        pub header_bytes_sent: u64,
        pub retransmitted_packets_sent: u64,
        pub retransmitted_bytes_sent: u64,
        pub rtx_ssrc: u32,
        pub target_bitrate: f64,
        pub total_encoded_bytes_target: u64,
        pub frame_width: u32,
        pub frame_height: u32,
        pub frames_per_second: f64,
        pub frames_sent: u32,
        pub huge_frames_sent: u32,
        pub frames_encoded: u32,
        pub key_frames_encoded: u32,
        pub qp_sum: u64,
        pub total_encode_time: f64,
        pub total_packet_send_delay: f64,
        pub quality_limitation_reason: QualityLimitationReason,
        pub quality_limitation_durations: HashMap<String, f64>,
        pub quality_limitation_resolution_changes: u32,
        pub nack_count: u32,
        pub fir_count: u32,
        pub pli_count: u32,
        pub encoder_implementation: String,
        pub power_efficient_encoder: bool,
        pub active: bool,
        pub scalibility_mode: String,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct RemoteInboundRtpStreamStats {
        pub local_id: String,
        pub round_trip_time: f64,
        pub total_round_trip_time: f64,
        pub fraction_lost: f64,
        pub round_trip_time_measurements: u64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct RemoteOutboundRtpStreamStats {
        pub local_id: String,
        pub remote_timestamp: f64,
        pub reports_sent: u64,
        pub round_trip_time: f64,
        pub total_round_trip_time: f64,
        pub round_trip_time_measurements: u64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct MediaSourceStats {
        pub track_identifier: String,
        pub kind: String,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct AudioSourceStats {
        pub audio_level: f64,
        pub total_audio_energy: f64,
        pub total_samples_duration: f64,
        pub echo_return_loss: f64,
        pub echo_return_loss_enhancement: f64,
        pub dropped_samples_duration: f64,
        pub dropped_samples_events: u32,
        pub total_capture_delay: f64,
        pub total_samples_captured: u64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct VideoSourceStats {
        pub width: u32,
        pub height: u32,
        pub frames: u32,
        pub frames_per_second: f64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct AudioPlayoutStats {
        pub kind: String,
        pub synthesized_samples_duration: f64,
        pub synthesized_samples_events: u32,
        pub total_samples_duration: f64,
        pub total_playout_delay: f64,
        pub total_samples_count: u64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct PeerConnectionStats {
        pub data_channels_opened: u32,
        pub data_channels_closed: u32,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct DataChannelStats {
        pub label: String,
        pub protocol: String,
        pub data_channel_identifier: i32,
        pub state: Option<DataChannelState>,
        pub messages_sent: u32,
        pub bytes_sent: u64,
        pub messages_received: u32,
        pub bytes_received: u64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct TransportStats {
        pub packets_sent: u64,
        pub packets_received: u64,
        pub bytes_sent: u64,
        pub bytes_received: u64,
        pub ice_role: IceRole,
        pub ice_local_username_fragment: String,
        pub dtls_state: Option<DtlsTransportState>,
        pub ice_state: Option<IceTransportState>,
        pub selected_candidate_pair_id: String,
        pub local_certificate_id: String,
        pub remote_certificate_id: String,
        pub tls_version: String,
        pub dtls_cipher: String,
        pub dtls_role: DtlsRole,
        pub srtp_cipher: String,
        pub selected_candidate_pair_changes: u32,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct CandidatePairStats {
        pub transport_id: String,
        pub local_candidate_id: String,
        pub remote_candidate_id: String,
        pub state: Option<IceCandidatePairState>,
        pub nominated: bool,
        pub packets_sent: u64,
        pub packets_received: u64,
        pub bytes_sent: u64,
        pub bytes_received: u64,
        pub last_packet_sent_timestamp: f64,
        pub last_packet_received_timestamp: f64,
        pub total_round_trip_time: f64,
        pub current_round_trip_time: f64,
        pub available_outgoing_bitrate: f64,
        pub available_incoming_bitrate: f64,
        pub requests_received: u64,
        pub requests_sent: u64,
        pub responses_received: u64,
        pub responses_sent: u64,
        pub consent_requests_sent: u64,
        pub packets_discarded_on_send: u32,
        pub bytes_discarded_on_send: u64,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(from = "IceCandidateStatsWire")]
    pub struct IceCandidateStats {
        pub transport_id: String,
        pub address: String,
        pub port: i32,
        pub protocol: String,
        pub candidate_type: Option<IceCandidateType>,
        pub priority: i32,
        pub url: String,
        pub relay_protocol: Option<IceServerTransportProtocol>,
        pub foundation: String,
        pub related_address: String,
        pub related_port: i32,
        pub username_fragment: String,
        pub tcp_type: Option<IceTcpCandidateType>,
        /// Chromium's non-standard `RTCIceCandidateStats.networkType` (WiFi, cellular, ethernet,
        /// etc.), reported only for local candidates.
        pub network_type: Option<String>,
        /// Captured when candidate JSON is deserialized. This does not replace
        /// `address`, and is not recomputed if callers mutate the public fields.
        pub address_observation: super::CandidateAddressObservation,
    }

    #[derive(Default)]
    struct ModernCandidateAddress {
        address: String,
        observation: super::CandidateAddressField,
    }

    impl<'de> Deserialize<'de> for ModernCandidateAddress {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            // Preserve String's existing errors for null/non-string address.
            // Only the new observation treats malformed values as unknown.
            let address = String::deserialize(deserializer)?;
            let observation = super::CandidateAddressField::text(&address);
            Ok(Self { address, observation })
        }
    }

    #[derive(Default, Deserialize)]
    #[serde(rename_all = "camelCase", default)]
    struct IceCandidateStatsWire {
        transport_id: String,
        address: ModernCandidateAddress,
        port: i32,
        protocol: String,
        candidate_type: Option<IceCandidateType>,
        priority: i32,
        url: String,
        relay_protocol: Option<IceServerTransportProtocol>,
        foundation: String,
        related_address: String,
        related_port: i32,
        username_fragment: String,
        tcp_type: Option<IceTcpCandidateType>,
        network_type: Option<String>,
        ip: super::CandidateAddressField,
    }

    impl From<IceCandidateStatsWire> for IceCandidateStats {
        fn from(wire: IceCandidateStatsWire) -> Self {
            Self {
                transport_id: wire.transport_id, address: wire.address.address,
                port: wire.port, protocol: wire.protocol, candidate_type: wire.candidate_type,
                priority: wire.priority, url: wire.url, relay_protocol: wire.relay_protocol,
                foundation: wire.foundation, related_address: wire.related_address,
                related_port: wire.related_port, username_fragment: wire.username_fragment,
                tcp_type: wire.tcp_type, network_type: wire.network_type,
                address_observation: super::CandidateAddressObservation {
                    modern: wire.address.observation, legacy: wire.ip,
                },
            }
        }
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct CertificateStats {
        pub fingerprint: String,
        pub fingerprint_algorithm: String,
        pub base64_certificate: String,
        pub issuer_certificate_id: String,
    }

    #[derive(Debug, Default, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[serde(default)]
    pub struct StreamStats {
        pub id: String,
        pub stream_identifier: String,
        // pub timestamp: i64,
    }
}

#[cfg(test)]
mod candidate_address_tests {
    use super::*;
    use serde_json::{json, Value};

    fn typed(kind: &str, fields: Value) -> dictionaries::IceCandidateStats {
        let mut candidate = json!({"type":kind,"id":"owned-synthetic-candidate","timestamp":123456789,
            "transportId":"owned-synthetic-transport","port":7882,"protocol":"udp",
            "candidateType":"host","priority":99,"foundation":"owned-synthetic-foundation",
            "relatedAddress":"203.0.113.90","relatedPort":9999,"usernameFragment":"owned-synthetic-fragment",
            "url":"turns:owned-synthetic.invalid","networkType":"ethernet","tcpType":"passive","relayProtocol":"tls"});
        for (key,value) in fields.as_object().unwrap() { candidate[key] = value.clone(); }
        match serde_json::from_value::<RtcStats>(candidate).unwrap() {
            RtcStats::LocalCandidate(value) => value.local_candidate,
            RtcStats::RemoteCandidate(value) => value.remote_candidate,
            _ => panic!("owned candidate fixture type"),
        }
    }

    #[test]
    fn typed_native_candidates_preserve_modern_and_legacy_presence_separately() {
        for kind in ["local-candidate","remote-candidate"] {
            for (fields,origin,modern,legacy) in [
                (json!({"address":"192.0.2.11"}),CandidateAddressOrigin::ModernAddress,true,false),
                (json!({"ip":"192.0.2.11"}),CandidateAddressOrigin::LegacyIp,false,true),
                (json!({"address":"192.0.2.11","ip":"192.0.2.11"}),CandidateAddressOrigin::BothAgree,true,true),
                (json!({"address":"","ip":"192.0.2.11"}),CandidateAddressOrigin::LegacyIp,true,true),
                (json!({"address":"192.0.2.11","ip":""}),CandidateAddressOrigin::ModernAddress,true,true),
            ] {
                let candidate = typed(kind,fields.clone());
                let observation = &candidate.address_observation;
                let resolved = observation.resolve_literal().unwrap();
                assert_eq!(resolved.literal_ip(),"192.0.2.11".parse::<std::net::IpAddr>().unwrap());
                assert_eq!(resolved.origin(),origin);
                assert_eq!(resolved.used_modern(),origin!=CandidateAddressOrigin::LegacyIp);
                assert_eq!(resolved.used_legacy(),origin!=CandidateAddressOrigin::ModernAddress);
                assert_eq!(observation.modern_present(),modern);
                assert_eq!(observation.legacy_present(),legacy);
                assert_eq!(candidate.address,fields.get("address").and_then(Value::as_str).unwrap_or(""));
                assert_eq!(candidate.transport_id,"owned-synthetic-transport");
                assert_eq!(candidate.port,7882);
                assert_eq!(candidate.protocol,"udp");
                assert_eq!(candidate.candidate_type,Some(IceCandidateType::Host));
                assert_eq!(candidate.priority,99);
                assert_eq!(candidate.url,"turns:owned-synthetic.invalid");
                assert_eq!(candidate.foundation,"owned-synthetic-foundation");
                assert_eq!(candidate.related_address,"203.0.113.90");
                assert_eq!(candidate.related_port,9999);
                assert_eq!(candidate.username_fragment,"owned-synthetic-fragment");
                assert_eq!(candidate.relay_protocol,Some(IceServerTransportProtocol::Tls));
                assert_eq!(candidate.network_type.as_deref(),Some("ethernet"));
                assert_eq!(candidate.tcp_type,Some(IceTcpCandidateType::Passive));
            }
        }
    }

    #[test]
    fn missing_and_empty_do_not_default_into_presence_or_a_resolved_address() {
        for kind in ["local-candidate","remote-candidate"] {
            for (fields,modern,legacy) in [
                (json!({}),CandidateAddressPresence::Missing,CandidateAddressPresence::Missing),
                (json!({"address":""}),CandidateAddressPresence::Empty,CandidateAddressPresence::Missing),
                (json!({"ip":""}),CandidateAddressPresence::Missing,CandidateAddressPresence::Empty),
                (json!({"address":"","ip":""}),CandidateAddressPresence::Empty,CandidateAddressPresence::Empty),
            ] {
                let candidate = typed(kind,fields);
                assert!(candidate.address.is_empty());
                assert_eq!(candidate.address_observation.modern_presence(),modern);
                assert_eq!(candidate.address_observation.legacy_presence(),legacy);
                assert_eq!(candidate.address_observation.resolve_literal().unwrap_err(),CandidateAddressError::Unavailable);
            }
        }
        let manually_constructed=dictionaries::IceCandidateStats {address:"192.0.2.11".into(),..Default::default()};
        assert_eq!(manually_constructed.address_observation.resolve_literal().unwrap_err(),CandidateAddressError::Unavailable,
            "a default observation is not evidence about JSON that was never observed");
    }

    #[test]
    fn equal_literals_normalize_ipv6_but_conflicting_fields_are_refused() {
        for kind in ["local-candidate","remote-candidate"] {
            let equal=typed(kind,json!({"address":"2001:db8::1","ip":"2001:0DB8:0:0:0:0:0:1"}));
            assert_eq!(equal.address_observation.resolve_literal().unwrap().origin(),CandidateAddressOrigin::BothAgree);
            for fields in [json!({"address":"192.0.2.11","ip":"192.0.2.12"}),
                           json!({"address":"192.0.2.11","ip":"::ffff:192.0.2.11"}),
                           json!({"address":"2001:db8::1","ip":"2001:db8::2"})] {
                let candidate=typed(kind,fields);
                assert_eq!(candidate.address_observation.resolve_literal().unwrap_err(),CandidateAddressError::Conflicting);
            }
        }
    }

    #[test]
    fn nonliteral_or_malformed_legacy_never_overrides_or_silently_uses_modern() {
        for kind in ["local-candidate","remote-candidate"] {
            for value in [Value::Null,json!(true),json!(123),json!([]),json!({"address":"192.0.2.11"})] {
                let candidate=typed(kind,json!({"address":"192.0.2.11","ip":value}));
                assert_eq!(candidate.address,"192.0.2.11","legacy fields do not alter the typed modern address");
                assert_eq!(candidate.address_observation.legacy_presence(),CandidateAddressPresence::Malformed);
                assert_eq!(candidate.address_observation.resolve_literal().unwrap_err(),CandidateAddressError::Malformed);
            }
            for value in ["owned-name.local","192.0.2.999"," 192.0.2.11","192.0.2.11:7882","[2001:db8::1]","fe80::1%owned-interface"] {
                let candidate=typed(kind,json!({"address":"192.0.2.11","ip":value}));
                assert_eq!(candidate.address_observation.legacy_presence(),CandidateAddressPresence::NonLiteral);
                assert_eq!(candidate.address_observation.resolve_literal().unwrap_err(),CandidateAddressError::NonLiteral);
            }
        }
    }

    #[test]
    fn existing_modern_mdns_and_nonstring_deserialization_contracts_remain_unchanged() {
        for kind in ["local-candidate","remote-candidate"] {
            for value in ["owned-name.local","not-an-ip","[2001:db8::1]"] {
                let candidate=typed(kind,json!({"address":value,"ip":"192.0.2.11"}));
                assert_eq!(candidate.address,value);
                assert_eq!(candidate.address_observation.resolve_literal().unwrap_err(),CandidateAddressError::NonLiteral);
            }
            for value in [Value::Null,json!(false),json!(123),json!([]),json!({})] {
                let raw=json!({"type":kind,"address":value,"ip":"192.0.2.11"});
                assert!(serde_json::from_value::<RtcStats>(raw.clone()).is_err(),"typed String keeps rejecting malformed address");
                let observation:CandidateAddressObservation=serde_json::from_value(raw).unwrap();
                assert_eq!(observation.modern_presence(),CandidateAddressPresence::Malformed);
                assert_eq!(observation.resolve_literal().unwrap_err(),CandidateAddressError::Malformed);
            }
        }
    }

    #[test]
    fn duplicate_address_fields_are_refused_without_selecting_a_value() {
        for kind in ["local-candidate","remote-candidate"] {
            for fields in ["\"address\":\"192.0.2.11\",\"address\":\"192.0.2.12\"",
                           "\"ip\":\"192.0.2.11\",\"ip\":\"192.0.2.12\""] {
                let raw=format!("{{\"type\":\"{kind}\",{fields}}}");
                assert!(serde_json::from_str::<RtcStats>(&raw).is_err());
                assert!(serde_json::from_str::<CandidateAddressObservation>(&raw).is_err());
            }
        }
    }

    #[test]
    fn new_observer_debug_labels_and_resolution_do_not_disclose_candidates() {
        let raw=json!({"address":"192.0.2.11","ip":"192.0.2.11","id":"owned-private-id",
            "url":"turns:owned-private.invalid","usernameFragment":"owned-private-fragment"});
        let observation:CandidateAddressObservation=serde_json::from_value(raw).unwrap();
        let resolved=observation.resolve_literal().unwrap();
        let output=format!("{observation:?} {resolved:?} {} {} {}",observation.modern_presence().label(),
            observation.legacy_presence().label(),resolved.origin().label());
        for forbidden in ["192.0.2.11","owned-private-id","owned-private.invalid","owned-private-fragment"] {
            assert!(!output.contains(forbidden));
        }
        for error in [CandidateAddressError::Unavailable,CandidateAddressError::NonLiteral,
                      CandidateAddressError::Malformed,CandidateAddressError::Conflicting] {
            assert!(error.label().len()<64);
            assert!(!error.label().contains("192.0.2"));
        }
        assert!(output.len()<512);
    }
}
