//! One exchange (a request, its backend and timing) and the outcomes the
//! model allows for it: usually one, two where the spec lets the split of
//! the bytes decide (a body error judged with the head or after it).

use super::*;

/// When and how the backend (or the client) acts in an exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Timing {
    /// The backend answers once it has the whole request.
    Normal,
    /// The client holds back the last `withhold` body bytes; the backend
    /// answers once it has the request head (FR-PROXY-27).
    Early { withhold: usize },
    /// The backend answers as soon as it is connected, while the request
    /// may still be on its way.
    Eager,
    // Timed scenarios (a round of their own; the clock moves):
    /// The connect never completes: 504.
    ConnectHang,
    /// The backend takes the request and says nothing: 504.
    Silent,
    /// The backend sends this many bytes of its response, not its whole
    /// head: 504.
    PartialHead(usize),
    /// The client sends only this many body bytes: 408.
    ClientBodyStall(usize),
    /// The backend takes only this many body bytes: 504.
    BackendBodyStall(usize),
    /// The backend sends this many bytes (past its head, short of its
    /// body's end): a cut.
    RelayStall(usize),
    /// The client takes nothing: a send timeout.
    ClientStall,
    /// The client sends only `sent` body bytes while the backend sends
    /// `k` bytes of its response: a 504 when a head was left incomplete
    /// and its own deadline comes first, else a 408 (FR-PROXY-29).
    HeadDuringUpload { sent: usize, k: usize },
}

impl Timing {
    pub(crate) fn timed(self) -> bool {
        !matches!(self, Timing::Normal | Timing::Early { .. } | Timing::Eager)
    }
}

/// What a backend must have received for an exchange: a prefix of
/// `bytes` of `min..=max` bytes.
#[derive(Debug, Clone)]
pub(crate) struct BackendExp {
    pub(crate) bytes: Vec<u8>,
    pub(crate) min: usize,
    pub(crate) max: usize,
}

/// One outcome the model allows.
#[derive(Debug, Clone)]
pub(crate) struct Alt {
    /// What the client receives; with `cut`, any prefix of it.
    pub(crate) bytes: Vec<u8>,
    pub(crate) cut: bool,
    pub(crate) logs: Vec<Log>,
    /// The connection ends with this exchange.
    pub(crate) closes: bool,
    /// `None`: no upstream may be opened for the exchange.
    pub(crate) upstream: Option<BackendExp>,
    pub(crate) tags: Vec<&'static str>,
}

#[derive(Debug)]
pub(crate) struct Exchange {
    pub(crate) req: Request,
    pub(crate) route: Option<usize>,
    pub(crate) connect: ConnectBehaviour,
    pub(crate) script: Option<Script>,
    pub(crate) timing: Timing,
    pub(crate) alts: Vec<Alt>,
}

impl Exchange {
    /// The connection certainly stays open after this exchange.
    pub(crate) fn keeps_alive(&self) -> bool {
        self.alts.len() == 1 && self.alts.iter().all(|a| !a.closes)
    }

    /// The exchange may open an upstream.
    pub(crate) fn may_connect(&self) -> bool {
        matches!(
            self.req.verdict,
            Verdict::Forward(_) | Verdict::BadBody { .. }
        )
    }

    /// How many request bytes the client sends.
    pub(crate) fn send_len(&self) -> usize {
        match self.timing {
            Timing::Early { withhold } => self.req.wire.len() - withhold,
            Timing::ClientBodyStall(j) | Timing::HeadDuringUpload { sent: j, .. } => {
                self.req.head_len + j
            }
            Timing::Normal
            | Timing::Eager
            | Timing::ConnectHang
            | Timing::Silent
            | Timing::PartialHead(_)
            | Timing::BackendBodyStall(_)
            | Timing::RelayStall(_)
            | Timing::ClientStall => self.req.wire.len(),
        }
    }
}

pub(crate) const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// Who the logs are about.
pub(crate) struct Who {
    pub(crate) conn: ConnId,
    pub(crate) peer: IpAddr,
}

