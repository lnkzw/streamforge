//! Engine controller: owns engine state and executes typed commands.
//!
//! Independent of UI and transport. CLI, the JSON-lines server, and future UI
//! and MCP adapters all drive the engine through [`CommandHandler`].

use command_api::{
    Command, CommandError, CommandHandler, CommandList, CommandOutput, EngineStatus,
};

#[derive(Debug)]
pub struct Engine {
    status: EngineStatus,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            status: EngineStatus {
                application: "StreamForge".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                // No media graph, capture backend, or output exists yet (M2+).
                media_engine_ready: false,
                capture_available: false,
                streaming_active: false,
                recording_active: false,
            },
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandHandler for Engine {
    fn handle(&mut self, command: Command) -> Result<CommandOutput, CommandError> {
        match command {
            Command::GetStatus => Ok(CommandOutput::Status(self.status.clone())),
            Command::ListCommands => Ok(CommandOutput::Commands(CommandList::all())),
        }
    }
}

#[cfg(test)]
mod tests {
    use command_api::registry;

    use super::*;

    #[test]
    fn get_status_reports_idle_scaffold() {
        let output = Engine::new().handle(Command::GetStatus).expect("status");
        let CommandOutput::Status(status) = output else {
            panic!("unexpected output: {output:?}");
        };
        assert_eq!(status.application, "StreamForge");
        assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
        assert!(!status.media_engine_ready);
        assert!(!status.recording_active);
        assert!(!status.streaming_active);
    }

    #[test]
    fn list_commands_returns_full_registry() {
        let output = Engine::new()
            .handle(Command::ListCommands)
            .expect("commands");
        assert_eq!(
            output,
            CommandOutput::Commands(CommandList {
                commands: registry().to_vec()
            })
        );
    }

    #[test]
    fn every_registered_command_is_handled() {
        let mut engine = Engine::new();
        for descriptor in registry() {
            let command = Command::decode(descriptor.name, None).expect("decode");
            engine
                .handle(command)
                .unwrap_or_else(|err| panic!("{} failed: {err}", descriptor.name));
        }
    }
}
