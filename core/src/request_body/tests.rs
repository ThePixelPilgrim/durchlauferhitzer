use super::*;
use crate::http::chunked::ChunkedError;

/// A buffer of `capacity` bytes holding `bytes` at its front.
fn buffer(capacity: usize, bytes: &[u8]) -> Vec<u8> {
    let mut recv = vec![0; capacity];
    recv.get_mut(..bytes.len()).unwrap().copy_from_slice(bytes);
    recv
}

/// Copy `bytes` into the body's spare space and report them.
fn receive(
    body: &mut RequestBody,
    recv: &mut [u8],
    bytes: &[u8],
) -> Result<usize, RequestBodyError> {
    let spare = body.spare(recv);
    assert!(
        spare.len() >= bytes.len(),
        "spare {} < {}",
        spare.len(),
        bytes.len()
    );
    spare.get_mut(..bytes.len()).unwrap().copy_from_slice(bytes);
    body.received(bytes.len(), recv)
}

#[test]
fn body_less_request_moves_the_next_one_to_the_front() {
    let mut recv = buffer(32, b"HEAD....GET /");
    let mut body =
        RequestBody::new(BodyFraming::None, BodyLimit::Unlimited, 8, 13, &mut recv).unwrap();
    assert_eq!(body.stage(), Stage::Written);
    assert_eq!(body.pending(), None);
    assert!(body.spare(&mut recv).is_empty());
    assert_eq!(body.end(&mut recv), BodyEnd::Read { buffered: 5 });
    assert_eq!(recv.get(..5), Some(b"GET /".as_slice()));
    // `Content-Length: 0` is no body either.
    let body = RequestBody::new(
        BodyFraming::Length(0),
        BodyLimit::Unlimited,
        0,
        0,
        &mut recv,
    )
    .unwrap();
    assert_eq!(body.stage(), Stage::Written);
}

/// Body bytes are written from where they landed; the buffer is offered
/// again, whole, only once they are all written; bytes after the body's
/// end are never part of it and move to the front once.
#[test]
fn content_length_body_fills_drains_and_refills() {
    let mut recv = buffer(16, b"HEADabcde");
    let mut body = RequestBody::new(
        BodyFraming::Length(10),
        BodyLimit::Bytes(1),
        4,
        9,
        &mut recv,
    )
    .unwrap();
    assert_eq!(body.stage(), Stage::Writing);
    assert_eq!(body.unsent(&recv), b"abcde");
    assert!(body.spare(&mut recv).is_empty());
    assert_eq!(body.pending(), Some(BodyPending { unsent: 5, tail: 0 }));
    assert_eq!(body.sent(3, &mut recv), 3);
    assert_eq!(body.unsent(&recv), b"de");
    assert!(body.spare(&mut recv).is_empty());
    // More than was unsent is clamped.
    assert_eq!(body.sent(9, &mut recv), 2);
    assert_eq!(body.stage(), Stage::Receiving);
    assert_eq!(body.all_arrived(), Arrived::Partly);
    assert_eq!(body.spare(&mut recv).len(), 16);
    assert_eq!(receive(&mut body, &mut recv, b"fghijGET"), Ok(8));
    assert_eq!(body.unsent(&recv), b"fghij");
    assert_eq!(body.all_arrived(), Arrived::All);
    assert!(body.spare(&mut recv).is_empty());
    body.sent(5, &mut recv);
    assert_eq!(body.stage(), Stage::Written);
    assert_eq!(body.end(&mut recv), BodyEnd::Read { buffered: 3 });
    assert_eq!(recv.get(..3), Some(b"GET".as_slice()));
}

#[test]
fn received_is_clamped_to_the_spare_space() {
    let mut recv = buffer(8, b"");
    let mut body = RequestBody::new(
        BodyFraming::Length(100),
        BodyLimit::Unlimited,
        0,
        0,
        &mut recv,
    )
    .unwrap();
    assert_eq!(body.received(50, &mut recv), Ok(8));
    assert_eq!(body.unsent_len(), 8);
    assert_eq!(body.received(5, &mut recv), Ok(0));
}

