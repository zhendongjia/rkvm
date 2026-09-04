//! Keep a partially decoded network frame alive while servicing input timers.
use rkvm_net::message::Message;
use rkvm_net::Update;
use std::future;
use std::io::{self, ErrorKind};
use std::time::Instant;
use tokio::io::AsyncRead;
use tokio::time::{self, Interval};

pub(crate) trait Repeater {
    fn deadline(&self) -> Option<Instant>;
    fn repeat(&mut self, now: Instant) -> io::Result<()>;
}

#[derive(Debug)]
pub(crate) enum ReceiveError {
    Network(io::Error),
    Input(io::Error),
}

#[cfg(test)]
impl ReceiveError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Network(err) | Self::Input(err) => err.kind(),
        }
    }
}

pub(crate) async fn receive<R: AsyncRead + Send + Unpin, T: Repeater>(
    stream: &mut R,
    watchdog: &mut Interval,
    repeater: &mut T,
) -> Result<Update, ReceiveError> {
    let message = Update::decode(stream);
    tokio::pin!(message);
    let mut prefer_message = false;

    loop {
        let deadline = repeater.deadline();
        let repeat = async move {
            match deadline {
                Some(deadline) => time::sleep_until(deadline.into()).await,
                None => future::pending().await,
            }
        };
        if prefer_message {
            // A slow input sink can leave its next deadline already overdue.
            // Poll the decoder between repeat batches so a queued release or
            // Ping cannot be starved by another immediately ready timer.
            tokio::select! {
                biased;
                _ = watchdog.tick() => return Err(ReceiveError::Network(io::Error::new(ErrorKind::TimedOut, "Ping timed out"))),
                update = &mut message => return update.map_err(ReceiveError::Network),
                _ = repeat => {},
            }
        } else {
            // A stream of complete messages must not starve a due repeat either.
            // Detection of a dead peer has priority in both cases.
            tokio::select! {
                biased;
                _ = watchdog.tick() => return Err(ReceiveError::Network(io::Error::new(ErrorKind::TimedOut, "Ping timed out"))),
                _ = repeat => {},
                update = &mut message => return update.map_err(ReceiveError::Network),
            }
        }
        repeater
            .repeat(time::Instant::now().into_std())
            .map_err(ReceiveError::Input)?;
        prefer_message = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{self as tokio_io, AsyncWriteExt};

    struct FakeRepeater {
        next: Option<Instant>,
        ticks: usize,
        fail: bool,
    }

    impl FakeRepeater {
        fn new() -> Self {
            Self {
                next: Some((time::Instant::now() + Duration::from_millis(5)).into_std()),
                ticks: 0,
                fail: false,
            }
        }
    }

    impl Repeater for FakeRepeater {
        fn deadline(&self) -> Option<Instant> {
            self.next
        }

        fn repeat(&mut self, now: Instant) -> io::Result<()> {
            self.ticks += 1;
            self.next = Some(now + Duration::from_millis(5));
            if self.fail {
                Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "test input failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn watchdog() -> Interval {
        time::interval_at(
            time::Instant::now() + Duration::from_millis(1500),
            Duration::from_millis(1500),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn fragmented_prefix_and_body_survive_multiple_repeat_ticks() {
        let mut frame = Vec::new();
        Update::DestroyDevice { id: 300 }
            .encode(&mut frame)
            .await
            .unwrap();
        let (mut sender, mut receiver) = tokio_io::duplex(64);
        let send = tokio::spawn(async move {
            // A timer fires with one prefix byte read, then with a complete
            // prefix, then with a partial body. Also verify the following frame.
            for byte in frame {
                sender.write_all(&[byte]).await.unwrap();
                time::sleep(Duration::from_millis(12)).await;
            }
            Update::Ping.encode(&mut sender).await.unwrap();
        });
        let mut repeater = FakeRepeater::new();
        let mut watchdog = watchdog();
        assert!(matches!(
            receive(&mut receiver, &mut watchdog, &mut repeater)
                .await
                .unwrap(),
            Update::DestroyDevice { id: 300 }
        ));
        assert!(matches!(
            receive(&mut receiver, &mut watchdog, &mut repeater)
                .await
                .unwrap(),
            Update::Ping
        ));
        assert!(repeater.ticks >= 4);
        send.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn repeats_do_not_extend_the_dead_peer_timeout() {
        let (mut sender, mut receiver) = tokio_io::duplex(64);
        sender.write_all(&[0]).await.unwrap();
        let mut repeater = FakeRepeater::new();
        let start = time::Instant::now();
        let error = receive(&mut receiver, &mut watchdog(), &mut repeater)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(repeater.ticks > 200);
        assert_eq!(start.elapsed(), Duration::from_millis(1500));
    }

    #[tokio::test(start_paused = true)]
    async fn eof_mid_frame_and_repeat_errors_propagate() {
        let (mut sender, mut receiver) = tokio_io::duplex(64);
        sender.write_all(&[0]).await.unwrap();
        drop(sender);
        let error = receive(&mut receiver, &mut watchdog(), &mut FakeRepeater::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnexpectedEof);

        let (_sender, mut receiver) = tokio_io::duplex(64);
        let mut repeater = FakeRepeater::new();
        repeater.fail = true;
        let error = receive(&mut receiver, &mut watchdog(), &mut repeater)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    }

    #[tokio::test(start_paused = true)]
    async fn absent_repeat_deadline_does_not_spin() {
        let (mut sender, mut receiver) = tokio_io::duplex(64);
        let send = tokio::spawn(async move {
            time::sleep(Duration::from_millis(25)).await;
            Update::Ping.encode(&mut sender).await.unwrap();
        });
        let mut repeater = FakeRepeater::new();
        repeater.next = None;
        assert!(matches!(
            receive(&mut receiver, &mut watchdog(), &mut repeater)
                .await
                .unwrap(),
            Update::Ping
        ));
        assert_eq!(repeater.ticks, 0);
        send.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_data_does_not_starve_repeat() {
        let (mut sender, mut receiver) = tokio_io::duplex(64);
        let mut repeater = FakeRepeater::new();
        let mut watchdog = watchdog();
        for _ in 0..100 {
            Update::Ping.encode(&mut sender).await.unwrap();
            time::advance(Duration::from_millis(1)).await;
            assert!(matches!(
                receive(&mut receiver, &mut watchdog, &mut repeater)
                    .await
                    .unwrap(),
                Update::Ping
            ));
        }
        assert_eq!(repeater.ticks, 20);
    }

    #[tokio::test(start_paused = true)]
    async fn overdue_repeats_cannot_starve_a_queued_release() {
        struct Overdue {
            ticks: usize,
            deadline: Instant,
        }
        impl Repeater for Overdue {
            fn deadline(&self) -> Option<Instant> {
                Some(self.deadline)
            }
            fn repeat(&mut self, _now: Instant) -> io::Result<()> {
                self.ticks += 1;
                // Model a batch taking longer than its period. Fail finitely
                // if the loop keeps starving the already-readable decoder.
                if self.ticks > 3 {
                    return Err(io::Error::other("decoder starved"));
                }
                Ok(())
            }
        }
        let (mut sender, mut receiver) = tokio_io::duplex(64);
        Update::DestroyDevice { id: 7 }
            .encode(&mut sender)
            .await
            .unwrap();
        let mut repeater = Overdue {
            ticks: 0,
            deadline: time::Instant::now().into_std(),
        };
        assert!(matches!(
            receive(&mut receiver, &mut watchdog(), &mut repeater)
                .await
                .unwrap(),
            Update::DestroyDevice { id: 7 }
        ));
        assert_eq!(repeater.ticks, 1);
    }
}
