use crate::message::Message;
use tokio::sync::Mutex;

pub struct DenwaRenji {
    pending: Mutex<Option<(u64, Vec<Message>)>>,
}

impl DenwaRenji {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(None),
        }
    }

    pub async fn send(&self, checkpoint_id: u64, messages: Vec<Message>) {
        *self.pending.lock().await = Some((checkpoint_id, messages));
    }

    pub async fn claim(&self) -> Option<(u64, Vec<Message>)> {
        self.pending.lock().await.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_and_claim() {
        let d = DenwaRenji::new();
        assert!(d.claim().await.is_none());

        d.send(
            5,
            vec![Message::User(crate::message::UserMessage::text(
                "hello from past",
            ))],
        )
        .await;
        let claimed = d.claim().await.unwrap();
        assert_eq!(claimed.0, 5);
        assert_eq!(claimed.1.len(), 1);

        // Second claim is empty
        assert!(d.claim().await.is_none());
    }

    #[tokio::test]
    async fn test_send_overwrites_pending() {
        let d = DenwaRenji::new();
        d.send(1, vec![Message::User(crate::message::UserMessage::text("first"))])
            .await;
        d.send(2, vec![Message::User(crate::message::UserMessage::text("second"))])
            .await;

        let claimed = d.claim().await.unwrap();
        assert_eq!(claimed.0, 2);
        if let Message::User(u) = &claimed.1[0] {
            let content = u.flatten_for_recall();
            assert_eq!(content, "second");
        } else {
            panic!("Expected User message");
        }
    }

    #[tokio::test]
    async fn test_send_empty_messages() {
        let d = DenwaRenji::new();
        d.send(3, vec![]).await;
        let claimed = d.claim().await.unwrap();
        assert_eq!(claimed.0, 3);
        assert!(claimed.1.is_empty());
    }

    #[tokio::test]
    async fn test_claim_returns_none_when_empty() {
        let d = DenwaRenji::new();
        assert!(d.claim().await.is_none());
        d.send(1, vec![Message::User(crate::message::UserMessage::text("x"))])
            .await;
        assert!(d.claim().await.is_some());
        assert!(d.claim().await.is_none());
    }
}
