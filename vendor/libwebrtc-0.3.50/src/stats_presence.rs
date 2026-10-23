//! Narrow presence-aware native sender/receiver RTP observations. Unlike `stats`, absent
//! numeric fields remain absent. Raw reports and their unrelated fields are not
//! retained or exposed. Identifiers are used only for in-memory track alignment.
use serde_json::Value;
use std::{collections::HashMap, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundKind {
    Audio,
    Video,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundCodec {
    Opus,
    Pcmu,
    Pcma,
    G722,
    Vp8,
    Vp9,
    H264,
    Av1,
    Rtx,
    Red,
    Fec,
    ComfortNoise,
    Unknown,
}

impl InboundCodec {
    fn from_mime(value: Option<&str>) -> Self {
        match value.map(str::to_ascii_lowercase).as_deref() {
            Some("audio/opus") => Self::Opus,
            Some("audio/pcmu") => Self::Pcmu,
            Some("audio/pcma") => Self::Pcma,
            Some("audio/g722") => Self::G722,
            Some("video/vp8") => Self::Vp8,
            Some("video/vp9") => Self::Vp9,
            Some("video/h264") => Self::H264,
            Some("video/av1") | Some("video/av1x") => Self::Av1,
            Some("video/rtx") | Some("audio/rtx") => Self::Rtx,
            Some("video/red") | Some("audio/red") => Self::Red,
            Some("video/ulpfec") | Some("video/flexfec-03") => Self::Fec,
            Some("audio/cn") => Self::ComfortNoise,
            _ => Self::Unknown,
        }
    }
    pub fn primary_for(self, kind: InboundKind) -> bool {
        matches!(
            (kind, self),
            (
                InboundKind::Audio,
                Self::Opus | Self::Pcmu | Self::Pcma | Self::G722
            ) | (
                InboundKind::Video,
                Self::Vp8 | Self::Vp9 | Self::H264 | Self::Av1
            )
        )
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Pcmu => "pcmu",
            Self::Pcma => "pcma",
            Self::G722 => "g722",
            Self::Vp8 => "vp8",
            Self::Vp9 => "vp9",
            Self::H264 => "h264",
            Self::Av1 => "av1",
            Self::Rtx => "rtx",
            Self::Red => "red",
            Self::Fec => "fec",
            Self::ComfortNoise => "comfort_noise",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundObservationError {
    ReportTooLarge,
    MalformedReport,
    MissingTrackIdentity,
    MissingPrimary,
    AmbiguousPrimary,
    UnknownCodec,
}
impl InboundObservationError {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReportTooLarge => "bounded_report_limit",
            Self::MalformedReport => "malformed_report",
            Self::MissingTrackIdentity => "missing_track_identity",
            Self::MissingPrimary => "missing_primary_rtp",
            Self::AmbiguousPrimary => "ambiguous_primary_rtp",
            Self::UnknownCodec => "unknown_or_ambiguous_codec",
        }
    }
}

/// No serde defaults: a genuine reported zero is `Some(0)`, never `None`.
#[derive(Clone, Debug, Default)]
pub struct InboundCounters {
    pub packets_received: Option<u64>,
    pub packets_lost: Option<i64>,
    pub bytes_received: Option<u64>,
    pub frames_decoded: Option<u64>,
    pub frames_dropped: Option<u64>,
    pub freeze_count: Option<u64>,
    pub total_freeze_duration: Option<f64>,
    pub jitter_buffer_delay: Option<f64>,
    pub jitter_buffer_emitted_count: Option<u64>,
    pub total_samples_received: Option<u64>,
    pub concealed_samples: Option<u64>,
    pub retransmitted_packets_received: Option<u64>,
}

#[derive(Clone)]
pub struct InboundRtpObservation {
    id: Option<String>,
    track: Option<String>,
    codec_id: Option<String>,
    kind: Option<InboundKind>,
    timestamp: Option<f64>,
    pub codec: InboundCodec,
    pub counters: InboundCounters,
}
impl fmt::Debug for InboundRtpObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundRtpObservation")
            .field("codec", &self.codec)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}
impl InboundRtpObservation {
    /// Native ToJson timestamp, with presence preserved. Its unit and age must
    /// be established for the pinned native backend before timed assessments.
    pub fn native_timestamp(&self) -> Option<f64> {
        self.timestamp
    }
    pub fn same_stream_as(&self, other: &Self) -> bool {
        self.id.is_some()
            && self.id == other.id
            && self.track.is_some()
            && self.track == other.track
            && self.codec_id.is_some()
            && self.codec_id == other.codec_id
            && self.codec == other.codec
            && self.kind == other.kind
    }
}

pub struct InboundRtpReport {
    rows: Vec<InboundRtpObservation>,
}
impl fmt::Debug for InboundRtpReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundRtpReport")
            .field("inbound_records", &self.rows.len())
            .finish()
    }
}

