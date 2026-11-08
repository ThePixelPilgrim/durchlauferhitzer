//! The end of a round of the proxy net: every window opens and every
//! client ends its stream, then nothing may be left open.

use super::*;

/// End a round: windows open, every client ends its stream.
pub(crate) fn wind_down(sim: &mut Sim, stories: &mut [Story], assign: &mut Assign) {
    for story in stories.iter() {
        if story.reset.is_none() && sim.state(story.conn) != SimConnState::Closed {
            sim.set_client_window(story.conn, Window::Unlimited);
        }
        for run in &story.runs {
            if let Some(up) = run.up {
                sim.set_backend_window(up, Window::Unlimited);
            }
        }
    }
    sim.run_until_idle();
    sync(sim, stories, assign);
    for story in stories.iter() {
        if story.reset.is_none() {
            sim.send_eof(story.conn);
        }
    }
    sim.run_until_idle();
    sync(sim, stories, assign);
}

/// Every connection and upstream of a round is closed.
pub(crate) fn assert_closed(sim: &Sim, stories: &[Story]) {
    for story in stories {
        assert_eq!(
            sim.state(story.conn),
            SimConnState::Closed,
            "conn={} still open at the end of its round\ntrace:\n{}",
            story.conn.0,
            sim.trace()
        );
    }
    assert_eq!(
        sim.open_upstreams(),
        0,
        "upstreams left open\ntrace:\n{}",
        sim.trace()
    );
}
