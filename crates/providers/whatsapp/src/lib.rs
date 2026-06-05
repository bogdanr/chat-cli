pub mod bridge;

pub struct WhatsAppProvider {
    handle: bridge::ClientHandle,
}

impl WhatsAppProvider {
    pub fn new(db_path: &str) -> anyhow::Result<Self> {
        let handle = bridge::new_client(db_path)?;
        Ok(Self { handle })
    }

    pub fn connect(&self) -> bool {
        bridge::connect(self.handle)
    }
}

impl Drop for WhatsAppProvider {
    fn drop(&mut self) {
        bridge::disconnect(self.handle);
    }
}

#[cfg(test)]
mod tests {
    use super::bridge;
    use std::time::Duration;

    #[tokio::test]
    async fn bridge_forwards_go_callback_into_tokio_channel() -> anyhow::Result<()> {
        let handle = bridge::new_client(":memory:")?;
        assert!(bridge::connect(handle));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let tx = Box::new(tx);
        let tx_ptr = Box::into_raw(tx);

        unsafe {
            bridge::set_message_callback(tx_ptr.cast());
        }
        assert!(bridge::fire_synthetic_message("hello from go")?);

        let received = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await?;
        assert_eq!(received.as_deref(), Some("hello from go"));

        unsafe {
            drop(Box::from_raw(tx_ptr));
        }
        bridge::disconnect(handle);

        Ok(())
    }
}
