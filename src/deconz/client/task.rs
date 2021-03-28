use std::{collections::HashMap, fmt::Display, io};
use std::{fmt::Debug, process::Command};

use bytes::{Buf, Bytes};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_serial::{Serial, SerialPortSettings};
use tracing::info;

use crate::deconz::{
    frame::OutgoingPacket,
    protocol::{device::DeviceState, CommandId, DeconzCommandRequest},
    DeconzFrame, DeconzStream,
};

use super::DeconzClientConfig;

pub enum TaskMessage {
    CommandRequest {
        command_outgoing: Box<dyn DeconzCommandRequest>,
        response_parser: Box<dyn FnOnce(DeconzFrame<Bytes>) -> Option<DeviceState> + Send>,
    },
}

impl Display for TaskMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskMessage::CommandRequest { .. } => f.write_str("CommandRequest"),
        }
    }
}

impl Debug for TaskMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskMessage::CommandRequest {
                command_outgoing,
                response_parser: _,
            } => f
                .debug_struct("TaskMessage::CommandRequest")
                .field("command", command_outgoing)
                .field("response_parser", &"...")
                .finish(),
        }
    }
}

#[derive(Error, Debug)]
pub enum TaskError {
    #[error(transparent)]
    IoError(#[from] io::Error),
}

/// The main loop task has a few responsibilities:
/// - Initiating a deCONZ device communications stream.
/// - Reacting to and/or responding to TaskMessages sent from client handles.
/// - Sending and receiving messages to the deCONZ device stream.
pub struct DeconzTask {
    config: DeconzClientConfig,
    task_rx: mpsc::UnboundedReceiver<TaskMessage>,
    next_sequence_number: u8,
    in_flight_commands: HashMap<(CommandId, u8), InFlightCommand>,
}

impl DeconzTask {
    pub fn new(config: DeconzClientConfig, task_rx: mpsc::UnboundedReceiver<TaskMessage>) -> Self {
        Self {
            config,
            task_rx,
            next_sequence_number: 0,
            in_flight_commands: Default::default(),
        }
    }

    /// Consumes the task, starting the main loop.
    pub async fn run(mut self) -> Result<(), TaskError> {
        let serial_stream = self.connect_serial()?;
        let mut deconz_stream = DeconzStream::new(serial_stream);

        loop {
            tokio::select! {
                Some(Ok(frame)) = deconz_stream.next_frame() => {
                    self.handle_deconz_frame(frame).await;

                }
                Some(task_kmessage) = self.task_rx.recv() => {
                    self.handle_task_message(task_kmessage, &mut deconz_stream).await?;
                }
            }
        }
    }

    fn connect_serial(&self) -> Result<Serial, TaskError> {
        Ok(tokio_serial::Serial::from_path(
            self.config.device_path.clone(),
            &SerialPortSettings {
                baud_rate: 38400,
                ..Default::default()
            },
        )?)
    }

    async fn handle_deconz_frame(&mut self, mut incoming_frame: DeconzFrame<Bytes>) {
        info!("incoming deconz frame {:?}", incoming_frame);

        // Unsolicited message, will handle.
        if incoming_frame.command_id() == CommandId::DeviceStateChanged {
            self.handle_device_state_changed(incoming_frame.get_u8().into())
                .await;
            return;
        }

        let key = &(
            incoming_frame.command_id(),
            incoming_frame.sequence_number(),
        );
        if let Some(in_flight_command) = self.in_flight_commands.remove(key) {
            if let Some(device_state) = (in_flight_command.response_parser)(incoming_frame) {
                self.handle_device_state_changed(device_state).await;
            }
        } else {
            info!("frame has no in-flight command handler registered, dropping!");
        }
    }

    async fn handle_device_state_changed(&mut self, device_state: DeviceState) {
        info!("deconz device state changed: {:?}", device_state);
    }

    async fn handle_task_message(
        &mut self,
        task_message: TaskMessage,
        deconz_stream: &mut DeconzStream<Serial>,
    ) -> Result<(), TaskError> {
        info!("incoming task message {:?}", task_message);

        match task_message {
            TaskMessage::CommandRequest {
                command_outgoing,
                response_parser,
            } => {
                let sequence_number = self.next_sequence_number();
                let command_id = command_outgoing.command_id();

                // todo: handle sequence id exhaustion (and queueing logic...)
                self.in_flight_commands.insert(
                    (command_id, sequence_number),
                    InFlightCommand { response_parser },
                );

                let frame = command_outgoing.into_frame(sequence_number);
                deconz_stream.write_frame(frame).await.unwrap(); // todo: Error handling!
            }
        }

        Ok(())
    }

    fn next_sequence_number(&mut self) -> u8 {
        let sequence_number = self.next_sequence_number;
        self.next_sequence_number = self.next_sequence_number.wrapping_add(1);
        sequence_number
    }
}

struct InFlightCommand {
    response_parser: Box<dyn FnOnce(DeconzFrame<Bytes>) -> Option<DeviceState> + Send>,
}