fn mode(req: &Request) -> BodyMode {
    if req.head_only {
        BodyMode::HeadOnly
    } else {
        BodyMode::Full
    }
}

fn dedup(mut ps: Vec<Persistence>) -> Vec<Persistence> {
    ps.dedup();
    ps
}

/// The outcomes of `ex`'s request, backend and timing.
pub(crate) fn alts(ex: &Exchange, who: &Who, net: &NetConfig) -> Vec<Alt> {
    let req = &ex.req;
    let rejected = |reason| Log::Rejected {
        conn: who.conn,
        peer: who.peer,
        reason,
    };
    let response = |status| Log::Response {
        conn: who.conn,
        peer: who.peer,
        status,
        request: Some(req.summary.clone()),
    };
    let tags: Vec<&'static str> = req.tag.into_iter().collect();
    match &req.verdict {
        Verdict::Parse(e) => vec![Alt {
            bytes: fixed(model_http_status(e), Persistence::Close, mode(req)),
            cut: false,
            logs: vec![rejected(RejectReason::Http(*e))],
            closes: true,
            upstream: None,
            tags,
        }],
        Verdict::Answer(status, p) => {
            let mut tags = tags;
            if *status == Status::BadRequest {
                tags.push("400 duplicate X-Forwarded-For");
            }
            vec![Alt {
                bytes: fixed(*status, *p, mode(req)),
                cut: false,
                logs: vec![response(*status)],
                closes: *p == Persistence::Close,
                upstream: None,
                tags,
            }]
        }
        Verdict::BadBody { errors, fwd, bound } => {
            let mut out = Vec::new();
            // Judged before the connect only if the offending byte can
            // arrive with the head, in the same buffer (FR-PROXY-19, -23).
            let before_connect = req.head_len + bound < net.client_recv_capacity();
            for e in errors {
                let (before, tag) = match e {
                    RequestBodyError::Chunked(_) => (
                        fixed(Status::BadRequest, Persistence::Close, mode(req)),
                        "400 bad request chunk",
                    ),
                    RequestBodyError::TooLarge => {
                        (Vec::new(), "request body over the limit, silent")
                    }
                };
                let mut t = tags.clone();
                if before_connect {
                    let mut a = t.clone();
                    a.push(tag);
                    a.push("body error before connect");
                    out.push(Alt {
                        bytes: before,
                        cut: false,
                        logs: vec![rejected(RejectReason::Body(*e))],
                        closes: true,
                        upstream: None,
                        tags: a,
                    });
                }
                if matches!(e, RequestBodyError::TooLarge) {
                    t.push("request body over the limit");
                }
                if ex.connect == ConnectBehaviour::Refuse {
                    // Not judged with the head: the connect is refused
                    // before the rest of the body is read.
                    let route = &net.routes[fwd.route];
                    out.push(Alt {
                        bytes: fixed(Status::BadGateway, Persistence::Close, mode(req)),
                        cut: false,
                        logs: vec![
                            Log::BackendFailed {
                                conn: who.conn,
                                peer: who.peer,
                                backend: route.backend,
                                kind: BackendFailure::Refused,
                            },
                            response(Status::BadGateway),
                        ],
                        closes: true,
                        upstream: Some(BackendExp {
                            bytes: fwd.bytes.clone(),
                            min: 0,
                            max: 0,
                        }),
                        tags: {
                            t.push("body error, then refused");
                            t
                        },
                    });
                    continue;
                }
                t.push("body error after connect");
                out.push(Alt {
                    bytes: if fwd.expect100 {
                        CONTINUE.to_vec()
                    } else {
                        Vec::new()
                    },
                    cut: true,
                    logs: vec![rejected(RejectReason::Body(*e))],
                    closes: true,
                    upstream: Some(BackendExp {
                        bytes: fwd.bytes.clone(),
                        min: fwd.head_len,
                        max: fwd.head_len.saturating_add(*bound).min(fwd.bytes.len()),
                    }),
                    tags: t,
                });
            }
            out
        }
        Verdict::Forward(fwd) => forward_alts(ex, fwd, who, net, tags),
    }
}