fn text(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)?
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .map(str::to_owned)
}
fn count(value: &Value, name: &str) -> Option<u64> {
    value.get(name)?.as_u64()
}
fn seconds(value: &Value, name: &str) -> Option<f64> {
    value
        .get(name)?
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
}
fn records(raw: &str) -> Result<Vec<Value>, InboundObservationError> {
    if raw.len() > 262_144 {
        return Err(InboundObservationError::ReportTooLarge);
    }
    let records: Vec<Value> = if raw.is_empty() {
        vec![]
    } else {
        serde_json::from_str(raw).map_err(|_| InboundObservationError::MalformedReport)?
    };
    if records.len() > 256 {
        return Err(InboundObservationError::ReportTooLarge);
    }
    Ok(records)
}
fn codecs(records: &[Value]) -> HashMap<String, InboundCodec> {
    let mut codecs = HashMap::new();
    for row in records.iter().filter(|r| r["type"] == "codec") {
        if let Some(id) = text(row, "id") {
            let codec = InboundCodec::from_mime(row["mimeType"].as_str());
            codecs
                .entry(id)
                .and_modify(|v| *v = InboundCodec::Unknown)
                .or_insert(codec);
        }
    }
    codecs
}

impl InboundRtpReport {
    /// Parse only a bounded whitelist at the native JSON boundary. Errors never
    /// contain source JSON, identifiers, SDP, transport addresses or secrets.
    pub fn from_native_json(raw: &str) -> Result<Self, InboundObservationError> {
        let records = records(raw)?;
        let codecs = codecs(&records);
        let rows = records
            .iter()
            .filter(|r| r["type"] == "inbound-rtp")
            .map(|r| {
                let codec_id = text(r, "codecId");
                InboundRtpObservation {
                    id: text(r, "id"),
                    track: text(r, "trackIdentifier"),
                    timestamp: seconds(r, "timestamp").filter(|n| *n <= 9_007_199_254_740_991.0),
                    codec: codec_id
                        .as_ref()
                        .and_then(|id| codecs.get(id))
                        .copied()
                        .unwrap_or(InboundCodec::Unknown),
                    codec_id,
                    kind: match r["kind"].as_str() {
                        Some("audio") => Some(InboundKind::Audio),
                        Some("video") => Some(InboundKind::Video),
                        _ => None,
                    },
                    counters: InboundCounters {
                        packets_received: count(r, "packetsReceived"),
                        packets_lost: r["packetsLost"].as_i64(),
                        bytes_received: count(r, "bytesReceived"),
                        frames_decoded: count(r, "framesDecoded"),
                        frames_dropped: count(r, "framesDropped"),
                        freeze_count: count(r, "freezeCount"),
                        total_freeze_duration: seconds(r, "totalFreezeDuration"),
                        jitter_buffer_delay: seconds(r, "jitterBufferDelay"),
                        jitter_buffer_emitted_count: count(r, "jitterBufferEmittedCount"),
                        total_samples_received: count(r, "totalSamplesReceived"),
                        concealed_samples: count(r, "concealedSamples"),
                        retransmitted_packets_received: count(r, "retransmittedPacketsReceived"),
                    },
                }
            })
            .collect();
        Ok(Self { rows })
    }

