// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.
//
// Licensed under the Creative Commons Attribution-NonCommercial-NoDerivatives 4.0 International
// (CC BY-NC-ND 4.0). You may use and share this file for non-commercial purposes only and you may not
// create derivatives. See <https://creativecommons.org/licenses/by-nc-nd/4.0/>.
//
// This software is provided "AS IS", without warranty of any kind.
//
// For commercial licensing, please contact: info@solare.cz

use anyhow::{Context, Result};
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tracing::{error, info};

use crate::config::EmailSettings;

#[derive(Debug)]
pub struct EmailNotifier {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    admin_recipients: Vec<String>,
}

impl EmailNotifier {
    pub fn new(config: &EmailSettings) -> Result<Self> {
        let from: Mailbox = config
            .from_address
            .parse()
            .with_context(|| format!("Invalid from_address: {}", config.from_address))?;

        let creds = Credentials::new(config.smtp_username.clone(), config.smtp_password.clone());

        let transport = if config.use_tls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)
                .with_context(|| format!("Failed to create SMTP relay: {}", config.smtp_host))?
                .port(config.smtp_port)
                .credentials(creds)
                .build()
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.smtp_host)
                .port(config.smtp_port)
                .credentials(creds)
                .build()
        };

        Ok(Self {
            transport,
            from,
            admin_recipients: config.admin_recipients.clone(),
        })
    }

    #[must_use]
    pub fn recipients(&self) -> Vec<String> {
        self.admin_recipients.clone()
    }

    pub async fn send_offline_alert(
        &self,
        instance_id: &str,
        friendly_name: &str,
        last_seen: &str,
    ) -> Result<()> {
        let subject = format!("FluxION Alert: {friendly_name} is offline");
        let body = format!(
            "FluxION instance '{friendly_name}' (ID: {instance_id}) has gone offline.\n\n\
             Last heartbeat received: {last_seen}\n\n\
             Please check the instance status at your FluxION Server dashboard."
        );

        self.send_to_all(&subject, &body).await
    }

    pub async fn send_recovery_alert(&self, instance_id: &str, friendly_name: &str) -> Result<()> {
        let subject = format!("FluxION Recovery: {friendly_name} is back online");
        let body = format!(
            "FluxION instance '{friendly_name}' (ID: {instance_id}) has recovered and is back online."
        );

        self.send_to_all(&subject, &body).await
    }

    async fn send_to_all(&self, subject: &str, body: &str) -> Result<()> {
        for recipient in &self.admin_recipients {
            let to: Mailbox = match recipient.parse() {
                Ok(m) => m,
                Err(e) => {
                    error!(recipient = %recipient, error = %e, "Invalid recipient address, skipping");
                    continue;
                }
            };

            let message = Message::builder()
                .from(self.from.clone())
                .to(to)
                .subject(subject)
                .body(body.to_owned())
                .context("Failed to build email message")?;

            match self.transport.send(message).await {
                Ok(_) => info!(recipient = %recipient, subject = ?subject, "Email sent"),
                Err(e) => error!(recipient = %recipient, error = %e, "Failed to send email"),
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_log::{captured_logs, start_log_capture};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// Accepts one message on localhost and answers the SMTP dialogue with success
    async fn fake_smtp() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake SMTP");
        let port = listener.local_addr().expect("local addr").port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = socket.into_split();
            let mut lines = tokio::io::BufReader::new(reader).lines();
            let _ = writer.write_all(b"220 fake\r\n").await;
            let mut in_data = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let reply: &[u8] = if in_data {
                    if line == "." {
                        in_data = false;
                        b"250 queued\r\n"
                    } else {
                        continue;
                    }
                } else if line.starts_with("EHLO") || line.starts_with("HELO") {
                    b"250-fake\r\n250 AUTH PLAIN\r\n"
                } else if line.starts_with("AUTH") {
                    b"235 authenticated\r\n"
                } else if line.starts_with("DATA") {
                    in_data = true;
                    b"354 end with dot\r\n"
                } else if line.starts_with("QUIT") {
                    let _ = writer.write_all(b"221 bye\r\n").await;
                    break;
                } else {
                    b"250 ok\r\n"
                };
                let _ = writer.write_all(reply).await;
            }
        });
        port
    }

    /// Scan #41: a reporter-supplied name reaches the log escaped, never as a new line
    #[tokio::test]
    async fn sent_email_log_escapes_subject() {
        let port = fake_smtp().await;
        let notifier = EmailNotifier::new(&EmailSettings {
            smtp_host: "127.0.0.1".to_owned(),
            smtp_port: port,
            smtp_username: "u".to_owned(),
            smtp_password: "p".to_owned(),
            from_address: "fluxion@example.com".to_owned(),
            use_tls: false,
            admin_recipients: vec!["ops@example.com".to_owned()],
        })
        .expect("test notifier");

        start_log_capture();
        notifier
            .send_recovery_alert("site-a", "Plant\nINFO forged: subject")
            .await
            .expect("send recovery alert");
        let logs = captured_logs();

        assert!(logs.contains("Email sent"), "{logs}");
        assert!(
            !logs.lines().any(|line| line.starts_with("INFO forged")),
            "{logs}"
        );
    }
}