fn forward_alts(
    ex: &Exchange,
    fwd: &Forward,
    who: &Who,
    net: &NetConfig,
    mut tags: Vec<&'static str>,
) -> Vec<Alt> {
    let req = &ex.req;
    let route = &net.routes[fwd.route];
    let backend = route.backend;
    let has_body = fwd.bytes.len() > fwd.head_len;
    let full = BackendExp {
        bytes: fwd.bytes.clone(),
        min: fwd.bytes.len(),
        max: fwd.bytes.len(),
    };
    let none = BackendExp {
        bytes: fwd.bytes.clone(),
        min: 0,
        max: 0,
    };
    let only = |n: usize| BackendExp {
        bytes: fwd.bytes.clone(),
        min: n,
        max: n,
    };
    // The head and `j` body bytes; a chunked body's incomplete framing
    // tail (at most 35 bytes) stays behind until it is complete.
    let upload = |j: usize| BackendExp {
        bytes: fwd.bytes.clone(),
        min: fwd.head_len + if req.chunked { j.saturating_sub(35) } else { j },
        max: fwd.head_len + j,
    };
    let cont: &[u8] = if fwd.expect100 { CONTINUE } else { &[] };
    if fwd.expect100 && ex.connect == ConnectBehaviour::Accept {
        tags.push("100 Continue");
    }
    let failed = |kind| Log::BackendFailed {
        conn: who.conn,
        peer: who.peer,
        backend,
        kind,
    };
    let response = |status| Log::Response {
        conn: who.conn,
        peer: who.peer,
        status,
        request: Some(req.summary.clone()),
    };
    // The core's own 502/504 after a backend failure, in each persistence
    // the model allows.
    let core_fail = |kind: BackendFailure,
                     persistences: Vec<Persistence>,
                     cont: &[u8],
                     up: BackendExp,
                     tags: &[&'static str]| {
        let status = match kind {
            BackendFailure::ConnectTimeout
            | BackendFailure::ResponseTimeout
            | BackendFailure::BodyTimeout => Status::GatewayTimeout,
            BackendFailure::Refused
            | BackendFailure::Reset
            | BackendFailure::ClosedEarly
            | BackendFailure::BadResponse(_)
            | BackendFailure::BadChunk(_)
            | BackendFailure::BodyTooLarge
            | BackendFailure::TransferEncodingToHttp10 => Status::BadGateway,
        };
        let mut tags = tags.to_vec();
        tags.push(fail_tag(kind));
        dedup(persistences)
            .into_iter()
            .map(|p| {
                let mut bytes = cont.to_vec();
                bytes.extend(fixed(status, p, mode(req)));
                Alt {
                    bytes,
                    cut: false,
                    logs: vec![failed(kind), response(status)],
                    closes: p == Persistence::Close,
                    upstream: Some(up.clone()),
                    tags: tags.clone(),
                }
            })
            .collect::<Vec<Alt>>()
    };
    // Whether a body that may not have been read stays unread: the client
    // closes, unless all of it was read. Before the connect completes only
    // the bytes that came with the head are read, so all of the body can
    // be read only if the whole request fits the receive buffer.
    let with_head = if has_body && req.wire.len() <= net.client_recv_capacity() {
        vec![req.persistence, Persistence::Close]
    } else if has_body {
        vec![Persistence::Close]
    } else {
        vec![req.persistence]
    };
    let maybe_unread = if has_body {
        vec![req.persistence, Persistence::Close]
    } else {
        vec![req.persistence]
    };
    // One coverage tag per branch of a body that may or may not have been
    // read, so a branch that never happens shows up.
    let mark = |mut alts: Vec<Alt>, read: &'static str, unread: &'static str| {
        if has_body && req.persistence == Persistence::KeepAlive {
            for a in &mut alts {
                a.tags.push(if a.closes { unread } else { read });
            }
        }
        alts
    };
    match (ex.connect, ex.timing) {
        (ConnectBehaviour::Refuse, _) => mark(
            core_fail(BackendFailure::Refused, with_head, &[], none, &tags),
            "refused, body read with the head",
            "refused, body unread",
        ),
        (ConnectBehaviour::Hang, _) => mark(
            core_fail(BackendFailure::ConnectTimeout, with_head, &[], none, &tags),
            "connect timeout, body read with the head",
            "connect timeout, body unread",
        ),
        (ConnectBehaviour::Accept, Timing::Silent | Timing::PartialHead(_)) => core_fail(
            BackendFailure::ResponseTimeout,
            vec![req.persistence],
            cont,
            full,
            &tags,
        ),
        (ConnectBehaviour::Accept, Timing::BackendBodyStall(j)) => mark(
            core_fail(
                BackendFailure::BodyTimeout,
                maybe_unread,
                cont,
                only(fwd.head_len + j),
                &tags,
            ),
            "backend body stall, body all read",
            "backend body stall, body unread",
        ),
        (ConnectBehaviour::Accept, Timing::HeadDuringUpload { sent, k })
            if ex
                .script
                .as_ref()
                .is_some_and(|s| !s.interim_ends.contains(&k))
                && net.response <= net.body_idle =>
        {
            let mut tags = tags;
            tags.push("head deadline during the upload");
            core_fail(
                BackendFailure::ResponseTimeout,
                vec![Persistence::Close],
                cont,
                upload(sent),
                &tags,
            )
        }
        (
            ConnectBehaviour::Accept,
            Timing::ClientBodyStall(j) | Timing::HeadDuringUpload { sent: j, .. },
        ) => {
            let mut bytes = cont.to_vec();
            bytes.extend(fixed(Status::RequestTimeout, Persistence::Close, mode(req)));
            vec![Alt {
                bytes,
                cut: false,
                logs: vec![Log::Rejected {
                    conn: who.conn,
                    peer: who.peer,
                    reason: RejectReason::Timeout(TimeoutKind::RequestBody),
                }],
                closes: true,
                upstream: Some(upload(j)),
                tags,
            }]
        }
        (ConnectBehaviour::Accept, timing) => {
            let Some(script) = &ex.script else {
                return Vec::new();
            };
            let early = matches!(timing, Timing::Early { .. });
            let up = match timing {
                Timing::Early { withhold } => BackendExp {
                    bytes: fwd.bytes.clone(),
                    min: fwd.head_len,
                    max: fwd.bytes.len() - withhold,
                },
                Timing::Normal
                | Timing::ConnectHang
                | Timing::Silent
                | Timing::PartialHead(_)
                | Timing::ClientBodyStall(_)
                | Timing::BackendBodyStall(_)
                | Timing::RelayStall(_)
                | Timing::ClientStall
                | Timing::HeadDuringUpload { .. } => full.clone(),
                // The head is written before any response byte is read;
                // a reset may come first.
                Timing::Eager => BackendExp {
                    bytes: fwd.bytes.clone(),
                    min: match script.ending {
                        Ending::Reset(_) => 0,
                        Ending::Done { .. } | Ending::Eof(_) => fwd.head_len,
                    },
                    max: fwd.bytes.len(),
                },
            };
            if timing == Timing::Eager {
                tags.push("eager response");
            }
            if early {
                tags.push("early response");
            }
            tags.extend(script.tag);
            if script.head_end > 0 && script.wire.starts_with(b"HTTP/1.1 10") {
                tags.push("interim heads");
            }
            if script.wire.len() > 70_000 {
                tags.push("large body");
            }
            let base = if early {
                Persistence::Close
            } else {
                req.persistence
            };
            // An eager answer may come before or after the whole body was
            // read from the client.
            let racing = timing == Timing::Eager && has_body;
            let bases = if racing {
                vec![base, Persistence::Close]
            } else {
                vec![base]
            };
            let alts = match &script.reply {
                Reply::Fail(kind) => core_fail(*kind, bases, cont, up, &tags),
                Reply::Relay(r) => {
                    let mut out = relay_alts(
                        req,
                        r,
                        timing,
                        script,
                        cont,
                        up.clone(),
                        who,
                        backend,
                        tags.clone(),
                        &core_fail,
                        net.upstream_recv_capacity(),
                    );
                    if racing {
                        let closed = if r.persistence == Persistence::Close {
                            r.clone()
                        } else {
                            closing(r)
                        };
                        out.extend(relay_alts(
                            req,
                            &closed,
                            Timing::Early { withhold: 0 },
                            script,
                            cont,
                            up,
                            who,
                            backend,
                            tags,
                            &core_fail,
                            net.upstream_recv_capacity(),
                        ));
                    }
                    out
                }
            };
            if !racing {
                return alts;
            }
            // An eager answer races the upload: the body may be read in
            // full (keep-alive) or not (close), even past one buffer.
            let mut alts = mark(alts, "eager, body all read", "eager, body unread");
            if req.wire.len() > net.client_recv_capacity() {
                for a in alts.iter_mut().filter(|a| !a.closes) {
                    a.tags.push("eager, body all read past one buffer");
                }
            }
            alts
        }
    }
}