    /// Select exactly one primary codec record for the subscribed native track.
    /// Missing identity, unsupported codec and multiple primaries fail unknown.
    pub fn select_track(
        &self,
        native_track_id: &str,
        kind: InboundKind,
    ) -> Result<&InboundRtpObservation, InboundObservationError> {
        if native_track_id.is_empty()
            || native_track_id.len() > 256
            || self.rows.iter().any(|r| {
                r.codec.primary_for(kind)
                    && (r.track.is_none()
                        || r.track.as_deref() == Some(native_track_id) && r.kind != Some(kind))
            })
        {
            return Err(InboundObservationError::MissingTrackIdentity);
        }
        if self
            .rows
            .iter()
            .any(|r| r.codec == InboundCodec::Unknown && r.track.is_none())
        {
            return Err(InboundObservationError::UnknownCodec);
        }
        let mut primary = None;
        for row in self
            .rows
            .iter()
            .filter(|r| r.track.as_deref() == Some(native_track_id) && r.kind == Some(kind))
        {
            if row.codec == InboundCodec::Unknown {
                return Err(InboundObservationError::UnknownCodec);
            }
            if !row.codec.primary_for(kind) {
                continue;
            }
            if row.id.is_none() {
                return Err(InboundObservationError::MissingTrackIdentity);
            }
            if primary.replace(row).is_some() {
                return Err(InboundObservationError::AmbiguousPrimary);
            }
        }
        primary.ok_or(InboundObservationError::MissingPrimary)
    }

    /// Counts matching auxiliary RTP records, not traffic, decoded media or
    /// padding. RTX/RED/FEC records never enter primary media deltas.
    pub fn auxiliary_records(&self, native_track_id: &str) -> Option<[usize; 3]> {
        if self.rows.iter().any(|row| {
            row.codec == InboundCodec::Unknown
                || !row
                    .codec
                    .primary_for(row.kind.unwrap_or(InboundKind::Video))
                    && row.track.is_none()
        }) {
            return None;
        }
        let mut counts = [0; 3];
        for row in self
            .rows
            .iter()
            .filter(|r| r.track.as_deref() == Some(native_track_id))
        {
            match row.codec {
                InboundCodec::Rtx => counts[0] += 1,
                InboundCodec::Red => counts[1] += 1,
                InboundCodec::Fec => counts[2] += 1,
                _ => {}
            }
        }
        Some(counts)
    }
}

#[derive(Clone, Debug, Default)]
pub struct OutboundCounters {
    pub packets_sent: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub frames_encoded: Option<u64>,
    pub frames_sent: Option<u64>,
    pub retransmitted_packets_sent: Option<u64>,
    pub total_encode_time: Option<f64>,
    pub source_samples_duration: Option<f64>,
}
#[derive(Clone)]
pub struct OutboundRtpObservation {
    id: Option<String>,
    track: Option<String>,
    codec_id: Option<String>,
    kind: Option<InboundKind>,
    timestamp: Option<f64>,
    pub codec: InboundCodec,
    pub counters: OutboundCounters,
}
impl fmt::Debug for OutboundRtpObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundRtpObservation")
            .field("codec", &self.codec)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}
