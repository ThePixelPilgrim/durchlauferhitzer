//! The timed scenarios of the proxy net, each alone in its round: the
//! stall is set up, then the clock runs to the deadline it must hit.

use super::*;

/// Advance by `d` but 1 ms, check that nothing happened on the
/// connection, then by the last millisecond.
fn wait_for(sim: &mut Sim, story: &mut Story, d: Duration, assign: &mut Assign) {
    sync(sim, std::slice::from_mut(story), assign);
    let before_logs = sim.logs().len();
    let before = story.received.len();
    sim.advance(d - Duration::from_millis(1));
    sync(sim, std::slice::from_mut(story), assign);
    assert!(
        sim.logs().len() == before_logs && story.received.len() == before,
        "conn={} acted {}ms before its deadline\ntrace:\n{}",
        story.conn.0,
        1,
        sim.trace()
    );
    sim.advance(Duration::from_millis(1));
    sync(sim, std::slice::from_mut(story), assign);
}

/// A timed scenario, alone in its round: set the stall up, then let the
/// clock run to the deadline it must hit.
pub(crate) fn run_timed(
    sim: &mut Sim,
    rng: &mut fastrand::Rng,
    net: &NetConfig,
    story: &mut Story,
    i: usize,
    assign: &mut Assign,
) {
    let conn = story.conn;
    let ex = &story.exchanges[i];
    let route = ex.route.map(|r| &net.routes[r]);
    let Some(route) = route else {
        return;
    };
    let wire = ex.req.wire.clone();
    let send_len = ex.send_len();
    let timing = ex.timing;
    let script_wire = ex
        .script
        .as_ref()
        .map(|s| s.wire.clone())
        .unwrap_or_default();
    let interim_ends = ex
        .script
        .as_ref()
        .map(|s| s.interim_ends.clone())
        .unwrap_or_default();
    let head_len = match &ex.req.verdict {
        Verdict::Forward(f) => f.head_len,
        Verdict::Parse(_) | Verdict::Answer(..) | Verdict::BadBody { .. } => 0,
    };
    // Alone with the clock: every idle connection runs out first, so the
    // scenario's exchange connects afresh, by the connect and window it
    // sets up (FR-PROXY-02).
    clear_pool(sim, net);
    story.send_off = send_len;
    story.runs[i].ended = true;
    // Half the time in one piece, so a whole request can arrive with its
    // head.
    let whole = rng.bool();
    let mut send = |sim: &mut Sim| {
        let mut at = 0;
        while at < send_len {
            let n = if whole {
                send_len
            } else {
                rng.usize(1..=send_len - at)
            };
            sim.send(conn, &wire[at..at + n]);
            at += n;
            if rng.bool() {
                sim.run_until_idle();
            }
        }
        sim.run_until_idle();
    };
    match timing {
        Timing::ConnectHang => {
            sim.set_connect(route.addr, ConnectBehaviour::Hang);
            send(sim);
            sim.set_connect(route.addr, ConnectBehaviour::Accept);
            wait_for(sim, story, net.connect, assign);
        }
        Timing::Silent | Timing::ClientBodyStall(_) => {
            send(sim);
            let d = if timing == Timing::Silent {
                net.response
            } else {
                net.body_idle
            };
            wait_for(sim, story, d, assign);
        }
        Timing::PartialHead(k) | Timing::RelayStall(k) => {
            send(sim);
            sync(sim, std::slice::from_mut(story), assign);
            let Some(up) = story.runs[i].up else {
                panic!("conn={} never connected\ntrace:\n{}", conn.0, sim.trace());
            };
            sim.backend_send(up, &script_wire[..k]);
            sim.run_until_idle();
            let d = if matches!(timing, Timing::PartialHead(_)) {
                net.response
            } else {
                net.body_idle
            };
            wait_for(sim, story, d, assign);
        }
        Timing::BackendBodyStall(j) => {
            sim.set_default_backend_window(Window::Budget(head_len + j));
            send(sim);
            sim.set_default_backend_window(Window::Unlimited);
            wait_for(sim, story, net.body_idle, assign);
        }
        Timing::ClientStall => {
            sim.set_client_window(conn, Window::Stalled);
            send(sim);
            sync(sim, std::slice::from_mut(story), assign);
            let Some(up) = story.runs[i].up else {
                panic!("conn={} never connected\ntrace:\n{}", conn.0, sim.trace());
            };
            sim.backend_send(up, &script_wire);
            sim.backend_eof(up);
            sim.run_until_idle();
            wait_for(sim, story, net.send_idle, assign);
            sim.set_client_window(conn, Window::Unlimited);
        }
        Timing::HeadDuringUpload { k, .. } => {
            send(sim);
            sync(sim, std::slice::from_mut(story), assign);
            let Some(up) = story.runs[i].up else {
                panic!("conn={} never connected\ntrace:\n{}", conn.0, sim.trace());
            };
            sim.backend_send(up, &script_wire[..k]);
            sim.run_until_idle();
            let capped = !interim_ends.contains(&k) && net.response <= net.body_idle;
            let d = if capped { net.response } else { net.body_idle };
            wait_for(sim, story, d, assign);
        }
        Timing::Normal | Timing::Early { .. } | Timing::Eager => {}
    }
    sim.run_until_idle();
}