/// Builds the core's own 502/504 outcomes (see `forward_alts`).
type CoreFail<'a> =
    dyn Fn(BackendFailure, Vec<Persistence>, &[u8], BackendExp, &[&'static str]) -> Vec<Alt> + 'a;

#[allow(clippy::too_many_arguments, reason = "the parts of one exchange")]
fn relay_alts(
    req: &Request,
    r: &Relay,
    timing: Timing,
    script: &Script,
    cont: &[u8],
    up: BackendExp,
    who: &Who,
    backend: BackendId,
    mut tags: Vec<&'static str>,
    core_fail: &CoreFail,
    upstream_capacity: usize,
) -> Vec<Alt> {
    let proxied = Log::ProxiedResponse {
        conn: who.conn,
        peer: who.peer,
        backend,
        status: r.status,
        request: req.summary.clone(),
    };
    let failed = |kind| Log::BackendFailed {
        conn: who.conn,
        peer: who.peer,
        backend,
        kind,
    };
    let mut whole = cont.to_vec();
    whole.extend_from_slice(&r.head);
    let cut_alt = |kind: BackendFailure, n: usize, tags: &[&'static str]| {
        let mut bytes = whole.clone();
        bytes.extend_from_slice(&r.body[..n.min(r.body.len())]);
        let mut tags = tags.to_vec();
        tags.push("cut after head");
        tags.push(cut_tag(kind));
        Alt {
            bytes,
            cut: true,
            logs: vec![proxied.clone(), failed(kind)],
            closes: true,
            upstream: Some(up.clone()),
            tags,
        }
    };
    match timing {
        Timing::ClientStall => {
            tags.push("client send timeout");
            return vec![Alt {
                bytes: Vec::new(),
                cut: false,
                logs: vec![
                    proxied,
                    Log::Rejected {
                        conn: who.conn,
                        peer: who.peer,
                        reason: RejectReason::Timeout(TimeoutKind::Send),
                    },
                ],
                closes: true,
                upstream: Some(up),
                tags,
            }];
        }
        Timing::RelayStall(k) => {
            return vec![cut_alt(
                BackendFailure::ResponseTimeout,
                k - script.head_end,
                &tags,
            )];
        }
        Timing::Normal
        | Timing::Early { .. }
        | Timing::ConnectHang
        | Timing::Silent
        | Timing::PartialHead(_)
        | Timing::ClientBodyStall(_)
        | Timing::BackendBodyStall(_)
        | Timing::Eager
        | Timing::HeadDuringUpload { .. } => {}
    }
    if let Some((errors, bound)) = &r.fails {
        // Judged with the head: a 502 that keeps the request's persistence
        // (closed if the backend answered early).
        let base = match timing {
            Timing::Early { .. } => Persistence::Close,
            Timing::Normal
            | Timing::ConnectHang
            | Timing::Silent
            | Timing::PartialHead(_)
            | Timing::ClientBodyStall(_)
            | Timing::BackendBodyStall(_)
            | Timing::RelayStall(_)
            | Timing::ClientStall
            | Timing::Eager
            | Timing::HeadDuringUpload { .. } => req.persistence,
        };
        // Only bytes within the upstream's receive buffer can arrive with
        // the head and be judged before it is queued (FR-PROXY-19, -24).
        let head_start = script.interim_ends.last().copied().unwrap_or(0);
        let with_head = script.head_end - head_start + bound < upstream_capacity;
        let mut out = Vec::new();
        for e in errors {
            if with_head {
                let mut t = tags.clone();
                t.push("body error with the response head");
                out.extend(core_fail(*e, vec![base], cont, up.clone(), &t));
            }
            let mut t = tags.clone();
            t.push("body error after the response head");
            out.push(cut_alt(*e, *bound, &t));
        }
        return out;
    }
    if let Some((kind, n)) = r.cut {
        return vec![cut_alt(kind, n, &tags)];
    }
    tags.push(match r.kind {
        BodyKind::None => "body None",
        BodyKind::Length => "body Length",
        BodyKind::Chunked => "body Chunked",
        BodyKind::UntilClose => "body UntilClose",
    });
    let mut bytes = whole;
    bytes.extend_from_slice(&r.body);
    vec![Alt {
        bytes,
        cut: false,
        logs: vec![proxied],
        closes: r.persistence == Persistence::Close,
        upstream: Some(up),
        tags,
    }]
}

/// `r` as sent to a client that closes after it.
fn closing(r: &Relay) -> Relay {
    let mut r = r.clone();
    r.persistence = Persistence::Close;
    let end = r.head.len() - 2;
    r.head
        .splice(end..end, b"connection: close\r\n".iter().copied());
    r
}

fn fail_tag(kind: BackendFailure) -> &'static str {
    match kind {
        BackendFailure::Refused => "502 Refused",
        BackendFailure::Reset => "502 Reset",
        BackendFailure::ClosedEarly => "502 ClosedEarly",
        BackendFailure::BadResponse(_) => "502 BadResponse",
        BackendFailure::ConnectTimeout => "504 ConnectTimeout",
        BackendFailure::ResponseTimeout => "504 ResponseTimeout",
        BackendFailure::BodyTimeout => "504 BodyTimeout",
        BackendFailure::BadChunk(_) => "502 BadChunk",
        BackendFailure::BodyTooLarge => "502 BodyTooLarge",
        BackendFailure::TransferEncodingToHttp10 => "502 TransferEncodingToHttp10",
    }
}

fn cut_tag(kind: BackendFailure) -> &'static str {
    match kind {
        BackendFailure::Reset => "cut Reset",
        BackendFailure::ClosedEarly => "cut ClosedEarly",
        BackendFailure::BadChunk(_) => "cut BadChunk",
        BackendFailure::BodyTooLarge => "cut BodyTooLarge",
        BackendFailure::ResponseTimeout => "cut ResponseTimeout",
        BackendFailure::Refused
        | BackendFailure::BadResponse(_)
        | BackendFailure::ConnectTimeout
        | BackendFailure::BodyTimeout
        | BackendFailure::TransferEncodingToHttp10 => "cut (other)",
    }
}

/// What may be drawn for a story's exchanges.
pub(crate) struct Draw {
    /// The connect behaviour of each route's backend in this round.
    pub(crate) connect: [ConnectBehaviour; 2],
    /// A timed scenario (solo rounds only).
    pub(crate) timed: bool,
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
        forward_only: false,
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
            need_body: false,
            large: rng.u8(0..3) == 0,
        };
        script = Some(random_script(rng, &ctx));
    }
    let mut ex = Exchange {
        req,
        route,
        connect,
        script,
        timing,
        alts: Vec::new(),
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
        need_body: kind == 5,
        large: false,
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
    };
    ex.alts = alts(&ex, who, net);
    Some(ex)
}