/// An incomplete size line or CRLF stays behind as a tail; it moves to the
/// front once the body bytes before it are written, and the next read
/// goes right behind it.
#[test]
fn chunked_tail_moves_to_the_front() {
    let mut recv = buffer(16, b"5\r\nhel");
    let mut body =
        RequestBody::new(BodyFraming::Chunked, BodyLimit::Unlimited, 0, 6, &mut recv).unwrap();
    assert_eq!(body.unsent(&recv), b"5\r\nhel");
    body.sent(6, &mut recv);
    assert_eq!(receive(&mut body, &mut recv, b"lo\r\n0\r"), Ok(6));
    assert_eq!(body.unsent(&recv), b"lo\r\n");
    assert_eq!(body.pending(), Some(BodyPending { unsent: 4, tail: 2 }));
    body.sent(4, &mut recv);
    assert_eq!(body.pending(), Some(BodyPending { unsent: 0, tail: 2 }));
    assert_eq!(recv.get(..2), Some(b"0\r".as_slice()));
    assert_eq!(body.spare(&mut recv).len(), 14);
    assert_eq!(receive(&mut body, &mut recv, b"\n\r\nPOST"), Ok(7));
    assert_eq!(body.unsent(&recv), b"0\r\n\r\n");
    assert_eq!(body.all_arrived(), Arrived::All);
    body.sent(5, &mut recv);
    assert_eq!(body.end(&mut recv), BodyEnd::Read { buffered: 4 });
    assert_eq!(recv.get(..4), Some(b"POST".as_slice()));
}

/// A tail that never completes is still judged with every new byte.
#[test]
fn chunked_tail_alone_is_rejudged() {
    let mut recv = buffer(64, b"");
    let mut body =
        RequestBody::new(BodyFraming::Chunked, BodyLimit::Unlimited, 0, 0, &mut recv).unwrap();
    for byte in b"0000000a" {
        assert_eq!(receive(&mut body, &mut recv, &[*byte]), Ok(1));
        assert_eq!(body.unsent_len(), 0);
        assert_eq!(body.stage(), Stage::Receiving);
    }
    assert_eq!(receive(&mut body, &mut recv, b"\r\n"), Ok(2));
    assert_eq!(body.unsent(&recv), b"0000000a\r\n");
}

#[test]
fn chunked_errors_and_limit() {
    let mut recv = buffer(32, b"HEAD5;x\r\n");
    assert_eq!(
        RequestBody::new(BodyFraming::Chunked, BodyLimit::Unlimited, 4, 10, &mut recv),
        Err(RequestBodyError::Chunked(ChunkedError::Extension))
    );
    // Exactly the limit passes; one byte more does not, also when it is
    // counted across reads.
    let mut recv = buffer(32, b"3\r\nabc\r\n");
    let mut body =
        RequestBody::new(BodyFraming::Chunked, BodyLimit::Bytes(4), 0, 8, &mut recv).unwrap();
    body.sent(8, &mut recv);
    assert_eq!(receive(&mut body, &mut recv, b"1\r\nd"), Ok(4));
    body.sent(4, &mut recv);
    assert_eq!(
        receive(&mut body, &mut recv, b"\r\n1\r\ne"),
        Err(RequestBodyError::TooLarge)
    );
}

#[test]
fn unread_body_cannot_be_reused() {
    let mut recv = buffer(16, b"ab");
    let mut body = RequestBody::new(
        BodyFraming::Length(5),
        BodyLimit::Unlimited,
        0,
        2,
        &mut recv,
    )
    .unwrap();
    assert_eq!(body.end(&mut recv), BodyEnd::Unread);
    // A whole body not yet written can be dropped, and the next request
    // kept.
    let mut recv = buffer(16, b"abcdeGET");
    let mut body = RequestBody::new(
        BodyFraming::Length(5),
        BodyLimit::Unlimited,
        0,
        8,
        &mut recv,
    )
    .unwrap();
    assert_eq!(body.stage(), Stage::Writing);
    assert_eq!(body.end(&mut recv), BodyEnd::Read { buffered: 3 });
    assert_eq!(recv.get(..3), Some(b"GET".as_slice()));
}
