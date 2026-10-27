use super::*;
use crate::http::chunked::ChunkedError;

const UP: UpstreamId = UpstreamId(7);
const BACKEND: BackendId = BackendId(0);

/// A buffer of `capacity` bytes holding `bytes` at its front.
fn buffer(capacity: usize, bytes: &[u8]) -> Vec<u8> {
    let mut recv = vec![0; capacity];
    recv.get_mut(..bytes.len()).unwrap().copy_from_slice(bytes);
    recv
}

fn body(
    framing: ResponseBody,
    limit: BodyLimit,
    start: usize,
    recv: &mut [u8],
    filled: usize,
) -> Result<(RelayBody, Closing), BackendFailure> {
    RelayBody::new(framing, UP, BACKEND, limit, start, filled, recv)
}

/// Copy `bytes` into the body's spare space and report them.
fn receive(body: &mut RelayBody, recv: &mut [u8], bytes: &[u8]) -> Result<Closing, BackendFailure> {
    let spare = body.spare(recv);
    assert!(spare.len() >= bytes.len(), "spare {}", spare.len());
    spare.get_mut(..bytes.len()).unwrap().copy_from_slice(bytes);
    body.received(bytes.len(), recv)
}

/// Send everything unsent; returns it.
fn drain(body: &mut RelayBody, recv: &mut [u8]) -> Vec<u8> {
    let out = body.unsent(recv).to_vec();
    assert_eq!(body.sent(out.len(), recv), out.len());
    out
}

#[test]
fn no_body_closes_at_once_and_drops_what_follows() {
    let mut recv = buffer(32, b"HEADjunk");
    let (b, closing) = body(ResponseBody::None, BodyLimit::Unlimited, 4, &mut recv, 8).unwrap();
    assert_eq!(closing, Closing::Upstream(UP));
    assert_eq!(b.unsent(&recv), b"");
    assert_eq!(b.upstream(), None);
    assert_eq!(b.pending(), None);
    assert!(b.spare(&mut recv).is_empty());
}

/// Fill, drain, refill: no read while bytes are unsent, then the whole
/// buffer; bytes after the body's end are never sent.
#[test]
fn content_length_fills_drains_refills_and_stops_at_its_end() {
    let mut recv = buffer(8, b"HEADabc");
    let limit = BodyLimit::Bytes(6);
    let (mut b, closing) = body(ResponseBody::Length(6), limit, 4, &mut recv, 7).unwrap();
    assert_eq!(closing, Closing::No);
    assert_eq!(b.unsent(&recv), b"abc");
    assert!(b.spare(&mut recv).is_empty());
    assert_eq!(b.pending(), Some(BodyPending { unsent: 3, tail: 0 }));
    assert_eq!(b.sent(2, &mut recv), 2);
    assert!(b.spare(&mut recv).is_empty());
    assert_eq!(b.sent(5, &mut recv), 1);
    assert_eq!(b.spare(&mut recv).len(), 8);
    assert_eq!(
        receive(&mut b, &mut recv, b"defXYZ"),
        Ok(Closing::Upstream(UP))
    );
    assert_eq!(drain(&mut b, &mut recv), b"def");
    assert_eq!(b.upstream(), None);
    assert!(b.spare(&mut recv).is_empty());
    assert_eq!(b.unsent(&recv), b"");
}

#[test]
fn content_length_over_the_limit_is_refused_up_front() {
    let mut recv = buffer(8, b"HEAD");
    let limit = BodyLimit::Bytes(5);
    assert_eq!(
        body(ResponseBody::Length(6), limit, 4, &mut recv, 4),
        Err(BackendFailure::BodyTooLarge)
    );
    assert!(body(ResponseBody::Length(5), limit, 4, &mut recv, 4).is_ok());
}