impl OutboundRtpObservation {
    /// Native ToJson timestamp; same presence/unit/age contract as inbound.
    pub fn native_timestamp(&self) -> Option<f64> {
        self.timestamp
    }
    pub fn same_stream_as(&self, other: &Self) -> bool {
        self.id.is_some()
            && self.id == other.id
            && self.track.is_some()
            && self.track == other.track
            && self.codec_id.is_some()
            && self.codec_id == other.codec_id
            && self.codec == other.codec
            && self.kind == other.kind
    }
}
pub struct OutboundRtpReport {
    rows: Vec<OutboundRtpObservation>,
}
impl fmt::Debug for OutboundRtpReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundRtpReport")
            .field("outbound_records", &self.rows.len())
            .finish()
    }
}
impl OutboundRtpReport {
    /// Parse a bounded whitelist before default-valued typed stats deserialize.
    /// A primary sender record binds only through mediaSourceId -> the exact
    /// media-source trackIdentifier. No SSRC or arbitrary raw fields escape.
    pub fn from_native_json(raw: &str) -> Result<Self, InboundObservationError> {
        let records = records(raw)?;
        let codecs = codecs(&records);
        let mut sources = HashMap::new();
        for row in records.iter().filter(|r| r["type"] == "media-source") {
            if let Some(id) = text(row, "id") {
                let source = (
                    text(row, "trackIdentifier"),
                    seconds(row, "totalSamplesDuration"),
                );
                sources
                    .entry(id)
                    .and_modify(|value| *value = (None, None))
                    .or_insert(source);
            }
        }
        let rows = records
            .iter()
            .filter(|r| r["type"] == "outbound-rtp")
            .map(|r| {
                let codec_id = text(r, "codecId");
                let source = text(r, "mediaSourceId").and_then(|id| sources.get(&id));
                OutboundRtpObservation {
                    id: text(r, "id"),
                    track: source.and_then(|(track, _)| track.clone()),
                    timestamp: seconds(r, "timestamp").filter(|n| *n <= 9_007_199_254_740_991.0),
                    codec: codec_id
                        .as_ref()
                        .and_then(|id| codecs.get(id))
                        .copied()
                        .unwrap_or(InboundCodec::Unknown),
                    codec_id,
                    kind: match r["kind"].as_str() {
                        Some("audio") => Some(InboundKind::Audio),
                        Some("video") => Some(InboundKind::Video),
                        _ => None,
                    },
                    counters: OutboundCounters {
                        packets_sent: count(r, "packetsSent"),
                        bytes_sent: count(r, "bytesSent"),
                        frames_encoded: count(r, "framesEncoded"),
                        frames_sent: count(r, "framesSent"),
                        retransmitted_packets_sent: count(r, "retransmittedPacketsSent"),
                        total_encode_time: seconds(r, "totalEncodeTime"),
                        source_samples_duration: source.and_then(|(_, duration)| *duration),
                    },
                }
            })
            .collect();
        Ok(Self { rows })
    }
    /// Select one primary sender for the published local native track. Repair
    /// codecs cannot become primary; missing/duplicate source identity is unknown.
    pub fn select_track(
        &self,
        native_track_id: &str,
        kind: InboundKind,
    ) -> Result<&OutboundRtpObservation, InboundObservationError> {
        if native_track_id.is_empty()
            || native_track_id.len() > 256
            || self.rows.iter().any(|r| {
                r.codec.primary_for(kind)
                    && (r.track.is_none()
                        || r.track.as_deref() == Some(native_track_id) && r.kind != Some(kind))
            })
        {
            return Err(InboundObservationError::MissingTrackIdentity);
        }
        if self
            .rows
            .iter()
            .any(|r| r.codec == InboundCodec::Unknown && r.track.is_none())
        {
            return Err(InboundObservationError::UnknownCodec);
        }
        let mut primary = None;
        for row in self
            .rows
            .iter()
            .filter(|r| r.track.as_deref() == Some(native_track_id) && r.kind == Some(kind))
        {
            if row.codec == InboundCodec::Unknown {
                return Err(InboundObservationError::UnknownCodec);
            }
            if !row.codec.primary_for(kind) {
                continue;
            }
            if row.id.is_none() {
                return Err(InboundObservationError::MissingTrackIdentity);
            }
            if primary.replace(row).is_some() {
                return Err(InboundObservationError::AmbiguousPrimary);
            }
        }
        primary.ok_or(InboundObservationError::MissingPrimary)
    }
    /// Exact-bound auxiliary record counts only. Unassigned repair records mean
    /// unknown, not zero; these counts do not measure repair or padding traffic.
    pub fn auxiliary_records(&self, native_track_id: &str) -> Option<[usize; 3]> {
        if self.rows.iter().any(|row| {
            row.codec == InboundCodec::Unknown
                || !row
                    .codec
                    .primary_for(row.kind.unwrap_or(InboundKind::Video))
                    && row.track.is_none()
        }) {
            return None;
        }
        let mut counts = [0; 3];
        for row in self
            .rows
            .iter()
            .filter(|r| r.track.as_deref() == Some(native_track_id))
        {
            match row.codec {
                InboundCodec::Rtx => counts[0] += 1,
                InboundCodec::Red => counts[1] += 1,
                InboundCodec::Fec => counts[2] += 1,
                _ => {}
            }
        }
        Some(counts)
    }
}
