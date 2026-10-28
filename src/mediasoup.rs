//! The client half of mediasoup's send-transport handshake, without
//! mediasoup-client.
//!
//! mediasoup never speaks SDP: `create-transport` hands out raw ICE / DTLS
//! parameters and `produce` wants raw RTP parameters. So, like mediasoup-client
//! does in the browser, we let the WebRTC stack (str0m) write a normal OFFER,
//! read what it chose out of it ([`parse_offer`]), fabricate the server's
//! ANSWER from the transport parameters ([`answer`]) and describe the stream
//! to the server in mediasoup's own terms ([`rtp_parameters`]).
//!
//! DTLS roles: the answer says the server is `passive`, so str0m is the DTLS
//! client, and `connect-transport` tells mediasoup the same ([`dtls_parameters`]).
//!
//! Opus is always signaled as `opus/48000/2` because mediasoup matches the
//! router's channel count exactly; mono vs stereo travels in the packets and
//! in the `stereo` / `sprop-stereo` parameters, which listeners' browsers need
//! to decode in stereo at all.

use serde_json::{json, Value};

const MID_EXTENSION: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";

/// The server side of a WebRtcTransport, from the `create-transport` reply.
#[derive(Debug, PartialEq)]
pub struct TransportParams {
    pub ice_ufrag: String,
    pub ice_pwd: String,
    pub ice_lite: bool,
    pub candidates: Vec<IceCandidate>,
    /// The sha-256 DTLS fingerprint.
    pub fingerprint: String,
}

#[derive(Debug, PartialEq)]
pub struct IceCandidate {
    pub foundation: String,
    pub priority: u64,
    pub ip: String,
    pub port: u16,
}

impl TransportParams {
    pub fn from_json(params: &Value) -> Result<TransportParams, String> {
        let bad = || "the server sent unusable transport parameters".to_string();
        let ice = &params["iceParameters"];
        let text = |v: &Value| v.as_str().map(str::to_string).ok_or_else(bad);
        let candidates = params["iceCandidates"]
            .as_array()
            .ok_or_else(bad)?
            .iter()
            // Only UDP: a TCP candidate needs ICE-TCP framing we don't do.
            .filter(|c| c["protocol"].as_str() == Some("udp"))
            .filter_map(|c| {
                Some(IceCandidate {
                    foundation: c["foundation"].as_str()?.to_string(),
                    priority: c["priority"].as_u64()?,
                    // mediasoup renamed `ip` to `address`; accept both.
                    ip: c["address"].as_str().or(c["ip"].as_str())?.to_string(),
                    port: u16::try_from(c["port"].as_u64()?).ok()?,
                })
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err("the server offered no UDP address for audio".to_string());
        }
        let fingerprint = params["dtlsParameters"]["fingerprints"]
            .as_array()
            .ok_or_else(bad)?
            .iter()
            .find(|f| f["algorithm"].as_str() == Some("sha-256"))
            .and_then(|f| f["value"].as_str())
            .ok_or_else(bad)?
            .to_string();
        Ok(TransportParams {
            ice_ufrag: text(&ice["usernameFragment"])?,
            ice_pwd: text(&ice["password"])?,
            ice_lite: ice["iceLite"].as_bool().unwrap_or(false),
            candidates,
            fingerprint,
        })
    }
}

/// What our WebRTC stack put in its offer for the one audio m-line.
#[derive(Debug, PartialEq)]
pub struct LocalOffer {
    pub mid: String,
    pub payload_type: u8,
    pub ssrc: u32,
    pub cname: String,
    pub mid_extension: Option<u8>,
    /// Our DTLS certificate fingerprint: (hash algorithm, value).
    pub fingerprint: (String, String),
}

pub fn parse_offer(sdp: &str) -> Result<LocalOffer, String> {
    let mut mid = None;
    let mut payload_type = None;
    let mut ssrc_cname = None;
    let mut mid_extension = None;
    let mut fingerprint = None;
    for line in sdp.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("m=audio ") {
            payload_type = m.split_whitespace().nth(2).and_then(|pt| pt.parse().ok());
        } else if let Some(m) = line.strip_prefix("a=mid:") {
            mid = Some(m.to_string());
        } else if let Some(ext) = line.strip_prefix("a=extmap:") {
            let mut parts = ext.split_whitespace();
            let id = parts.next().and_then(|id| id.split('/').next()?.parse().ok());
            if parts.next() == Some(MID_EXTENSION) {
                mid_extension = id;
            }
        } else if let Some(f) = line.strip_prefix("a=fingerprint:") {
            if let Some((alg, value)) = f.split_once(' ') {
                fingerprint = Some((alg.to_string(), value.trim().to_string()));
            }
        } else if let Some(s) = line.strip_prefix("a=ssrc:") {
            if ssrc_cname.is_none() {
                let (ssrc, attr) = s.split_once(' ').unwrap_or((s, ""));
                if let (Ok(ssrc), Some(cname)) = (ssrc.parse(), attr.strip_prefix("cname:")) {
                    ssrc_cname = Some((ssrc, cname.to_string()));
                }
            }
        }
    }
    let missing = |what: &str| format!("the local offer has no {}", what);
    let (ssrc, cname) = ssrc_cname.ok_or_else(|| missing("ssrc"))?;
    Ok(LocalOffer {
        mid: mid.ok_or_else(|| missing("mid"))?,
        payload_type: payload_type.ok_or_else(|| missing("audio payload type"))?,
        ssrc,
        cname,
        mid_extension,
        fingerprint: fingerprint.ok_or_else(|| missing("DTLS fingerprint"))?,
    })
}

