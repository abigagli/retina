// Copyright (C) 2021 Scott Lamb <slamb@slamb.org>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! RTP and RTCP handling; see [RFC 3550](https://datatracker.ietf.org/doc/html/rfc3550).

use bytes::Bytes;
use log::debug;

use crate::client::PacketItem;
use crate::rtcp::ReceivedCompoundPacket;
use crate::rtp::{RawPacket, ReceivedPacket};
use crate::{ConnectionContext, Error, ErrorInt, PacketContext, StreamContext, StreamContextInner};

use super::{SessionOptions, Timeline};

/// RTP/RTCP demarshaller which ensures packets have the correct SSRC and
/// monotonically increasing SEQ. Unstable; exposed for benchmark.
///
/// When using UDP, skips and logs out-of-order packets. When using TCP,
/// fails on them.
///
/// This reports packet loss (via [ReceivedPacket::loss]) but doesn't prohibit it
/// of more than `i16::MAX` which would be indistinguishable from non-monotonic sequence numbers.
/// Servers sometimes drop packets internally even when sending data via TCP.
///
/// At least [one camera](https://github.com/scottlamb/moonfire-nvr/wiki/Cameras:-Reolink#reolink-rlc-410-hardware-version-ipc_3816m)
/// sometimes sends data from old RTSP sessions over new ones. This seems like a
/// serious bug, and currently `InorderRtpParser` will error in this case,
/// although it'd be possible to discard the incorrect SSRC instead.
///
/// [RFC 3550 section 8.2](https://tools.ietf.org/html/rfc3550#section-8.2) says that SSRC
/// can change mid-session with a RTCP BYE message. This currently isn't handled. I'm
/// not sure it will ever come up with IP cameras.
#[doc(hidden)] // pub only for the benchmarks; not a stable API.
#[derive(Debug)]
pub struct InorderParser {
    ssrc: Option<u32>,
    next_seq: Option<u16>,

    /// True iff `ssrc` was set from the beginning via a `RTP-Info` header.
    initial_ssrc: bool,

    /// Total packets seen in this stream.
    seen_packets: u64,
}

