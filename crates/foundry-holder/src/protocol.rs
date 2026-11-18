//! The length-prefixed socket protocol: attach, input, resize, status, close. Filled by plan task 4.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_round_trips() {
        for request in [
            Request::Attach,
            Request::Input(Bytes::from_static(b"ls -la\r\0\xff")),
            Request::Resize {
                cols: 132,
                rows: 43,
            },
            Request::Status,
            Request::Close,
        ] {
            let frame = request.encode();
            let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
            assert_eq!(len, frame.len() - 4);
            assert_eq!(frame[4], VERSION);
            assert_eq!(Request::decode(&frame[4..]).unwrap(), request);
        }
    }

    #[test]
    fn every_reply_round_trips() {
        for reply in [
            Reply::Snapshot(Bytes::from_static(b"screen\r\n")),
            Reply::Output(Bytes::from_static(b"\x1b[31mred\x1b[0m")),
            Reply::Status(ChildStatus::Running),
            Reply::Status(ChildStatus::Exited(0)),
            Reply::Status(ChildStatus::Exited(-1)),
            Reply::Status(ChildStatus::Exited(137)),
        ] {
            let frame = reply.encode();
            assert_eq!(Reply::decode(&frame[4..]).unwrap(), reply);
        }
    }

    #[test]
    fn another_version_is_refused_by_number() {
        let mut frame = Request::Status.encode();
        frame[4] = VERSION + 1;
        let err = Request::decode(&frame[4..]).unwrap_err();
        assert!(matches!(err, Error::Version(v) if v == VERSION + 1));
        assert!(err.to_string().contains(&(VERSION + 1).to_string()));
    }

    #[test]
    fn an_unknown_kind_or_a_short_body_is_an_error() {
        assert!(matches!(
            Request::decode(&[VERSION, 99]),
            Err(Error::Kind(99))
        ));
        assert!(matches!(
            Request::decode(&[VERSION, 3, 0, 80]),
            Err(Error::Malformed(_))
        ));
        assert!(matches!(
            Reply::decode(&[VERSION]),
            Err(Error::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn frames_are_read_in_order_and_eof_ends_the_stream() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        write_request(&mut client, &Request::Input(Bytes::from_static(b"a")))
            .await
            .unwrap();
        write_request(&mut client, &Request::Close).await.unwrap();
        drop(client);
        assert_eq!(
            read_request(&mut server).await.unwrap(),
            Some(Request::Input(Bytes::from_static(b"a")))
        );
        assert_eq!(
            read_request(&mut server).await.unwrap(),
            Some(Request::Close)
        );
        assert_eq!(read_request(&mut server).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_before_it_is_read() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let len = u32::try_from(MAX_FRAME + 1).unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut client, &len.to_be_bytes())
            .await
            .unwrap();
        assert!(matches!(
            read_reply(&mut server).await,
            Err(Error::TooLarge(n)) if n == MAX_FRAME + 1
        ));
    }
}
