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
                Ok(_) => info!(recipient = %recipient, subject = %subject, "Email sent"),
                Err(e) => error!(recipient = %recipient, error = %e, "Failed to send email"),
            }
        }

        Ok(())
    }
}