impl InorderParser {
    pub fn new(ssrc: Option<u32>, next_seq: Option<u16>) -> Self {
        Self {
            ssrc,
            next_seq,
            initial_ssrc: ssrc.is_some(),
            seen_packets: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn rtp(
        &mut self,
        session_options: &SessionOptions,
        stream_ctx: &StreamContext,
        tool: Option<&super::Tool>,
        conn_ctx: &ConnectionContext,
        pkt_ctx: &PacketContext,
        timeline: &mut Timeline,
        stream_id: usize,
        data: Bytes,
    ) -> Result<Option<PacketItem>, Error> {
        let (raw, payload_range) = RawPacket::new(data).map_err(|e| {
            wrap!(ErrorInt::PacketError {
                conn_ctx: *conn_ctx,
                stream_ctx: stream_ctx.to_owned(),
                pkt_ctx: *pkt_ctx,
                stream_id,
                description: format!(
                    "corrupt RTP header while expecting seq={:04x?}: {:?}\n{:#?}",
                    &self.next_seq,
                    e.reason,
                    crate::hex::LimitedHex::new(&e.data[..], 64),
                ),
            })
        })?;

        // Skip pt=50 packets, sent by at least Geovision cameras. I'm not sure
        // what purpose these serve, but they have the same sequence number as
        // the packet immediately before. In TCP streams, this can cause an
        // "Out-of-order packet or large loss" error. In UDP streams, if these
        // are delivered out of order, they will cause the more important other
        // packet with the same sequence number to be skipped.
        if raw.payload_type() == 50 {
            debug!("skipping pkt with invalid payload type 50");
            return Ok(None);
        }

        let sequence_number = raw.sequence_number();
        let ssrc = raw.ssrc();
        let loss = sequence_number.wrapping_sub(self.next_seq.unwrap_or(sequence_number));
        if matches!(self.ssrc, Some(s) if s != ssrc) {
            if matches!(stream_ctx.0, StreamContextInner::Udp(_)) {
                super::note_stale_live555_data(tool, session_options);
            }
            bail!(ErrorInt::RtpPacketError {
                conn_ctx: *conn_ctx,
                pkt_ctx: *pkt_ctx,
                stream_ctx: stream_ctx.to_owned(),
                stream_id,
                ssrc,
                sequence_number,
                description: format!(
                    "Wrong ssrc after {} packets; expecting ssrc={:08x?} seq={:04x?} \
                     (initial ssrc: {:?})",
                    self.seen_packets, self.ssrc, self.next_seq, self.initial_ssrc,
                ),
            });
        }
        if loss > 0x80_00 {
            if matches!(stream_ctx.0, StreamContextInner::Tcp { .. }) {
                bail!(ErrorInt::RtpPacketError {
                    conn_ctx: *conn_ctx,
                    pkt_ctx: *pkt_ctx,
                    stream_ctx: stream_ctx.to_owned(),
                    stream_id,
                    ssrc,
                    sequence_number,
                    description: format!(
                        "Out-of-order packet or large loss; expecting ssrc={:08x?} seq={:04x?}",
                        self.ssrc, self.next_seq
                    ),
                });
            } else {
                log::info!(
                    "Skipping out-of-order seq={:04x} when expecting ssrc={:08x?} seq={:04x?}",
                    sequence_number,
                    self.ssrc,
                    self.next_seq,
                );
                return Ok(None);
            }
        }
        let timestamp = match timeline.advance_to(raw.timestamp()) {
            Ok(ts) => ts,
            Err(description) => bail!(ErrorInt::RtpPacketError {
                conn_ctx: *conn_ctx,
                pkt_ctx: *pkt_ctx,
                stream_ctx: stream_ctx.to_owned(),
                stream_id,
                ssrc,
                sequence_number,
                description,
            }),
        };
        self.ssrc = Some(ssrc);
        self.next_seq = Some(sequence_number.wrapping_add(1));
        self.seen_packets += 1;
        Ok(Some(PacketItem::Rtp(ReceivedPacket {
            ctx: *pkt_ctx,
            stream_id,
            timestamp,
            raw,
            payload_range,
            loss,
        })))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn rtcp(
        &mut self,
        session_options: &SessionOptions,
        stream_ctx: &StreamContext,
        tool: Option<&super::Tool>,
        pkt_ctx: &PacketContext,
        timeline: &mut Timeline,
        stream_id: usize,
        data: Bytes,
    ) -> Result<Option<PacketItem>, String> {
        let first_pkt = crate::rtcp::ReceivedCompoundPacket::validate(&data[..])?;
        let mut rtp_timestamp = None;
        if let Ok(Some(sr)) = first_pkt.as_sender_report() {
            rtp_timestamp = Some(timeline.place(sr.rtp_timestamp()).map_err(
                |mut description| {
                    description.push_str(" in RTCP SR");
                    description
                },
            )?);

            let ssrc = sr.ssrc();
            if matches!(self.ssrc, Some(s) if s != ssrc) {
                if matches!(stream_ctx.0, StreamContextInner::Tcp { .. }) {
                    super::note_stale_live555_data(tool, session_options);
                }
                return Err(format!(
                    "Expected ssrc={:08x?}, got RTCP SR ssrc={:08x}",
                    self.ssrc, ssrc
                ));
            }
            self.ssrc = Some(ssrc);
        }
        Ok(Some(PacketItem::Rtcp(ReceivedCompoundPacket {
            ctx: *pkt_ctx,
            stream_id,
            rtp_timestamp,
            raw: data,
        })))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::client::UdpStreamContext;

    use super::*;

    /// Checks dropping and logging Geovision's extra payload type 50 packets.
    /// On a GV-EBD4701 running V1.02_2021_04_08, these seem to appear after
    /// every IDR frame, with the same sequence number as the final packet in
    /// that frame.
    #[test]
    fn geovision_pt50_packet() {
        let mut timeline = Timeline::new(None, 90_000, None).unwrap();
        let mut parser = InorderParser::new(Some(0xd25614e), None);
        let stream_ctx = StreamContext::dummy();

        // Normal packet.
        let (pkt, _payload_range) = crate::rtp::RawPacketBuilder {
            sequence_number: 0x1234,
            timestamp: 141000,
            payload_type: 105,
            ssrc: 0xd25614e,
            mark: true,
        }
        .build(*b"foo")
        .unwrap();
        match parser.rtp(
            &SessionOptions::default(),
            &stream_ctx,
            None,
            &ConnectionContext::dummy(),
            &PacketContext::dummy(),
            &mut timeline,
            0,
            pkt.0,
        ) {
            Ok(Some(PacketItem::Rtp(_))) => {}
            o => panic!("unexpected packet 1 result: {o:#?}"),
        }

        // Mystery pt=50 packet with same sequence number.
        let (pkt, _payload_range) = crate::rtp::RawPacketBuilder {
            sequence_number: 0x1234,
            timestamp: 141000,
            payload_type: 50,
            ssrc: 0xd25614e,
            mark: true,
        }
        .build(*b"bar")
        .unwrap();
        match parser.rtp(
            &SessionOptions::default(),
            &stream_ctx,
            None,
            &ConnectionContext::dummy(),
            &PacketContext::dummy(),
            &mut timeline,
            0,
            pkt.0,
        ) {
            Ok(None) => {}
            o => panic!("unexpected packet 2 result: {o:#?}"),
        }
    }

    #[test]
    fn out_of_order() {
        let mut timeline = Timeline::new(None, 90_000, None).unwrap();
        let mut parser = InorderParser::new(Some(0xd25614e), None);
        let stream_ctx = StreamContext(StreamContextInner::Udp(UdpStreamContext {
            local_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            peer_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            local_rtp_port: 0,
            peer_rtp_port: 0,
        }));
        let session_options = SessionOptions::default();
        let (pkt, _payload_range) = crate::rtp::RawPacketBuilder {
            sequence_number: 2,
            timestamp: 2,
            payload_type: 96,
            ssrc: 0xd25614e,
            mark: true,
        }
        .build(*b"pkt 2")
        .unwrap();
        match parser.rtp(
            &session_options,
            &stream_ctx,
            None,
            &ConnectionContext::dummy(),
            &PacketContext::dummy(),
            &mut timeline,
            0,
            pkt.0,
        ) {
            Ok(Some(PacketItem::Rtp(p))) => {
                assert_eq!(p.timestamp().elapsed(), 0);
            }
            o => panic!("unexpected packet 2 result: {o:#?}"),
        }

        let (pkt, _payload_range) = crate::rtp::RawPacketBuilder {
            sequence_number: 1,
            timestamp: 1,
            payload_type: 96,
            ssrc: 0xd25614e,
            mark: true,
        }
        .build(*b"pkt 1")
        .unwrap();
        match parser.rtp(
            &session_options,
            &stream_ctx,
            None,
            &ConnectionContext::dummy(),
            &PacketContext::dummy(),
            &mut timeline,
            0,
            pkt.0,
        ) {
            Ok(None) => {}
            o => panic!("unexpected packet 1 result: {o:#?}"),
        }

        let (pkt, _payload_range) = crate::rtp::RawPacketBuilder {
            sequence_number: 3,
            timestamp: 3,
            payload_type: 96,
            ssrc: 0xd25614e,
            mark: true,
        }
        .build(*b"pkt 3")
        .unwrap();
        match parser.rtp(
            &session_options,
            &stream_ctx,
            None,
            &ConnectionContext::dummy(),
            &PacketContext::dummy(),
            &mut timeline,
            0,
            pkt.0,
        ) {
            Ok(Some(PacketItem::Rtp(p))) => {
                // The missing timestamp shouldn't have adjusted time.
                assert_eq!(p.timestamp().elapsed(), 1);
            }
            o => panic!("unexpected packet 2 result: {o:#?}"),
        }
    }
}