/// The server's ANSWER: ICE-lite, its candidates and fingerprint, receiving
/// our one Opus stream.
pub fn answer(server: &TransportParams, offer: &LocalOffer, stereo: bool) -> String {
    let pt = offer.payload_type;
    let mut sdp = String::new();
    let mut line = |l: String| {
        sdp.push_str(&l);
        sdp.push_str("\r\n");
    };
    line("v=0".into());
    line("o=mediasoup 1 1 IN IP4 0.0.0.0".into());
    line("s=-".into());
    line("t=0 0".into());
    if server.ice_lite {
        line("a=ice-lite".into());
    }
    line(format!("a=group:BUNDLE {}", offer.mid));
    line("a=msid-semantic: WMS *".into());
    line(format!("m=audio 7 UDP/TLS/RTP/SAVPF {}", pt));
    line("c=IN IP4 127.0.0.1".into());
    line(format!("a=mid:{}", offer.mid));
    line("a=recvonly".into());
    line(format!("a=ice-ufrag:{}", server.ice_ufrag));
    line(format!("a=ice-pwd:{}", server.ice_pwd));
    line(format!("a=fingerprint:sha-256 {}", server.fingerprint));
    line("a=setup:passive".into());
    for c in &server.candidates {
        line(format!(
            "a=candidate:{} 1 udp {} {} {} typ host",
            c.foundation, c.priority, c.ip, c.port
        ));
    }
    line("a=end-of-candidates".into());
    line("a=rtcp-mux".into());
    line("a=rtcp-rsize".into());
    line(format!("a=rtpmap:{} opus/48000/2", pt));
    line(format!("a=fmtp:{} {}", pt, fmtp(stereo)));
    if let Some(id) = offer.mid_extension {
        line(format!("a=extmap:{} {}", id, MID_EXTENSION));
    }
    sdp
}

fn fmtp(stereo: bool) -> String {
    let s = u8::from(stereo);
    format!(
        "minptime=10;useinbandfec=1;usedtx=0;stereo={};sprop-stereo={}",
        s, s
    )
}

/// `connect-transport`'s DTLS parameters: we are the DTLS client.
pub fn dtls_parameters(offer: &LocalOffer) -> Value {
    let (algorithm, value) = &offer.fingerprint;
    json!({
        "role": "client",
        "fingerprints": [{ "algorithm": algorithm, "value": value }],
    })
}

