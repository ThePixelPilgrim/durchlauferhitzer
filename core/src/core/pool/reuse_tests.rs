//! FR-PROXY-02: the one rule for whether an exchange is reusable on both
//! sides, shared by a relayed response and a backend-directed file
//! (FR-PROXY-32). Each of its four conditions alone closes the upstream.
//! (A request head not wholly written cannot meet a response through
//! events: no response read is offered before it is out. Hence this pure
//! test.)

use super::{Exchange, Written};
use crate::conn::Release;
use crate::http::request::Persistence;
use crate::request_body::Stage;
use crate::upstream::BackendId;

const BACKEND: BackendId = BackendId(3);

const REUSABLE: Exchange = Exchange {
    toward: Persistence::KeepAlive,
    request_head: Written::Whole,
    body: Stage::Written,
    response: Persistence::KeepAlive,
};

#[test]
fn an_exchange_is_reusable_only_on_both_sides() {
    assert_eq!(REUSABLE.release(BACKEND), Release::Park(BACKEND));
    let closing = [
        Exchange {
            toward: Persistence::Close,
            ..REUSABLE
        },
        Exchange {
            request_head: Written::Partly,
            ..REUSABLE
        },
        Exchange {
            body: Stage::Receiving,
            ..REUSABLE
        },
        Exchange {
            body: Stage::Writing,
            ..REUSABLE
        },
        Exchange {
            response: Persistence::Close,
            ..REUSABLE
        },
    ];
    for exchange in closing {
        assert_eq!(exchange.release(BACKEND), Release::Close, "{exchange:?}");
    }
}