/// A chunked body is sent verbatim; an incomplete framing line stays
/// behind, moves to the front once the rest is sent, and the next read is
/// offered the rest of the buffer behind it.
#[test]
fn chunked_tail_moves_to_the_front_once() {
    let mut recv = buffer(16, b"HEAD3\r\nabc\r\n1");
    let (mut b, _) = body(
        ResponseBody::Chunked,
        BodyLimit::Unlimited,
        4,
        &mut recv,
        13,
    )
    .unwrap();
    assert_eq!(b.unsent(&recv), b"3\r\nabc\r\n");
    assert_eq!(b.pending(), Some(BodyPending { unsent: 8, tail: 1 }));
    assert_eq!(drain(&mut b, &mut recv), b"3\r\nabc\r\n");
    assert_eq!(recv.first(), Some(&b'1'));
    assert_eq!(b.spare(&mut recv).len(), 15);
    assert_eq!(b.pending(), Some(BodyPending { unsent: 0, tail: 1 }));
    // Only part of the size line's CRLF: nothing to send, tail grows.
    assert_eq!(receive(&mut b, &mut recv, b"\r"), Ok(Closing::No));
    assert_eq!(b.unsent(&recv), b"");
    assert_eq!(b.spare(&mut recv).len(), 14);
    assert_eq!(
        receive(&mut b, &mut recv, b"\nz\r\n0\r\n\r\nNEXT"),
        Ok(Closing::Upstream(UP))
    );
    assert_eq!(drain(&mut b, &mut recv), b"1\r\nz\r\n0\r\n\r\n");
    assert_eq!(b.pending(), None);
}

#[test]
fn chunked_errors_and_limits_send_nothing_of_the_bad_read() {
    let mut recv = buffer(32, b"HEAD");
    let limit = BodyLimit::Bytes(4);
    let (mut b, _) = body(ResponseBody::Chunked, limit, 4, &mut recv, 4).unwrap();
    assert_eq!(receive(&mut b, &mut recv, b"2\r\nab\r\n"), Ok(Closing::No));
    drain(&mut b, &mut recv);
    assert_eq!(
        receive(&mut b, &mut recv, b"3\r\nabc\r\n"),
        Err(BackendFailure::BodyTooLarge)
    );
    assert_eq!(b.unsent(&recv), b"");

    let mut recv = buffer(32, b"HEAD");
    let (mut b, _) = body(ResponseBody::Chunked, limit, 4, &mut recv, 4).unwrap();
    assert_eq!(
        receive(&mut b, &mut recv, b"2;x\r\n"),
        Err(BackendFailure::BadChunk(ChunkedError::Extension))
    );
    assert_eq!(b.unsent(&recv), b"");
    // With the head: refused before anything is sent.
    let mut recv = buffer(32, b"HEADzz\r\n");
    assert_eq!(
        body(ResponseBody::Chunked, limit, 4, &mut recv, 8),
        Err(BackendFailure::BadChunk(ChunkedError::BadSize))
    );
}

#[test]
fn close_delimited_body_ends_with_the_stream() {
    let mut recv = buffer(8, b"HEADab");
    let limit = BodyLimit::Bytes(5);
    let (mut b, closing) = body(ResponseBody::UntilClose, limit, 4, &mut recv, 6).unwrap();
    assert_eq!(closing, Closing::No);
    assert_eq!(drain(&mut b, &mut recv), b"ab");
    assert_eq!(receive(&mut b, &mut recv, b"cde"), Ok(Closing::No));
    assert_eq!(drain(&mut b, &mut recv), b"cde");
    assert_eq!(
        receive(&mut b, &mut recv, b"f"),
        Err(BackendFailure::BodyTooLarge)
    );
    assert_eq!(b.end_of_stream(), Some(UP));
    assert_eq!(b.upstream(), None);
    assert_eq!(b.end_of_stream(), None);
    // End of stream does not complete any other body.
    let (mut b, _) = body(ResponseBody::Length(3), limit, 0, &mut recv, 0).unwrap();
    assert_eq!(b.end_of_stream(), None);
    assert_eq!(b.upstream(), Some(UP));
    assert_eq!(b.backend(), Some(BACKEND));
}

#[test]
fn reads_and_sends_beyond_the_offer_are_clamped() {
    let mut recv = buffer(8, b"");
    let (mut b, _) = body(
        ResponseBody::UntilClose,
        BodyLimit::Unlimited,
        0,
        &mut recv,
        0,
    )
    .unwrap();
    assert_eq!(b.received(100, &mut recv), Ok(Closing::No));
    assert_eq!(b.unsent_len(), 8);
    assert_eq!(b.received(5, &mut recv), Ok(Closing::No));
    assert_eq!(b.unsent_len(), 8);
    assert_eq!(b.sent(100, &mut recv), 8);
}
