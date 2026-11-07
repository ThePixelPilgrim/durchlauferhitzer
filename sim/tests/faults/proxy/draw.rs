//! Drawing one random exchange for a story: a plain exchange (request,
//! backend script and an early or eager answer), or one of the timed
//! scenarios (connect, response, head, body and send timeouts) a solo
//! round runs. The outcomes each allows come from `outcome.rs`.

use super::*;

/// What may be drawn for a story's exchanges.
pub(crate) struct Draw {
    /// The connect behaviour of each route's backend in this round.
    pub(crate) connect: [ConnectBehaviour; 2],
    /// A timed scenario (solo rounds only).
    pub(crate) timed: bool,
    /// A story that keeps forwarding well-formed requests and gets clean
    /// responses, so its connections are pooled and reused (FR-PROXY-02).
    pub(crate) pooly: bool,
}

/// A random exchange on `who`'s connection.
pub(crate) fn random_exchange(
    rng: &mut fastrand::Rng,
    net: &NetConfig,
    who: &Who,
    peer_text: &str,
    draw: &Draw,
) -> Exchange {
    if draw.timed {
        for _ in 0..20 {
            if let Some(ex) = timed_exchange(rng, net, who, peer_text) {
                return ex;
            }
        }
    }
    let allow = Allow {
        body_defects: draw.connect.iter().all(|c| *c == ConnectBehaviour::Accept),
        forward_only: draw.pooly,
        need_body: false,
    };
    let req = random_proxy_request(rng, net, peer_text, &allow);
    let route = match &req.verdict {
        Verdict::Forward(f) | Verdict::BadBody { fwd: f, .. } => Some(f.route),
        Verdict::Parse(_) | Verdict::Answer(..) => None,
    };
    let connect = route.map_or(ConnectBehaviour::Accept, |r| draw.connect[r]);
    let mut timing = Timing::Normal;
    let mut script = None;
    if let (Verdict::Forward(fwd), ConnectBehaviour::Accept) = (&req.verdict, connect) {
        let body = fwd.bytes.len() - fwd.head_len;
        match rng.u8(0..12) {
            0 | 1 if body > 0 => {
                timing = Timing::Early {
                    withhold: rng.usize(1..=body.min(100)),
                };
            }
            2 => timing = Timing::Eager,
            _ => {}
        }
        let ctx = Ctx {
            head_only: req.head_only,
            http10: req.http10,
            persistence: req.persistence,
            early: matches!(timing, Timing::Early { .. }),
            limit: net.routes[fwd.route].max_response_body,
            max_head: net.max_head,
            valid_only: false,
            clean: draw.pooly || rng.bool(),
            exact: (rng.u8(0..12) == 0).then(|| net.upstream_recv_capacity()),
            need_body: false,
            large: rng.u8(0..3) == 0,
            // An eager backend may answer while the request head is still
            // being written: a reset then fails that write at once.
            resets: match timing {
                Timing::Eager => Resets::Late,
                Timing::Normal
                | Timing::Early { .. }
                | Timing::ConnectHang
                | Timing::Silent
                | Timing::PartialHead(_)
                | Timing::ClientBodyStall(_)
                | Timing::BackendBodyStall(_)
                | Timing::RelayStall(_)
                | Timing::ClientStall
                | Timing::HeadDuringUpload { .. } => Resets::WithBytes,
            },
        };
        script = Some(random_script(rng, &ctx));
    }
    let stale = match rng.u8(0..16) {
        0 | 1 => Some(Stale::Reset),
        2 => Some(Stale::Eof),
        3 | 7 | 8 => Some(Stale::Twice),
        4 | 5 => Some(Stale::HalfReset),
        6 => Some(Stale::HalfEof),
        10..13 => Some(Stale::LateReset),
        13..16 => Some(Stale::LateEof),
        _ => None,
    };
    let mut ex = Exchange {
        req,
        route,
        connect,
        script,
        timing,
        alts: Vec::new(),
        stale,
        poke: match rng.u8(0..12) {
            0 => Some(Poke::Byte),
            1 => Some(Poke::Eof),
            2 => Some(Poke::Reset),
            3 | 4 => Some(Poke::Stray),
            _ => None,
        },
    };
    ex.alts = alts(&ex, who, net);
    ex
}

/// A random timed exchange, if the draw allows one.
fn timed_exchange(
    rng: &mut fastrand::Rng,
    net: &NetConfig,
    who: &Who,
    peer_text: &str,
) -> Option<Exchange> {
    let kind = rng.u8(0..8);
    let need_body = matches!(kind, 3 | 4 | 7);
    let allow = Allow {
        body_defects: false,
        forward_only: true,
        need_body,
    };
    let req = random_proxy_request(rng, net, peer_text, &allow);
    let Verdict::Forward(fwd) = &req.verdict else {
        return None;
    };
    let body = fwd.bytes.len() - fwd.head_len;
    if need_body && body == 0 {
        return None;
    }
    let ctx = Ctx {
        head_only: req.head_only,
        http10: req.http10,
        persistence: req.persistence,
        early: false,
        limit: net.routes[fwd.route].max_response_body,
        max_head: net.max_head,
        valid_only: true,
        clean: false,
        exact: None,
        need_body: kind == 5,
        large: false,
        resets: Resets::Late,
    };
    let script = matches!(kind, 2 | 5 | 6 | 7).then(|| random_script(rng, &ctx));
    let (connect, timing) = match kind {
        0 => (ConnectBehaviour::Hang, Timing::ConnectHang),
        1 => (ConnectBehaviour::Accept, Timing::Silent),
        2 => {
            let s = script.as_ref()?;
            (
                ConnectBehaviour::Accept,
                Timing::PartialHead(rng.usize(1..s.head_end)),
            )
        }
        3 => (
            ConnectBehaviour::Accept,
            Timing::ClientBodyStall(rng.usize(0..body)),
        ),
        4 => (
            ConnectBehaviour::Accept,
            Timing::BackendBodyStall(rng.usize(0..body)),
        ),
        5 => {
            let s = script.as_ref()?;
            match &s.reply {
                Reply::Relay(r) if r.fails.is_none() && r.kind != BodyKind::None => {}
                Reply::Relay(_) | Reply::Fail(_) => return None,
            }
            if s.body_end <= s.head_end {
                return None;
            }
            (
                ConnectBehaviour::Accept,
                Timing::RelayStall(rng.usize(s.head_end..s.body_end)),
            )
        }
        7 => {
            let s = script.as_ref()?;
            // A head left incomplete, or interim heads that are complete.
            let k = match rng.choice(&s.interim_ends) {
                Some(&end) if rng.bool() => end,
                Some(_) | None => rng.usize(1..s.head_end),
            };
            let sent = rng.usize(0..body);
            (
                ConnectBehaviour::Accept,
                Timing::HeadDuringUpload { sent, k },
            )
        }
        _ => {
            // A client that takes nothing takes no `100 Continue` either.
            if fwd.expect100 {
                return None;
            }
            let s = script.as_ref()?;
            match &s.reply {
                Reply::Relay(r) if r.fails.is_none() => {}
                Reply::Relay(_) | Reply::Fail(_) => return None,
            }
            (ConnectBehaviour::Accept, Timing::ClientStall)
        }
    };
    let mut ex = Exchange {
        route: Some(fwd.route),
        req,
        connect,
        script,
        timing,
        alts: Vec::new(),
        stale: None,
        poke: None,
    };
    ex.alts = alts(&ex, who, net);
    Some(ex)
}