/// `produce`'s RTP parameters for the stream described by `offer`.
pub fn rtp_parameters(offer: &LocalOffer, stereo: bool) -> Value {
    let s = u8::from(stereo);
    let mut params = json!({
        "mid": offer.mid,
        "codecs": [{
            "mimeType": "audio/opus",
            "payloadType": offer.payload_type,
            "clockRate": 48000,
            "channels": 2,
            "parameters": {
                "minptime": 10,
                "useinbandfec": 1,
                "usedtx": 0,
                "stereo": s,
                "sprop-stereo": s,
            },
            "rtcpFeedback": [],
        }],
        "headerExtensions": [],
        "encodings": [{ "ssrc": offer.ssrc, "dtx": false }],
        "rtcp": { "cname": offer.cname, "reducedSize": true },
    });
    if let Some(id) = offer.mid_extension {
        params["headerExtensions"] = json!([{
            "uri": MID_EXTENSION,
            "id": id,
            "encrypt": false,
            "parameters": {},
        }]);
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\n\
o=str0m-0.24.1 2502650648208299655 2 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 5aX\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=fingerprint:sha-256 9F:5D:B7\r\n\
a=setup:actpass\r\n\
a=mid:5aX\r\n\
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
a=sendonly\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=ssrc:1768355706 cname:cast\r\n\
a=ssrc:1768355706 msid:audioman cast\r\n";

    fn transport() -> Value {
        json!({
            "id": "t1",
            "iceParameters": { "usernameFragment": "uf", "password": "pw", "iceLite": true },
            "iceCandidates": [
                { "foundation": "udpcandidate", "priority": 1076302079u64, "address": "203.0.113.10",
                  "protocol": "udp", "port": 40001, "type": "host" },
                { "foundation": "tcpcandidate", "priority": 1076276479u64, "address": "203.0.113.10",
                  "protocol": "tcp", "port": 40002, "type": "host", "tcpType": "passive" },
                { "foundation": "old", "priority": 1u64, "ip": "192.168.1.50",
                  "protocol": "udp", "port": 40003, "type": "host" }
            ],
            "dtlsParameters": { "role": "auto", "fingerprints": [
                { "algorithm": "sha-1", "value": "AA" },
                { "algorithm": "sha-256", "value": "AB:CD" }
            ]}
        })
    }

    #[test]
    fn reads_transport_parameters() {
        let t = TransportParams::from_json(&transport()).unwrap();
        assert_eq!(t.ice_ufrag, "uf");
        assert!(t.ice_lite);
        assert_eq!(t.fingerprint, "AB:CD");
        assert_eq!(t.candidates.len(), 2, "tcp candidate skipped");
        assert_eq!(t.candidates[1].ip, "192.168.1.50", "legacy `ip` field");

        let mut tcp_only = transport();
        tcp_only["iceCandidates"] = json!([transport()["iceCandidates"][1]]);
        assert!(TransportParams::from_json(&tcp_only).is_err());
    }

    #[test]
    fn reads_the_local_offer() {
        assert_eq!(
            parse_offer(OFFER).unwrap(),
            LocalOffer {
                mid: "5aX".into(),
                payload_type: 111,
                ssrc: 1768355706,
                cname: "cast".into(),
                mid_extension: Some(4),
                fingerprint: ("sha-256".into(), "9F:5D:B7".into()),
            }
        );
        assert!(parse_offer("v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n").is_err());
    }

    #[test]
    fn answers_for_the_server() {
        let t = TransportParams::from_json(&transport()).unwrap();
        let o = parse_offer(OFFER).unwrap();
        let sdp = answer(&t, &o, true);
        for want in [
            "a=ice-lite",
            "a=group:BUNDLE 5aX",
            "m=audio 7 UDP/TLS/RTP/SAVPF 111",
            "a=setup:passive",
            "a=fingerprint:sha-256 AB:CD",
            "a=candidate:udpcandidate 1 udp 1076302079 203.0.113.10 40001 typ host",
            "a=candidate:old 1 udp 1 192.168.1.50 40003 typ host",
            "a=fmtp:111 minptime=10;useinbandfec=1;usedtx=0;stereo=1;sprop-stereo=1",
            "a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid",
        ] {
            assert!(sdp.lines().any(|l| l == want), "missing {:?} in\n{}", want, sdp);
        }
        // str0m must accept what we fabricate.
        str0m::change::SdpAnswer::from_sdp_string(&sdp).unwrap();
    }

    #[test]
    fn describes_the_stream_for_mediasoup() {
        let o = parse_offer(OFFER).unwrap();
        let p = rtp_parameters(&o, false);
        assert_eq!(p["mid"], "5aX");
        assert_eq!(p["codecs"][0]["payloadType"], 111);
        assert_eq!(p["codecs"][0]["channels"], 2);
        assert_eq!(p["codecs"][0]["parameters"]["stereo"], 0);
        assert_eq!(p["encodings"][0]["ssrc"], 1768355706u32);
        assert_eq!(p["headerExtensions"][0]["id"], 4);
        assert_eq!(p["rtcp"]["cname"], "cast");

        let d = dtls_parameters(&o);
        assert_eq!(d["role"], "client");
        assert_eq!(d["fingerprints"][0]["algorithm"], "sha-256");
        assert_eq!(d["fingerprints"][0]["value"], "9F:5D:B7");
    }
}
