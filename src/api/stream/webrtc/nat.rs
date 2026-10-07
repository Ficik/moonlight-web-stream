use std::{collections::HashSet, net::IpAddr};

use moonlight_common::webrtc::sdp::{Attribute, Session};
use rtc::ice::candidate::{CandidateType, unmarshal_candidate};

use crate::config::{WebRtcNat1To1IceCandidateType, WebRtcNat1To1Mapping};

/// webrtc/rtc 0.21 stores the SettingEngine NAT mapping without using it when
/// gathering candidates. Advertise the published, port-preserving addresses in
/// the answer as well as the original addresses. Keep deduplication per media
/// section, since unbundled transports need their own candidates.
pub(super) fn apply_nat_1to1(sdp: &mut Session, mapping: &WebRtcNat1To1Mapping) {
    let ips: Vec<IpAddr> = mapping
        .ips
        .iter()
        .filter_map(|ip| match ip.parse() {
            Ok(ip) => Some(ip),
            Err(error) => {
                tracing::warn!(%ip, %error, "ignoring invalid WebRTC NAT mapping IP");
                None
            }
        })
        .collect();

    for attributes in std::iter::once(&mut sdp.attributes)
        .chain(sdp.medias.iter_mut().map(|media| &mut media.attributes))
    {
        let candidates: Vec<_> = attributes
            .iter()
            .filter(|attribute| attribute.attribute == "candidate")
            .filter_map(|attribute| {
                let value = attribute.value.as_deref()?;
                Some((value, unmarshal_candidate(value).ok()?))
            })
            .collect();
        let mut endpoints: HashSet<_> = candidates
            .iter()
            .filter_map(|(_, candidate)| {
                Some((
                    candidate.address().parse::<IpAddr>().ok()?,
                    candidate.port(),
                    candidate.component(),
                ))
            })
            .collect();
        let mut foundations: HashSet<_> = candidates
            .iter()
            .map(|(_, candidate)| candidate.foundation())
            .collect();
        let mut additions = Vec::new();
        let mut next_foundation = 0;
        for (value, candidate) in &candidates {
            if candidate.candidate_type() != CandidateType::Host {
                continue;
            }
            for ip in &ips {
                if !endpoints.insert((*ip, candidate.port(), candidate.component())) {
                    continue;
                }
                let foundation = loop {
                    let foundation = format!("nat{next_foundation}");
                    next_foundation += 1;
                    if foundations.insert(foundation.clone()) {
                        break foundation;
                    }
                };
                // Keep the transport, component, port and extension fields of
                // the gathered candidate; only change the mapped fields.
                let mut fields: Vec<String> = value.split_whitespace().map(str::to_owned).collect();
                fields[0] = foundation;
                fields[4] = ip.to_string();
                if matches!(
                    mapping.ice_candidate_type,
                    WebRtcNat1To1IceCandidateType::Srflx
                ) {
                    fields[7] = "srflx".into();
                    // RFC 8445: srflx type preference 100, preserving local
                    // preference and the component-specific priority bits.
                    fields[3] = ((100 << 24) | (candidate.priority() & 0x00ff_ffff)).to_string();
                    fields.extend([
                        "raddr".into(),
                        candidate.address().into(),
                        "rport".into(),
                        candidate.port().to_string(),
                    ]);
                }
                additions.push(Attribute {
                    attribute: "candidate".into(),
                    value: Some(fields.join(" ")),
                });
            }
        }
        // Candidates must precede end-of-candidates in each media section.
        let index = attributes
            .iter()
            .position(|a| a.attribute == "end-of-candidates")
            .unwrap_or(attributes.len());
        attributes.splice(index..index, additions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session::parse(concat!(
            "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:0\r\n",
            "a=candidate:base1 1 udp 2130706431 192.168.32.2 40000 typ host generation 0\r\n",
            "a=candidate:base2 1 udp 2130706431 100.124.19.148 40000 typ host\r\n",
            "a=candidate:base3 2 udp 2130706430 192.168.32.2 40001 typ host\r\n",
            "a=candidate:stun 1 udp 1694498815 78.44.85.98 63723 typ srflx raddr 192.168.32.2 rport 40000\r\n",
            "a=end-of-candidates\r\n",
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:1\r\n",
            "a=candidate:base1 1 udp 2130706431 192.168.32.2 40000 typ host\r\n",
        ).as_bytes()).unwrap()
    }

    fn mapping(kind: WebRtcNat1To1IceCandidateType) -> WebRtcNat1To1Mapping {
        WebRtcNat1To1Mapping {
            ips: vec![
                "192.168.33.24".into(),
                "192.168.33.24".into(),
                "192.168.33.25".into(),
            ],
            ice_candidate_type: kind,
        }
    }

    #[test]
    fn host_mapping_preserves_originals_ports_components_and_media_sections() {
        let mut sdp = session();
        let original = sdp.clone();
        apply_nat_1to1(&mut sdp, &mapping(WebRtcNat1To1IceCandidateType::Host));
        for (before, after) in original.medias.iter().zip(&sdp.medias) {
            assert!(
                before
                    .attributes
                    .iter()
                    .all(|a| after.attributes.contains(a))
            );
            let added: Vec<_> = after
                .attributes
                .iter()
                .filter(|a| !before.attributes.contains(a))
                .collect();
            assert_eq!(added.len(), if before.media == "video" { 4 } else { 2 });
            for attr in added {
                let c = unmarshal_candidate(attr.value.as_deref().unwrap()).unwrap();
                assert!(c.foundation().starts_with("nat"));
                assert_eq!(c.candidate_type(), CandidateType::Host);
                assert_eq!(c.port(), if c.component() == 1 { 40000 } else { 40001 });
                assert_eq!(
                    c.priority(),
                    if c.component() == 1 {
                        2130706431
                    } else {
                        2130706430
                    }
                );
            }
        }
        assert_eq!(
            sdp.medias[0].attributes.last().unwrap().attribute,
            "end-of-candidates"
        );
        let once = sdp.clone();
        apply_nat_1to1(&mut sdp, &mapping(WebRtcNat1To1IceCandidateType::Host));
        assert_eq!(sdp, once, "mapping is idempotent");
    }

    #[test]
    fn srflx_mapping_includes_related_address_and_port() {
        let mut sdp = session();
        apply_nat_1to1(&mut sdp, &mapping(WebRtcNat1To1IceCandidateType::Srflx));
        let value = sdp.medias[0]
            .attributes
            .iter()
            .filter_map(|a| a.value.as_deref())
            .find(|value| value.contains("192.168.33.24 40000 typ srflx"))
            .unwrap();
        assert!(value.contains("raddr 192.168.32.2 rport 40000"));
        let c = unmarshal_candidate(value).unwrap();
        assert_eq!(c.priority(), 1694498815);
        let once = sdp.clone();
        apply_nat_1to1(&mut sdp, &mapping(WebRtcNat1To1IceCandidateType::Srflx));
        assert_eq!(sdp, once);
    }

    #[test]
    fn existing_mapped_address_and_invalid_config_add_nothing() {
        let mut sdp = session();
        let before = sdp.clone();
        apply_nat_1to1(
            &mut sdp,
            &WebRtcNat1To1Mapping {
                ips: vec!["192.168.32.2".into(), "not an ip".into()],
                ice_candidate_type: WebRtcNat1To1IceCandidateType::Host,
            },
        );
        assert_eq!(sdp, before);
    }
}
